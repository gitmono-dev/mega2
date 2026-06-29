use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::Arc};

use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Duration, Utc};
use futures::{StreamExt, stream};
use russh::{
    Channel, ChannelId,
    keys::{HashAlg, PublicKey},
    server::{self, Auth, Msg, Session},
};
use tokio::{io::AsyncReadExt, sync::Mutex};

use crate::{
    ceres::{
        api_service::state::ProtocolApiState,
        lfs::lfs_structs::Link,
        protocol::{
            ServiceType, SmartSession, TransportProtocol,
            smart::{self},
            v2,
        },
    },
    contract::git_protocol::{check_push_permission, check_upload_pack_access},
};

type ClientMap = HashMap<(usize, ChannelId), Channel<Msg>>;

const LFS_TRANSFER_UNSUPPORTED_ERROR: &str =
    "git-lfs-transfer is not supported; use git-lfs-authenticate HTTP fallback\n";

#[derive(Debug, PartialEq)]
enum SshExecKind {
    Git(ServiceType),
    LfsAuthenticate,
    LfsTransfer,
}

#[derive(Debug, PartialEq)]
struct SshExecRequest {
    kind: SshExecKind,
    repo_path: PathBuf,
    lfs_operation: Option<String>,
}

#[derive(Clone)]
pub struct SshServer {
    pub clients: Arc<Mutex<ClientMap>>,
    pub id: usize,
    pub channels: HashMap<ChannelId, GitSshChannelState>,
    pub v2_channels: HashMap<ChannelId, bool>,
    pub state: ProtocolApiState,
    pub authenticated_user: Option<String>,
}

#[derive(Clone)]
pub struct GitSshChannelState {
    pub smart_protocol: SmartSession,
    pub data_combined: BytesMut,
}

impl server::Server for SshServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        let s = self.clone();
        self.id += 1;
        s
    }
}

