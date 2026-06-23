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

use crate::ceres::{
    api_service::state::ProtocolApiState,
    lfs::lfs_structs::Link,
    protocol::{
        ServiceType, SmartSession, TransportProtocol,
        smart::{self},
    },
};

type ClientMap = HashMap<(usize, ChannelId), Channel<Msg>>;

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
    pub smart_protocol: Option<SmartSession>,
    pub state: ProtocolApiState,
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
                let smart_protocol =
                    SmartSession::new(exec.repo_path, service_type, TransportProtocol::Ssh);
                // TODO handler ProtocolError
                let res = smart_protocol.git_info_refs(&self.state).await?;
                self.smart_protocol = Some(smart_protocol);
                session.data(channel, res.to_vec())?;
                session.channel_success(channel)?;
            }
            //Note that currently mega does not support pure ssh to transfer files, still relay on the https server.
            //see https://github.com/git-lfs/git-lfs/blob/main/docs/proposals/ssh_adapter.md for more details about pure ssh file transfer.
            SshExecKind::LfsTransfer => {
                tracing::debug!(
                    repo_path = %exec.repo_path.display(),
                    operation = ?exec.lfs_operation,
                    "git-lfs-transfer requested"
                );
                session.data(channel, "not implemented yet".as_bytes().to_vec())?;
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
                        let expire_time: DateTime<Utc> =
                            Utc::now() + Duration::try_seconds(86400).unwrap();
                        expire_time.to_rfc3339()
                    },
                };
                session.data(channel, serde_json::to_vec(&link).unwrap())?;
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
        let res = self
            .state
            .storage
            .user_storage()
            .search_ssh_key_finger(&fingerprint)
            .await
            .unwrap();
        if !res.is_empty() {
            tracing::info!("Client public key verified successfully!");
            Ok(Auth::Accept)
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
        let smart_protocol = self.smart_protocol.as_mut().unwrap();
        tracing::info!(
            "receiving data length:{}",
            // String::from_utf8_lossy(data),
            data.len()
        );
        let service_type = smart_protocol.service_type;
        match service_type {
            ServiceType::UploadPack => {
                self.handle_upload_pack(channel, data, session).await;
            }
            ServiceType::ReceivePack => {
                self.data_combined.extend_from_slice(data);
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
        if let Some(smart_protocol) = self.smart_protocol.as_mut()
            && smart_protocol.service_type == ServiceType::ReceivePack
        {
            self.handle_receive_pack(channel, session).await;
        };

        {
            let mut clients = self.clients.lock().await;
            clients.remove(&(self.id, channel));
        }
        session.exit_status_request(channel, 0000)?;
        session.close(channel)?;
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
            if !(2..=3).contains(&args.len()) {
                return Err(
                    "git-lfs-authenticate requires a repository path and optional operation"
                        .to_owned(),
                );
            }
            Ok(SshExecRequest {
                kind: SshExecKind::LfsAuthenticate,
                repo_path: normalize_ssh_repo_path(&args[1])?,
                lfs_operation: args.get(2).cloned(),
            })
        }
        "git-lfs-transfer" => {
            if !(2..=3).contains(&args.len()) {
                return Err(
                    "git-lfs-transfer requires a repository path and optional operation".to_owned(),
                );
            }
            Ok(SshExecRequest {
                kind: SshExecKind::LfsTransfer,
                repo_path: normalize_ssh_repo_path(&args[1])?,
                lfs_operation: args.get(2).cloned(),
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

impl SshServer {
    async fn handle_upload_pack(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) {
        let smart_protocol = self.smart_protocol.as_mut().unwrap();

        let (mut send_pack_data, buf) = smart_protocol
            .git_upload_pack(&self.state, &mut Bytes::copy_from_slice(data))
            .await
            .unwrap();

        tracing::info!("buf is {:?}", buf);
        session
            .data(channel, String::from_utf8(buf.to_vec()).unwrap())
            .unwrap();

        while let Some(chunk) = send_pack_data.next().await {
            let mut reader = chunk.as_slice();
            loop {
                let mut temp = BytesMut::new();
                temp.reserve(65500);
                let length = reader.read_buf(&mut temp).await.unwrap();
                if length == 0 {
                    break;
                }
                let bytes_out = smart_protocol.build_side_band_format(temp, length);
                session.data(channel, bytes_out.to_vec()).unwrap();
            }
        }
        session
            .data(channel, smart::PKT_LINE_END_MARKER.to_vec())
            .unwrap();
    }

    async fn handle_receive_pack(&mut self, channel: ChannelId, session: &mut Session) {
        let smart_protocol = self.smart_protocol.as_mut().unwrap();
        let data = self.data_combined.split().freeze();
        let (commands, pack_bytes) = match smart_protocol.split_receive_pack_request(data) {
            Ok(split) => split,
            Err(err) => {
                tracing::warn!(error = %err, "invalid receive-pack request");
                let _ = session.data(channel, format!("error: {err}\n").into_bytes());
                return;
            }
        };
        let pack_stream = stream::once(async { Ok(pack_bytes) });
        let report_status = smart_protocol
            .git_receive_pack_stream(&self.state, commands, Box::pin(pack_stream))
            .await
            .unwrap();

        tracing::info!("report status: {:?}", report_status);
        session.data(channel, report_status.to_vec()).unwrap();
    }
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
}