impl server::Handler for SshServer {
    type Error = anyhow::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _: &mut Session,
    ) -> Result<bool, Self::Error> {
        tracing::info!("SshServer::channel_open_session:{}", channel.id());
        {
            let mut clients = self.clients.lock().await;
            clients.insert((self.id, channel.id()), channel);
        }
        Ok(true)
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if variable_name == "GIT_PROTOCOL" && variable_value == "version=2" {
            self.v2_channels.insert(channel, true);
        }
        Ok(())
    }

    /// # Executes a request on the SSH server.
    ///
    /// This function processes the received data from the specified channel and performs the
    /// corresponding action based on the received command.
    ///
    /// Arguments:
    /// - `self`: The current instance of the SSH server.
    /// - `channel`: The channel ID on which the request was received.
    /// - `data`: The received data from the channel.
    /// - `session`: The current SSH session.
    ///
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let data = String::from_utf8_lossy(data).trim().to_owned();
        tracing::info!("exec_request, channel:{:?}, command: {}", channel, data);
        // command exmaple:
        // Push: git-receive-pack '/path/to/repo.git'
        // Pull: git-upload-pack '/path/to/repo.git'
        // LFS HTTP Authenticate: git-lfs-authenticate '/path/to/repo.git' download/upload
        let exec = match parse_ssh_exec_request(&data) {
            Ok(exec) => exec,
            Err(err) => {
                tracing::warn!(error = %err, "invalid SSH git exec request");
                session.data(channel, format!("error: {err}\n").into_bytes())?;
                session.channel_failure(channel)?;
                return Ok(());
            }
        };

        match exec.kind {
            SshExecKind::Git(service_type) => {
                let mut smart_protocol =
                    SmartSession::new(exec.repo_path, service_type, TransportProtocol::Ssh);
                if let Some(username) = self.authenticated_user.clone() {
                    smart_protocol.set_authenticated_user(username);
                }
                if service_type == ServiceType::ReceivePack {
                    check_push_permission(
                        &self.state,
                        &smart_protocol.auth,
                        &smart_protocol.repo_path,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                if service_type == ServiceType::UploadPack {
                    check_upload_pack_access(
                        &self.state.storage.config().git,
                        &smart_protocol.auth,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                }

                let is_v2 = self.v2_channels.get(&channel).copied().unwrap_or(false);
                if is_v2 && service_type == ServiceType::UploadPack {
                    let v2_adv = v2::build_v2_capability_advertisement();
                    self.channels.insert(
                        channel,
                        GitSshChannelState {
                            smart_protocol,
                            data_combined: BytesMut::new(),
                        },
                    );
                    session.data(channel, v2_adv.to_vec())?;
                    session.channel_success(channel)?;
                } else {
                    let res = smart_protocol.git_info_refs(&self.state).await?;
                    self.channels.insert(
                        channel,
                        GitSshChannelState {
                            smart_protocol,
                            data_combined: BytesMut::new(),
                        },
                    );
                    session.data(channel, res.to_vec())?;
                    session.channel_success(channel)?;
                }
            }
            //Note that currently mega does not support pure ssh to transfer files, still relay on the https server.
            //see https://github.com/git-lfs/git-lfs/blob/main/docs/proposals/ssh_adapter.md for more details about pure ssh file transfer.
            SshExecKind::LfsTransfer => {
                tracing::debug!(
                    repo_path = %exec.repo_path.display(),
                    operation = ?exec.lfs_operation,
                    "git-lfs-transfer requested"
                );
                session.extended_data(
                    channel,
                    1,
                    LFS_TRANSFER_UNSUPPORTED_ERROR.as_bytes().to_vec(),
                )?;
                session.channel_failure(channel)?;
            }
            // When connecting over SSH, the first attempt will be made to use
            // `git-lfs-transfer`, the pure SSH protocol, and if it fails, Git LFS will fall
            // back to the hybrid protocol using `git-lfs-authenticate`.
            SshExecKind::LfsAuthenticate => {
                tracing::debug!(
                    repo_path = %exec.repo_path.display(),
                    operation = ?exec.lfs_operation,
                    "git-lfs-authenticate requested"
                );
                let mut header = HashMap::new();
                let config = self.state.storage.config();
                header.insert("Accept".to_string(), "application/vnd.git-lfs".to_string());
                let link = Link {
                    href: config.lfs.ssh.http_url.clone(),
                    header,
                    expires_at: {
                        // 86400 seconds is well within the range of a `chrono::Duration`.
                        let expire_time: DateTime<Utc> = Utc::now() + Duration::seconds(86400);
                        expire_time.to_rfc3339()
                    },
                };
                let response = serde_json::to_vec(&link).map_err(anyhow::Error::from)?;
                session.data(channel, response)?;
                session.channel_success(channel)?;
            }
        }
        Ok(())
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();

        tracing::info!("auth_publickey: {} / {}", user, fingerprint);
        let res = match self
            .state
            .storage
            .user_storage()
            .search_ssh_key_finger(&fingerprint)
            .await
        {
            Ok(res) => res,
            Err(e) => {
                tracing::error!(error = %e, "SSH key DB lookup failed");
                return Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                });
            }
        };
        if !res.is_empty() {
            let username = &res[0].username;
            if res.iter().all(|m| m.username == *username) {
                tracing::info!("Client public key verified successfully!");
                self.authenticated_user = Some(username.clone());
                Ok(Auth::Accept)
            } else {
                tracing::warn!("SSH key fingerprint matches multiple distinct users; rejecting");
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
        } else {
            tracing::warn!("Client public key verification failed!");
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(state) = self.channels.get_mut(&channel) else {
            tracing::warn!(
                channel = ?channel,
                "data received before exec request initialized smart protocol"
            );
            return Ok(());
        };
        tracing::info!(
            channel = ?channel,
            "receiving data length:{}",
            data.len()
        );
        let service_type = state.smart_protocol.service_type;
        match service_type {
            ServiceType::UploadPack => {
                let api_state = &self.state;
                handle_upload_pack(state, api_state, channel, data, session).await;
            }
            ServiceType::ReceivePack => {
                state.data_combined.extend_from_slice(data);
            }
        };
        session.channel_success(channel)?;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(mut state) = self.channels.remove(&channel)
            && state.smart_protocol.service_type == ServiceType::ReceivePack
        {
            let api_state = &self.state;
            handle_receive_pack(&mut state, api_state, channel, session).await;
        }

        {
            let mut clients = self.clients.lock().await;
            clients.remove(&(self.id, channel));
        }
        session.exit_status_request(channel, 0000)?;
        session.close(channel)?;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // A channel may be closed without a preceding EOF (e.g. aborted by the
        // client). Ensure any accumulated per-channel state and the client entry
        // are dropped so receive-pack buffers cannot leak across the connection.
        self.channels.remove(&channel);
        {
            let mut clients = self.clients.lock().await;
            clients.remove(&(self.id, channel));
        }
        let _ = session.close(channel);
        Ok(())
    }
}

fn parse_ssh_exec_request(input: &str) -> Result<SshExecRequest, String> {
    let args = split_ssh_exec_args(input)?;
    let Some(command) = args.first().map(String::as_str) else {
        return Err("missing SSH exec command".to_owned());
    };

    match command {
        "git-upload-pack" | "git-receive-pack" => {
            if args.len() != 2 {
                return Err(format!("{command} requires exactly one repository path"));
            }
            let service_type = ServiceType::from_str(command).map_err(|err| err.to_string())?;
            Ok(SshExecRequest {
                kind: SshExecKind::Git(service_type),
                repo_path: normalize_ssh_repo_path(&args[1])?,
                lfs_operation: None,
            })
        }
        "git-lfs-authenticate" => {
            if args.len() != 3 {
                return Err(
                    "git-lfs-authenticate requires a repository path and upload/download operation"
                        .to_owned(),
                );
            }
            Ok(SshExecRequest {
                kind: SshExecKind::LfsAuthenticate,
                repo_path: normalize_ssh_repo_path(&args[1])?,
                lfs_operation: Some(parse_lfs_operation(&args[2])?),
            })
        }
        "git-lfs-transfer" => {
            if args.len() != 3 {
                return Err(
                    "git-lfs-transfer requires a repository path and upload/download operation"
                        .to_owned(),
                );
            }
            Ok(SshExecRequest {
                kind: SshExecKind::LfsTransfer,
                repo_path: normalize_ssh_repo_path(&args[1])?,
                lfs_operation: Some(parse_lfs_operation(&args[2])?),
            })
        }
        _ => Err(format!("unsupported SSH git command: {command}")),
    }
}

fn normalize_ssh_repo_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("repository path is empty".to_owned());
    }
    Ok(PathBuf::from(raw.strip_suffix(".git").unwrap_or(raw)))
}

fn parse_lfs_operation(raw: &str) -> Result<String, String> {
    match raw {
        "upload" | "download" => Ok(raw.to_owned()),
        _ => Err(format!(
            "unsupported Git LFS SSH operation: {raw}; expected upload or download"
        )),
    }
}

fn split_ssh_exec_args(input: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut in_arg = false;

    for ch in input.trim().chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            in_arg = true;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            in_arg = true;
            continue;
        }

        match quote {
            Some(q) if ch == q => {
                quote = None;
                in_arg = true;
            }
            Some(_) => {
                current.push(ch);
                in_arg = true;
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                in_arg = true;
            }
            None if ch.is_ascii_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            None => {
                current.push(ch);
                in_arg = true;
            }
        }
    }

    if escaped {
        return Err("dangling escape in SSH exec command".to_owned());
    }
    if quote.is_some() {
        return Err("unterminated quote in SSH exec command".to_owned());
    }
    if in_arg {
        args.push(current);
    }

    Ok(args)
}

async fn handle_upload_pack(
    state: &mut GitSshChannelState,
    api_state: &ProtocolApiState,
    channel: ChannelId,
    data: &[u8],
    session: &mut Session,
) {
    let mut body = Bytes::copy_from_slice(data);
    if v2::is_v2_upload_pack_request(&mut body) {
        handle_v2_upload_pack_ssh(state, api_state, channel, &mut body, session).await;
        return;
    }

    let smart_protocol = &mut state.smart_protocol;
    let (mut send_pack_data, buf) = match smart_protocol.git_upload_pack(api_state, &mut body).await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(error = %e, "upload-pack protocol error");
            let _ = session.data(channel, format!("error: {e}\n").into_bytes());
            return;
        }
    };

    tracing::info!("buf is {:?}", buf);
    let _ = session.data(channel, buf.to_vec());

    while let Some(chunk) = send_pack_data.next().await {
        let mut reader = chunk.as_slice();
        loop {
            let mut temp = BytesMut::new();
            temp.reserve(65500);
            let length = match reader.read_buf(&mut temp).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = %e, "read error in upload-pack stream");
                    break;
                }
            };
            if length == 0 {
                break;
            }
            let bytes_out = smart_protocol.build_side_band_format(temp, length);
            let _ = session.data(channel, bytes_out.to_vec());
        }
    }
    let _ = session.data(channel, smart::PKT_LINE_END_MARKER.to_vec());
}

async fn handle_v2_upload_pack_ssh(
    state: &mut GitSshChannelState,
    api_state: &ProtocolApiState,
    channel: ChannelId,
    body: &mut Bytes,
    session: &mut Session,
) {
    let (command, _caps) = match v2::parse_v2_command(body) {
        Ok(cmd) => cmd,
        Err(e) => {
            tracing::error!(error = %e, "v2 command parse error");
            let _ = session.data(channel, format!("error: {e}\n").into_bytes());
            return;
        }
    };

    match command.as_str() {
        "ls-refs" => {
            let refs = match v2::handle_v2_ls_refs(&state.smart_protocol, api_state, body).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "v2 ls-refs error");
                    let _ = session.data(channel, format!("error: {e}\n").into_bytes());
                    return;
                }
            };
            let _ = session.data(channel, refs.to_vec());
        }
        "fetch" => {
            let (mut send_pack_data, protocol_buf) =
                match v2::handle_v2_fetch(&mut state.smart_protocol, api_state, body).await {
                    Ok(result) => result,
                    Err(e) => {
                        tracing::error!(error = %e, "v2 fetch error");
                        let _ = session.data(channel, format!("error: {e}\n").into_bytes());
                        return;
                    }
                };

            let _ = session.data(channel, protocol_buf.to_vec());

            while let Some(chunk) = send_pack_data.next().await {
                let _ = session.data(channel, chunk);
            }
            let _ = session.data(channel, smart::PKT_LINE_END_MARKER.to_vec());
        }
        other => {
            tracing::warn!(command = %other, "unsupported v2 command");
            let _ = session.data(
                channel,
                format!("error: unsupported v2 command: {other}\n").into_bytes(),
            );
        }
    }
}

async fn handle_receive_pack(
    state: &mut GitSshChannelState,
    api_state: &ProtocolApiState,
    channel: ChannelId,
    session: &mut Session,
) {
    let smart_protocol = &mut state.smart_protocol;
    let data = state.data_combined.split().freeze();
    let (commands, pack_bytes) = match smart_protocol.split_receive_pack_request(data) {
        Ok(split) => split,
        Err(err) => {
            tracing::warn!(error = %err, "invalid receive-pack request");
            let _ = session.data(channel, format!("error: {err}\n").into_bytes());
            return;
        }
    };
    let pack_stream = stream::once(async { Ok(pack_bytes) });
    let report_status = match smart_protocol
        .git_receive_pack_stream(api_state, commands, Box::pin(pack_stream))
        .await
    {
        Ok(status) => status,
        Err(e) => {
            tracing::error!(error = %e, "receive-pack protocol error");
            let _ = session.data(channel, format!("error: {e}\n").into_bytes());
            return;
        }
    };

    tracing::info!("report status: {:?}", report_status);
    let _ = session.data(channel, report_status.to_vec());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_exec_accepts_quoted_repo_path_with_spaces() {
        let parsed = parse_ssh_exec_request("git-upload-pack '/srv/git/project repo.git'").unwrap();

        assert_eq!(parsed.kind, SshExecKind::Git(ServiceType::UploadPack));
        assert_eq!(parsed.repo_path, PathBuf::from("/srv/git/project repo"));
        assert_eq!(parsed.lfs_operation, None);
    }

    #[test]
    fn parse_git_exec_strips_only_trailing_git_suffix() {
        let parsed = parse_ssh_exec_request("git-receive-pack '/srv/git.git/project.git'").unwrap();

        assert_eq!(parsed.kind, SshExecKind::Git(ServiceType::ReceivePack));
        assert_eq!(parsed.repo_path, PathBuf::from("/srv/git.git/project"));
    }

    #[test]
    fn parse_lfs_authenticate_keeps_operation() {
        let parsed =
            parse_ssh_exec_request("git-lfs-authenticate \"/srv/git/project.git\" download")
                .unwrap();

        assert_eq!(parsed.kind, SshExecKind::LfsAuthenticate);
        assert_eq!(parsed.repo_path, PathBuf::from("/srv/git/project"));
        assert_eq!(parsed.lfs_operation.as_deref(), Some("download"));
    }

    #[test]
    fn parse_lfs_exec_rejects_missing_or_unknown_operation() {
        let missing =
            parse_ssh_exec_request("git-lfs-authenticate \"/srv/git/project.git\"").unwrap_err();
        assert!(missing.contains("upload/download operation"));

        let unknown =
            parse_ssh_exec_request("git-lfs-transfer \"/srv/git/project.git\" verify").unwrap_err();
        assert!(unknown.contains("expected upload or download"));
    }

    #[test]
    fn lfs_transfer_unsupported_error_is_stderr_friendly() {
        assert!(LFS_TRANSFER_UNSUPPORTED_ERROR.starts_with("git-lfs-transfer is not supported"));
        assert!(LFS_TRANSFER_UNSUPPORTED_ERROR.ends_with('\n'));
    }

    #[test]
    fn parse_git_exec_rejects_missing_path() {
        let err = parse_ssh_exec_request("git-upload-pack").unwrap_err();

        assert!(err.contains("requires exactly one repository path"));
    }

    #[test]
    fn parse_git_exec_rejects_unsupported_command_without_fallback() {
        let err = parse_ssh_exec_request("git-upload-archive '/srv/git/project.git'").unwrap_err();

        assert!(err.contains("unsupported SSH git command"));
    }

    #[test]
    fn parse_git_exec_rejects_unterminated_quote() {
        let err = parse_ssh_exec_request("git-upload-pack '/srv/git/project.git").unwrap_err();

        assert!(err.contains("unterminated quote"));
    }

    #[test]
    fn per_channel_receive_pack_buffers_are_isolated() {
        let mut channel_a = GitSshChannelState {
            smart_protocol: SmartSession::new(
                PathBuf::from("/a"),
                ServiceType::ReceivePack,
                TransportProtocol::Ssh,
            ),
            data_combined: BytesMut::new(),
        };
        let mut channel_b = GitSshChannelState {
            smart_protocol: SmartSession::new(
                PathBuf::from("/b"),
                ServiceType::ReceivePack,
                TransportProtocol::Ssh,
            ),
            data_combined: BytesMut::new(),
        };

        channel_a.data_combined.extend_from_slice(b"payload-a");
        channel_b.data_combined.extend_from_slice(b"payload-b");

        assert_eq!(&channel_a.data_combined[..], b"payload-a");
        assert_eq!(&channel_b.data_combined[..], b"payload-b");
    }
}
