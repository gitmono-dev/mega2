use std::{collections::HashMap, hash::Hash, path::PathBuf, str::FromStr, sync::Arc};

use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use russh::{
    Channel, ChannelId,
    keys::{HashAlg, PublicKey},
    server::{self, Auth, ChannelOpenHandle, Msg, Session},
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
    common::errors::ProtocolError,
    config::PushAuth,
    contract::git_protocol::{
        check_push_permission, check_upload_pack_access, lookup_push_token,
        path::{RepoLocator, classify_repo_locator},
        resolve_view_target,
    },
    jupiter::storage::Storage,
};

type ClientMap = HashMap<(usize, ChannelId), Channel<Msg>>;

const LFS_TRANSFER_UNSUPPORTED_ERROR: &str =
    "git-lfs-transfer is not supported; use git-lfs-authenticate HTTP fallback\n";

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshErrReply {
    payload: Bytes,
    exit_code: u32,
}

impl SshErrReply {
    fn new(message: impl std::fmt::Display, exit_code: u32) -> Self {
        let mut payload = BytesMut::new();
        smart::add_pkt_line_string(&mut payload, format!("ERR {message}\n"));
        Self {
            payload: payload.freeze(),
            exit_code,
        }
    }

    fn view_not_found() -> Self {
        Self::new("view not found", 1)
    }

    fn view_read_only() -> Self {
        Self::new("view URLs are read-only", 1)
    }

    fn from_view_error(error: &ProtocolError) -> Option<Self> {
        match error {
            ProtocolError::ViewUnavailable { .. } => Some(Self::new(error, 75)),
            ProtocolError::PackRejected(_) => Some(Self::new(error, 1)),
            _ => None,
        }
    }
}

trait SshChannelOut {
    fn channel_success(&mut self) -> Result<(), russh::Error>;
    fn data(&mut self, data: Bytes) -> Result<(), russh::Error>;
    fn exit_status_request(&mut self, exit_code: u32) -> Result<(), russh::Error>;
    fn eof(&mut self) -> Result<(), russh::Error>;
    fn close(&mut self) -> Result<(), russh::Error>;
}

struct SessionChannelOut<'a> {
    channel: ChannelId,
    session: &'a mut Session,
}

impl SshChannelOut for SessionChannelOut<'_> {
    fn channel_success(&mut self) -> Result<(), russh::Error> {
        self.session.channel_success(self.channel)
    }

    fn data(&mut self, data: Bytes) -> Result<(), russh::Error> {
        self.session.data(self.channel, data)
    }

    fn exit_status_request(&mut self, exit_code: u32) -> Result<(), russh::Error> {
        self.session.exit_status_request(self.channel, exit_code)
    }

    fn eof(&mut self) -> Result<(), russh::Error> {
        self.session.eof(self.channel)
    }

    fn close(&mut self) -> Result<(), russh::Error> {
        self.session.close(self.channel)
    }
}

#[derive(Debug, PartialEq)]
enum V0Exec {
    Advertise(BytesMut),
    ViewError(SshErrReply),
}

fn classify_v0_advertise(result: Result<BytesMut, ProtocolError>) -> Result<V0Exec, ProtocolError> {
    match result {
        Ok(advertise) => Ok(V0Exec::Advertise(advertise)),
        Err(error) => match SshErrReply::from_view_error(&error) {
            Some(reply) => Ok(V0Exec::ViewError(reply)),
            None => Err(error),
        },
    }
}

fn reject_exec<K, C, O>(
    clients: &mut HashMap<(usize, K), C>,
    v2_channels: &mut HashMap<K, bool>,
    client_key: (usize, K),
    reply: &SshErrReply,
    out: &mut O,
) -> Result<(), russh::Error>
where
    K: Eq + Hash + Copy,
    O: SshChannelOut,
{
    clients.remove(&client_key);
    v2_channels.remove(&client_key.1);
    out.channel_success()?;
    out.data(reply.payload.clone())?;
    out.exit_status_request(reply.exit_code)?;
    out.eof()?;
    out.close()
}

fn finish_v0_exec<K, C, O>(
    channels: &mut HashMap<K, GitSshChannelState>,
    clients: &mut HashMap<(usize, K), C>,
    v2_channels: &mut HashMap<K, bool>,
    client_key: (usize, K),
    smart_protocol: SmartSession,
    outcome: V0Exec,
    out: &mut O,
) -> Result<(), russh::Error>
where
    K: Eq + Hash + Copy,
    O: SshChannelOut,
{
    match outcome {
        V0Exec::Advertise(advertise) => {
            channels.insert(
                client_key.1,
                GitSshChannelState {
                    smart_protocol,
                    data_combined: BytesMut::new(),
                    pending_shallow_request: None,
                    exit_code: None,
                },
            );
            out.data(advertise.freeze())?;
            out.channel_success()
        }
        V0Exec::ViewError(reply) => reject_exec(clients, v2_channels, client_key, &reply, out),
    }
}

fn write_data_error<O: SshChannelOut>(
    out: &mut O,
    state: &mut GitSshChannelState,
    error: &ProtocolError,
) {
    if state.exit_code.is_some() {
        return;
    }
    if let Some(reply) = SshErrReply::from_view_error(error) {
        let _ = out.data(reply.payload);
        state.exit_code = Some(reply.exit_code);
    } else {
        let _ = out.data(Bytes::from(format!("error: {error}\n")));
    }
}

fn take_ready_request(state: Option<&mut GitSshChannelState>, data: &[u8]) -> Option<Bytes> {
    let state = state?;
    if state.exit_code.is_some() {
        return None;
    }
    match state.smart_protocol.service_type {
        ServiceType::ReceivePack => {
            state.data_combined.extend_from_slice(data);
            None
        }
        ServiceType::UploadPack => {
            // Git may deliver the upload-pack request across multiple SSH
            // data packets. Process only a complete round, terminated
            // by a flush pkt-line (`0000`) or a complete `done` packet.
            // Handling a partial/flush-only chunk as a complete request
            // previously ran pack generation with want=[] and returned
            // `error: …` (`bad line length character: erro` on the client).
            state.data_combined.extend_from_slice(data);
            if upload_pack_buffer_complete(
                &state.data_combined,
                state.pending_shallow_request.is_some(),
            ) {
                Some(take_complete_upload_pack_request(&mut state.data_combined))
            } else {
                None
            }
        }
    }
}

fn take_eof_residual(state: &mut GitSshChannelState) -> Option<Bytes> {
    if state.exit_code.is_some() || state.data_combined.is_empty() {
        return None;
    }
    Some(state.data_combined.split().freeze())
}

fn eof_exit_code(state: Option<&GitSshChannelState>, had_client_entry: bool) -> Option<u32> {
    if !had_client_entry {
        return None;
    }
    Some(state.and_then(|state| state.exit_code).unwrap_or(0))
}

#[derive(Debug, PartialEq)]
enum SshExecKind {
    Git(ServiceType),
    LfsAuthenticate,
    LfsTransfer,
}

#[derive(Debug)]
struct SshExecRequest {
    kind: SshExecKind,
    repo_path: PathBuf,
    locator: Result<RepoLocator, ProtocolError>,
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
    /// Wants/depth from a v0 shallow-info round; SSH sends only `done` next.
    pub pending_shallow_request: Option<Bytes>,
    pub exit_code: Option<u32>,
}

impl server::Server for SshServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        let s = self.clone();
        self.id += 1;
        s
    }
}

impl SshServer {
    async fn send_exec_err(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        reply: &SshErrReply,
    ) -> Result<(), russh::Error> {
        let mut clients = self.clients.lock().await;
        let mut out = SessionChannelOut { channel, session };
        reject_exec(
            &mut clients,
            &mut self.v2_channels,
            (self.id, channel),
            reply,
            &mut out,
        )
    }
}

impl server::Handler for SshServer {
    type Error = anyhow::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        tracing::info!("SshServer::channel_open_session:{}", channel.id());
        {
            let mut clients = self.clients.lock().await;
            clients.insert((self.id, channel.id()), channel);
        }
        reply.accept().await;
        Ok(())
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

        match &exec.locator {
            Err(_) => {
                self.send_exec_err(channel, session, &SshErrReply::view_not_found())
                    .await?;
                return Ok(());
            }
            Ok(RepoLocator::View(_)) => {
                let reply = if matches!(
                    &exec.kind,
                    SshExecKind::LfsAuthenticate | SshExecKind::LfsTransfer
                ) || !self.state.storage.config().views.enabled
                {
                    Some(SshErrReply::view_not_found())
                } else if matches!(&exec.kind, SshExecKind::Git(ServiceType::ReceivePack)) {
                    Some(SshErrReply::view_read_only())
                } else {
                    None
                };
                if let Some(reply) = reply {
                    self.send_exec_err(channel, session, &reply).await?;
                    return Ok(());
                }
            }
            Ok(RepoLocator::Path(_)) => {}
        }

        match exec.kind {
            SshExecKind::Git(service_type) => {
                let mut smart_protocol = SmartSession::from_state(
                    exec.repo_path,
                    service_type,
                    TransportProtocol::Ssh,
                    &self.state,
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
                if let Some(username) = self.authenticated_user.clone() {
                    smart_protocol.set_authenticated_user(username);
                }
                if service_type == ServiceType::ReceivePack {
                    if !self.state.storage.config().git.ssh_receive_pack_enabled() {
                        // Accept the exec so OpenSSH surfaces the error on the
                        // git channel; CHANNEL_FAILURE only yields "exec request
                        // failed" and drops the payload.
                        session.channel_success(channel)?;
                        session.extended_data(
                            channel,
                            1,
                            b"error: SSH receive-pack is disabled\n".to_vec(),
                        )?;
                        session.eof(channel)?;
                        session.close(channel)?;
                        return Ok(());
                    }
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
                    if let Ok(RepoLocator::View(locator)) = &exec.locator {
                        match resolve_view_target(&self.state, locator).await {
                            Ok(view) => smart_protocol.view = Some(view),
                            Err(error) => {
                                let reply = match &error {
                                    ProtocolError::NotFound(_) => SshErrReply::view_not_found(),
                                    ProtocolError::ViewUnavailable { .. } => {
                                        SshErrReply::new(&error, 75)
                                    }
                                    _ => {
                                        tracing::error!(error = %error, "view resolution failed");
                                        SshErrReply::new("view unavailable", 75)
                                    }
                                };
                                self.send_exec_err(channel, session, &reply).await?;
                                return Ok(());
                            }
                        }
                    }
                }

                let is_v2 = self.v2_channels.get(&channel).copied().unwrap_or(false);
                if is_v2 && service_type == ServiceType::UploadPack {
                    let v2_adv = v2::build_v2_capability_advertisement(smart_protocol.hash_kind);
                    self.channels.insert(
                        channel,
                        GitSshChannelState {
                            smart_protocol,
                            data_combined: BytesMut::new(),
                            pending_shallow_request: None,
                            exit_code: None,
                        },
                    );
                    session.data(channel, v2_adv.to_vec())?;
                    session.channel_success(channel)?;
                } else {
                    let outcome =
                        classify_v0_advertise(smart_protocol.git_info_refs(&self.state).await)?;
                    let mut clients = self.clients.lock().await;
                    let mut out = SessionChannelOut { channel, session };
                    finish_v0_exec(
                        &mut self.channels,
                        &mut clients,
                        &mut self.v2_channels,
                        (self.id, channel),
                        smart_protocol,
                        outcome,
                        &mut out,
                    )?;
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

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        let git = &self.state.storage.config().git;
        let accept = git.storage_only() && git.anonymous_access;
        tracing::info!(
            user,
            storage_only = git.storage_only(),
            anonymous_access = git.anonymous_access,
            accept,
            "auth_none"
        );
        if accept {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        let git = &self.state.storage.config().git;
        if git.push_auth != Some(PushAuth::Token) {
            return Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            });
        }
        match lookup_push_token(&git.push_tokens, password) {
            Some(token) => {
                tracing::info!(token_name = %token.name, "auth_password token hit");
                self.authenticated_user = Some(token.name.clone());
                Ok(Auth::Accept)
            }
            None => {
                tracing::info!(
                    n_tokens = git.push_tokens.len(),
                    presented_len = password.len(),
                    "auth_password token miss"
                );
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        if self.state.storage.config().git.storage_only() {
            return Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            });
        }

        let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();

        tracing::info!("auth_publickey: {} / {}", user, fingerprint);
        let res = match lookup_ssh_key_finger_for_review(&self.state.storage, &fingerprint).await {
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
        let mut out = SessionChannelOut { channel, session };
        on_channel_data(&mut self.channels, channel, &self.state, data, &mut out).await?;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let had_client_entry = {
            let mut clients = self.clients.lock().await;
            clients.remove(&(self.id, channel)).is_some()
        };
        let mut out = SessionChannelOut { channel, session };
        finish_channel_eof(
            &mut self.channels,
            channel,
            had_client_entry,
            &self.state,
            &mut out,
        )
        .await?;
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
            let repo_path = normalize_ssh_repo_path(&args[1])?;
            Ok(SshExecRequest {
                kind: SshExecKind::Git(service_type),
                repo_path,
                locator: classify_repo_locator(&args[1]),
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
            let repo_path = normalize_ssh_repo_path(&args[1])?;
            let locator = classify_repo_locator(&args[1]);
            let lfs_operation = if matches!(&locator, Ok(RepoLocator::Path(_))) {
                Some(parse_lfs_operation(&args[2])?)
            } else {
                None
            };
            Ok(SshExecRequest {
                kind: SshExecKind::LfsAuthenticate,
                repo_path,
                locator,
                lfs_operation,
            })
        }
        "git-lfs-transfer" => {
            if args.len() != 3 {
                return Err(
                    "git-lfs-transfer requires a repository path and upload/download operation"
                        .to_owned(),
                );
            }
            let repo_path = normalize_ssh_repo_path(&args[1])?;
            let locator = classify_repo_locator(&args[1]);
            let lfs_operation = if matches!(&locator, Ok(RepoLocator::Path(_))) {
                Some(parse_lfs_operation(&args[2])?)
            } else {
                None
            };
            Ok(SshExecRequest {
                kind: SshExecKind::LfsTransfer,
                repo_path,
                locator,
                lfs_operation,
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

async fn on_channel_data<K, O>(
    channels: &mut HashMap<K, GitSshChannelState>,
    key: K,
    api_state: &ProtocolApiState,
    data: &[u8],
    out: &mut O,
) -> Result<(), russh::Error>
where
    K: Eq + Hash + std::fmt::Debug,
    O: SshChannelOut + Send,
{
    let Some(state) = channels.get_mut(&key) else {
        tracing::warn!(
            channel = ?key,
            "data received before exec request initialized smart protocol"
        );
        return Ok(());
    };
    tracing::info!(channel = ?key, "receiving data length:{}", data.len());
    if let Some(request) = take_ready_request(Some(state), data) {
        handle_upload_pack(state, api_state, &request, out).await;
    }
    out.channel_success()
}

async fn finish_channel_eof<K, O>(
    channels: &mut HashMap<K, GitSshChannelState>,
    key: K,
    had_client_entry: bool,
    api_state: &ProtocolApiState,
    out: &mut O,
) -> Result<(), russh::Error>
where
    K: Eq + Hash,
    O: SshChannelOut + Send,
{
    let mut state = channels.remove(&key);
    if let Some(state) = state.as_mut() {
        match state.smart_protocol.service_type {
            ServiceType::ReceivePack => handle_receive_pack(state, api_state, out).await,
            ServiceType::UploadPack => {
                if let Some(request) = take_eof_residual(state) {
                    handle_upload_pack(state, api_state, &request, out).await;
                }
            }
        }
    }
    if let Some(exit_code) = eof_exit_code(state.as_ref(), had_client_entry) {
        out.exit_status_request(exit_code)?;
    }
    out.close()
}

async fn handle_upload_pack<O: SshChannelOut + Send>(
    state: &mut GitSshChannelState,
    api_state: &ProtocolApiState,
    data: &[u8],
    out: &mut O,
) {
    let mut body = Bytes::copy_from_slice(data);
    if v2::is_v2_upload_pack_request(&mut body) {
        handle_v2_upload_pack_ssh(state, api_state, &mut body, out).await;
        return;
    }

    let previous = state.pending_shallow_request.take();
    let shallow_info_sent = previous.is_some();
    let mut request = BytesMut::new();
    if let Some(previous) = previous {
        request.extend_from_slice(&previous);
    }
    request.extend_from_slice(data);
    let request = request.freeze();
    body = request.clone();
    let smart_protocol = &mut state.smart_protocol;
    let (mut send_pack_data, buf) = match smart_protocol
        .git_upload_pack_with_shallow_info(api_state, &mut body, shallow_info_sent)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(error = %e, "upload-pack protocol error");
            write_data_error(out, state, &e);
            return;
        }
    };

    tracing::info!("buf is {:?}", buf);
    let _ = out.data(buf.clone().freeze());
    if buf.ends_with(smart::PKT_LINE_END_MARKER) {
        state.pending_shallow_request = Some(request);
        return;
    }

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
            let _ = out.data(bytes_out.freeze());
        }
    }
    let _ = out.data(Bytes::copy_from_slice(smart::PKT_LINE_END_MARKER));
}

/// True when `buf` holds at least one terminated upload-pack/v2 command.
///
/// A flush-only (`0000`) or empty buffer is *not* complete. A done-only
/// continuation is complete when the channel already holds the wants/depth.
fn upload_pack_buffer_complete(buf: &[u8], shallow_negotiation_pending: bool) -> bool {
    if buf.len() < 8 {
        return false;
    }
    let text = String::from_utf8_lossy(buf);
    let has_done = text.contains("0009done\n");
    let has_payload = text.contains("want ")
        || text.contains("command=")
        || text.contains("have ")
        || (shallow_negotiation_pending && has_done);
    let terminated = buf.ends_with(smart::PKT_LINE_END_MARKER) || has_done;
    has_payload && terminated
}

fn take_complete_upload_pack_request(buf: &mut BytesMut) -> Bytes {
    buf.split().freeze()
}

async fn handle_v2_upload_pack_ssh<O: SshChannelOut + Send>(
    state: &mut GitSshChannelState,
    api_state: &ProtocolApiState,
    body: &mut Bytes,
    out: &mut O,
) {
    let (command, _caps) = match v2::parse_v2_command(body) {
        Ok(command) => command,
        Err(e) => {
            tracing::error!(error = %e, "v2 command parse error");
            let _ = out.data(Bytes::from(format!("error: {e}\n")));
            return;
        }
    };

    match command.as_str() {
        "ls-refs" => {
            let refs = match v2::handle_v2_ls_refs(&state.smart_protocol, api_state, body).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "v2 ls-refs error");
                    write_data_error(out, state, &e);
                    return;
                }
            };
            let _ = out.data(refs.freeze());
        }
        "fetch" => {
            let v2::V2FetchResponse {
                pack_data: mut send_pack_data,
                protocol_buf,
                has_packfile,
            } = match v2::handle_v2_fetch(&mut state.smart_protocol, api_state, body).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::error!(error = %e, "v2 fetch error");
                    write_data_error(out, state, &e);
                    return;
                }
            };

            let mut protocol_buf = protocol_buf;
            if !has_packfile {
                // Negotiation round: the `acknowledgments` section already
                // closed the response with its flush packet.
                let _ = out.data(protocol_buf.freeze());
                return;
            }
            v2::add_packfile_section_header(&mut protocol_buf);
            let _ = out.data(protocol_buf.freeze());

            while let Some(chunk) = send_pack_data.next().await {
                let mut reader = chunk.as_slice();
                loop {
                    let mut temp = BytesMut::new();
                    temp.reserve(65500);
                    let length = match reader.read_buf(&mut temp).await {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::error!(error = %e, "read error in v2 upload-pack stream");
                            break;
                        }
                    };
                    if length == 0 {
                        break;
                    }
                    let bytes_out = v2::build_packfile_data_packet(temp, length);
                    let _ = out.data(bytes_out.freeze());
                }
            }
            let _ = out.data(Bytes::copy_from_slice(smart::PKT_LINE_END_MARKER));
        }
        other => {
            tracing::warn!(command = %other, "unsupported v2 command");
            let _ = out.data(Bytes::from(format!(
                "error: unsupported v2 command: {other}\n"
            )));
        }
    }
}

async fn handle_receive_pack<O: SshChannelOut + Send>(
    state: &mut GitSshChannelState,
    api_state: &ProtocolApiState,
    out: &mut O,
) {
    let smart_protocol = &mut state.smart_protocol;
    let data = state.data_combined.split().freeze();
    let (commands, pack_bytes) = match smart_protocol.split_receive_pack_request(data) {
        Ok(split) => split,
        Err(err) => {
            tracing::warn!(error = %err, "invalid receive-pack request");
            let _ = out.data(Bytes::from(format!("error: {err}\n")));
            return;
        }
    };
    let report_status = match smart_protocol
        .git_receive_pack_stream(api_state, commands, pack_bytes)
        .await
    {
        Ok(status) => status,
        Err(err) => {
            tracing::error!(error = %err, "receive-pack protocol error");
            let _ = out.data(Bytes::from(format!("error: {err}\n")));
            return;
        }
    };

    tracing::info!("report status: {:?}", report_status);
    let _ = out.data(report_status);
}

async fn lookup_ssh_key_finger_for_review(
    storage: &Storage,
    fingerprint: &str,
) -> Result<Vec<crate::callisto::ssh_keys::Model>, crate::common::errors::MegaError> {
    storage
        .user_storage()
        .search_ssh_key_finger(fingerprint)
        .await
}

#[cfg(test)]
mod tests {
    use russh::server::Handler;

    use super::*;
    use crate::{
        ceres::api_service::{cache::GitObjectCache, state::ProtocolApiState},
        config::{GitConfig, PushAuth, testing::isolated_config},
        contract::policy::entitystore::SharedEntityStore,
        jupiter::tests::test_storage_with_config,
    };

    fn sample_public_key() -> PublicKey {
        PublicKey::from_openssh(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqvkVqjzI9K4TDbjKjktkiHFdBzxv88ZUFl/XtNwF",
        )
        .expect("fixture public key")
    }

    async fn ssh_server_with_git(temp_dir: &std::path::Path, git: GitConfig) -> SshServer {
        let mut config = isolated_config(temp_dir.join("cfg"));
        config.git = git;
        let storage = test_storage_with_config(temp_dir, config).await;
        let redis_url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        SshServer {
            clients: Arc::new(Mutex::new(HashMap::new())),
            state: ProtocolApiState {
                storage,
                git_object_cache: Arc::new(GitObjectCache {
                    connection: crate::jupiter::redis::init_connection(
                        &crate::config::RedisConfig { url: redis_url },
                    )
                    .await
                    .expect("redis connection"),
                    prefix: "disabled".to_string(),
                }),
                entity_store: Arc::new(SharedEntityStore::new()),
            },
            id: 0,
            channels: HashMap::new(),
            v2_channels: HashMap::new(),
            authenticated_user: None,
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum RecordedEvent {
        ChannelSuccess,
        Data(Bytes),
        ExitStatus(u32),
        Eof,
        Close,
    }

    #[derive(Default)]
    struct RecordedOut {
        events: Vec<RecordedEvent>,
    }

    impl SshChannelOut for RecordedOut {
        fn channel_success(&mut self) -> Result<(), russh::Error> {
            self.events.push(RecordedEvent::ChannelSuccess);
            Ok(())
        }

        fn data(&mut self, data: Bytes) -> Result<(), russh::Error> {
            self.events.push(RecordedEvent::Data(data));
            Ok(())
        }

        fn exit_status_request(&mut self, exit_code: u32) -> Result<(), russh::Error> {
            self.events.push(RecordedEvent::ExitStatus(exit_code));
            Ok(())
        }

        fn eof(&mut self) -> Result<(), russh::Error> {
            self.events.push(RecordedEvent::Eof);
            Ok(())
        }

        fn close(&mut self) -> Result<(), russh::Error> {
            self.events.push(RecordedEvent::Close);
            Ok(())
        }
    }

    fn upload_pack_state() -> GitSshChannelState {
        GitSshChannelState {
            smart_protocol: SmartSession::new(
                PathBuf::from("/"),
                ServiceType::UploadPack,
                TransportProtocol::Ssh,
            ),
            data_combined: BytesMut::new(),
            pending_shallow_request: None,
            exit_code: None,
        }
    }

    fn expected_err_packet(message: &str) -> Bytes {
        let mut packet = BytesMut::new();
        smart::add_pkt_line_string(&mut packet, format!("ERR {message}\n"));
        packet.freeze()
    }

    #[test]
    fn ssh_err_reply_matches_contract() {
        for (reply, message) in [
            (SshErrReply::view_not_found(), "view not found"),
            (SshErrReply::view_read_only(), "view URLs are read-only"),
        ] {
            assert_eq!(reply.payload, expected_err_packet(message));
            assert_eq!(reply.exit_code, 1);
            let mut payload = reply.payload.clone();
            assert_eq!(
                smart::try_read_pkt_line(&mut payload).expect("ERR packet"),
                smart::PktLine::Data(Bytes::from(format!("ERR {message}\n")))
            );
            assert!(payload.is_empty());
        }

        let filter_id = "a".repeat(64);
        for (reason, expected_reason) in [
            (
                crate::common::errors::ViewUnavailableReason::WarmingUp,
                "warming up",
            ),
            (
                crate::common::errors::ViewUnavailableReason::RootChainHalted,
                "root chain halted",
            ),
            (
                crate::common::errors::ViewUnavailableReason::DefinitionCorrupt,
                "definition corrupt",
            ),
        ] {
            let error = ProtocolError::ViewUnavailable {
                filter_id: filter_id.clone(),
                reason,
            };
            let reply = SshErrReply::from_view_error(&error).expect("view unavailable reply");
            let expected = format!("view {filter_id} unavailable: {expected_reason}");
            assert_eq!(reply.payload, expected_err_packet(&expected));
            assert_eq!(reply.exit_code, 75);
            let mut payload = reply.payload.clone();
            assert_eq!(
                smart::try_read_pkt_line(&mut payload).expect("ERR packet"),
                smart::PktLine::Data(Bytes::from(format!("ERR {expected}\n")))
            );
            assert!(payload.is_empty());
        }

        for message in [
            format!("upload-pack: not our ref {}", "b".repeat(40)),
            format!(
                "view {} pack aborted: tree {} does not match its stored entries",
                "c".repeat(64),
                "d".repeat(40),
            ),
        ] {
            let error = ProtocolError::PackRejected(message.clone());
            let reply = SshErrReply::from_view_error(&error).expect("pack rejected reply");
            assert_eq!(reply.payload, expected_err_packet(&message));
            assert_eq!(reply.exit_code, 1);
            let mut payload = reply.payload.clone();
            assert_eq!(
                smart::try_read_pkt_line(&mut payload).expect("ERR packet"),
                smart::PktLine::Data(Bytes::from(format!("ERR {message}\n")))
            );
            assert!(payload.is_empty());
        }

        for error in [
            ProtocolError::NotFound("missing".to_owned()),
            ProtocolError::Forbidden("denied".to_owned()),
            ProtocolError::InvalidInput("bad input".to_owned()),
            ProtocolError::AdvertiseFailed,
        ] {
            assert!(SshErrReply::from_view_error(&error).is_none());
        }
    }

    #[test]
    fn exec_view_error_sequence() {
        let filter_id = "e".repeat(64);
        for (error, exit_code) in [
            (
                ProtocolError::ViewUnavailable {
                    filter_id: filter_id.clone(),
                    reason: crate::common::errors::ViewUnavailableReason::WarmingUp,
                },
                75,
            ),
            (
                ProtocolError::PackRejected(format!("upload-pack: not our ref {}", "f".repeat(40))),
                1,
            ),
        ] {
            let outcome = classify_v0_advertise(Err(error)).expect("view error outcome");
            assert!(matches!(outcome, V0Exec::ViewError(_)));
            let mut channels = HashMap::<u32, GitSshChannelState>::new();
            let mut clients = HashMap::from([((3_usize, 7_u32), ()), ((3, 8), ()), ((4, 7), ())]);
            let mut v2_channels = HashMap::from([(7_u32, true), (8_u32, true)]);
            let mut out = RecordedOut::default();
            finish_v0_exec(
                &mut channels,
                &mut clients,
                &mut v2_channels,
                (3, 7),
                SmartSession::new(
                    PathBuf::from("/"),
                    ServiceType::UploadPack,
                    TransportProtocol::Ssh,
                ),
                outcome,
                &mut out,
            )
            .expect("finish view exec");
            assert_eq!(
                out.events,
                [
                    RecordedEvent::ChannelSuccess,
                    RecordedEvent::Data(expected_err_packet(&match exit_code {
                        75 => format!("view {filter_id} unavailable: warming up"),
                        _ => format!("upload-pack: not our ref {}", "f".repeat(40)),
                    })),
                    RecordedEvent::ExitStatus(exit_code),
                    RecordedEvent::Eof,
                    RecordedEvent::Close,
                ]
            );
            assert!(channels.is_empty());
            assert_eq!(clients.len(), 2);
            assert!(clients.contains_key(&(3, 8)));
            assert!(clients.contains_key(&(4, 7)));
            assert_eq!(v2_channels, HashMap::from([(8_u32, true)]));
        }

        let initial_clients = HashMap::from([((3_usize, 7_u32), ()), ((3, 8), ()), ((4, 7), ())]);
        let initial_v2_channels = HashMap::from([(7_u32, true), (8_u32, true)]);

        let mut clients = initial_clients.clone();
        let mut v2_channels = initial_v2_channels.clone();
        let mut out = RecordedOut::default();
        reject_exec(
            &mut clients,
            &mut v2_channels,
            (3, 7),
            &SshErrReply::view_not_found(),
            &mut out,
        )
        .expect("reject exec");
        assert_eq!(
            out.events,
            [
                RecordedEvent::ChannelSuccess,
                RecordedEvent::Data(expected_err_packet("view not found")),
                RecordedEvent::ExitStatus(1),
                RecordedEvent::Eof,
                RecordedEvent::Close,
            ]
        );
        assert_eq!(
            clients,
            HashMap::from([((3_usize, 8_u32), ()), ((4, 7), ())])
        );
        assert_eq!(v2_channels, HashMap::from([(8_u32, true)]));

        let mut channels = HashMap::<u32, GitSshChannelState>::new();
        let mut clients = initial_clients;
        let mut v2_channels = initial_v2_channels;
        let mut out = RecordedOut::default();
        finish_v0_exec(
            &mut channels,
            &mut clients,
            &mut v2_channels,
            (3, 7),
            SmartSession::new(
                PathBuf::from("/"),
                ServiceType::UploadPack,
                TransportProtocol::Ssh,
            ),
            V0Exec::Advertise(BytesMut::from(&b"advertise"[..])),
            &mut out,
        )
        .expect("finish advertise");
        assert_eq!(
            out.events,
            [
                RecordedEvent::Data(Bytes::from_static(b"advertise")),
                RecordedEvent::ChannelSuccess,
            ]
        );
        assert!(
            channels
                .get(&7)
                .expect("advertise must register channel state")
                .exit_code
                .is_none()
        );
        assert_eq!(
            clients,
            HashMap::from([((3_usize, 7_u32), ()), ((3, 8), ()), ((4, 7), ())])
        );
        assert_eq!(v2_channels, HashMap::from([(7_u32, true), (8_u32, true)]));
    }

    #[test]
    fn data_view_error_writes_single_err() {
        let filter_id = "1".repeat(64);
        for (error, expected_message, expected_exit) in [
            (
                ProtocolError::ViewUnavailable {
                    filter_id: filter_id.clone(),
                    reason: crate::common::errors::ViewUnavailableReason::RootChainHalted,
                },
                format!("view {filter_id} unavailable: root chain halted"),
                75,
            ),
            (
                ProtocolError::PackRejected(format!("upload-pack: not our ref {}", "2".repeat(40))),
                format!("upload-pack: not our ref {}", "2".repeat(40)),
                1,
            ),
        ] {
            let mut state = upload_pack_state();
            let mut out = RecordedOut::default();
            write_data_error(&mut out, &mut state, &error);
            assert_eq!(
                out.events,
                [RecordedEvent::Data(expected_err_packet(&expected_message))]
            );
            assert_eq!(state.exit_code, Some(expected_exit));
        }
    }

    #[test]
    fn non_view_errors_unchanged() {
        let advertisement = BytesMut::from(&b"advertise"[..]);
        assert_eq!(
            classify_v0_advertise(Ok(advertisement.clone())).expect("advertisement"),
            V0Exec::Advertise(advertisement)
        );

        for error in [
            ProtocolError::NotFound("missing".to_owned()),
            ProtocolError::Forbidden("denied".to_owned()),
            ProtocolError::InvalidInput("x".to_owned()),
            ProtocolError::AdvertiseFailed,
        ] {
            let expected = error.to_string();
            let classified = classify_v0_advertise(Err(error)).unwrap_err();
            assert_eq!(classified.to_string(), expected);
        }

        let mut state = upload_pack_state();
        let mut out = RecordedOut::default();
        for error in [
            ProtocolError::InvalidInput("x".to_owned()),
            ProtocolError::NotFound("y".to_owned()),
        ] {
            write_data_error(&mut out, &mut state, &error);
        }
        assert_eq!(
            out.events,
            [
                RecordedEvent::Data(Bytes::from_static(b"error: Invalid Input: x\n")),
                RecordedEvent::Data(Bytes::from_static(b"error: Repository not found: y\n")),
            ]
        );
        assert_eq!(state.exit_code, None);
    }

    #[tokio::test]
    async fn data_stage_call_sites_use_writer() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let server = ssh_server_with_git(
            dir.path(),
            GitConfig {
                anonymous_access: true,
                push_auth: Some(PushAuth::None),
                ssh_receive_pack: Some(false),
                push_tokens: Vec::new(),
            },
        )
        .await;
        let expected = Bytes::from_static(
            b"error: Invalid Input: pkt-line length header is not hexadecimal\n",
        );

        for request in [
            Bytes::from_static(b"zzzz"),
            Bytes::from_static(b"0014command=ls-refs\n0001zzzz"),
            Bytes::from_static(b"0012command=fetch\n0001zzzz"),
        ] {
            let mut state = upload_pack_state();
            let mut out = RecordedOut::default();
            handle_upload_pack(&mut state, &server.state, &request, &mut out).await;
            assert_eq!(out.events, [RecordedEvent::Data(expected.clone())]);
            assert_eq!(state.exit_code, None);

            let mut state = upload_pack_state();
            state.exit_code = Some(75);
            let mut out = RecordedOut::default();
            handle_upload_pack(&mut state, &server.state, &request, &mut out).await;
            assert!(out.events.is_empty());
            assert_eq!(state.exit_code, Some(75));
        }

        let complete_request = Bytes::from(format!("0032want {}\n00000009done\n", "7".repeat(40)));
        let mut channels = HashMap::from([(7_u32, upload_pack_state())]);
        channels.get_mut(&7).expect("channel").exit_code = Some(75);
        let mut out = RecordedOut::default();
        on_channel_data(&mut channels, 7, &server.state, &complete_request, &mut out)
            .await
            .expect("errored channel data");
        assert_eq!(out.events, [RecordedEvent::ChannelSuccess]);
        assert!(channels.get(&7).expect("channel").data_combined.is_empty());

        let mut out = RecordedOut::default();
        on_channel_data(
            &mut HashMap::<u32, GitSshChannelState>::new(),
            7,
            &server.state,
            &complete_request,
            &mut out,
        )
        .await
        .expect("missing channel data");
        assert!(out.events.is_empty());

        let v2_error_request = Bytes::from_static(b"0012command=fetch\n0001zzzz0000");
        let mut channels = HashMap::from([(7_u32, upload_pack_state())]);
        let mut out = RecordedOut::default();
        on_channel_data(&mut channels, 7, &server.state, &v2_error_request, &mut out)
            .await
            .expect("plain error channel data");
        assert_eq!(
            out.events,
            [
                RecordedEvent::Data(expected.clone()),
                RecordedEvent::ChannelSuccess
            ]
        );

        let mut state = upload_pack_state();
        state.exit_code = Some(75);
        state.data_combined.extend_from_slice(b"zzzz");
        let mut channels = HashMap::from([(7_u32, state)]);
        let mut out = RecordedOut::default();
        finish_channel_eof(&mut channels, 7, true, &server.state, &mut out)
            .await
            .expect("errored EOF");
        assert_eq!(
            out.events,
            [RecordedEvent::ExitStatus(75), RecordedEvent::Close]
        );
        assert!(channels.is_empty());

        let mut out = RecordedOut::default();
        finish_channel_eof(
            &mut HashMap::<u32, GitSshChannelState>::new(),
            7,
            true,
            &server.state,
            &mut out,
        )
        .await
        .expect("missing EOF");
        assert_eq!(
            out.events,
            [RecordedEvent::ExitStatus(0), RecordedEvent::Close]
        );

        let mut channels = HashMap::from([(7_u32, upload_pack_state())]);
        let mut out = RecordedOut::default();
        finish_channel_eof(&mut channels, 7, true, &server.state, &mut out)
            .await
            .expect("empty EOF");
        assert_eq!(
            out.events,
            [RecordedEvent::ExitStatus(0), RecordedEvent::Close]
        );
        assert!(channels.is_empty());

        let mut residual = upload_pack_state();
        residual.data_combined.extend_from_slice(b"zzzz");
        let mut channels = HashMap::from([(7_u32, residual)]);
        let mut out = RecordedOut::default();
        finish_channel_eof(&mut channels, 7, true, &server.state, &mut out)
            .await
            .expect("residual EOF");
        assert_eq!(
            out.events,
            [
                RecordedEvent::Data(expected),
                RecordedEvent::ExitStatus(0),
                RecordedEvent::Close,
            ]
        );
        assert!(channels.is_empty());

        let mut channels = HashMap::<u32, GitSshChannelState>::new();
        let mut clients = HashMap::from([((3_usize, 7_u32), ())]);
        let mut v2_channels = HashMap::new();
        let mut out = RecordedOut::default();
        reject_exec(
            &mut clients,
            &mut v2_channels,
            (3, 7),
            &SshErrReply::view_not_found(),
            &mut out,
        )
        .expect("reject exec");
        let had_client_entry = clients.remove(&(3, 7)).is_some();
        finish_channel_eof(&mut channels, 7, had_client_entry, &server.state, &mut out)
            .await
            .expect("EOF after rejected exec");
        assert_eq!(
            out.events,
            [
                RecordedEvent::ChannelSuccess,
                RecordedEvent::Data(expected_err_packet("view not found")),
                RecordedEvent::ExitStatus(1),
                RecordedEvent::Eof,
                RecordedEvent::Close,
                RecordedEvent::Close,
            ]
        );

        let filter_id = "6".repeat(64);
        let mut state = upload_pack_state();
        let mut out = RecordedOut::default();
        let unavailable = ProtocolError::ViewUnavailable {
            filter_id: filter_id.clone(),
            reason: crate::common::errors::ViewUnavailableReason::WarmingUp,
        };
        write_data_error(&mut out, &mut state, &unavailable);
        let mut channels = HashMap::from([(7_u32, state)]);
        finish_channel_eof(&mut channels, 7, true, &server.state, &mut out)
            .await
            .expect("EOF after data error");
        assert_eq!(
            out.events,
            [
                RecordedEvent::Data(expected_err_packet(&format!(
                    "view {filter_id} unavailable: warming up"
                ))),
                RecordedEvent::ExitStatus(75),
                RecordedEvent::Close,
            ]
        );
        assert!(channels.is_empty());
    }

    #[test]
    fn channel_state_after_err() {
        let filter_id = "3".repeat(64);
        let unavailable = ProtocolError::ViewUnavailable {
            filter_id,
            reason: crate::common::errors::ViewUnavailableReason::WarmingUp,
        };
        let rejected =
            ProtocolError::PackRejected(format!("upload-pack: not our ref {}", "4".repeat(40)));
        for (error, expected_exit) in [(unavailable, 75), (rejected, 1)] {
            let mut state = upload_pack_state();
            let mut out = RecordedOut::default();
            write_data_error(&mut out, &mut state, &error);
            assert_eq!(eof_exit_code(Some(&state), true), Some(expected_exit));
        }
        assert_eq!(eof_exit_code(None, true), Some(0));
        assert_eq!(eof_exit_code(Some(&upload_pack_state()), true), Some(0));
        let mut non_view_error = upload_pack_state();
        let mut out = RecordedOut::default();
        write_data_error(
            &mut out,
            &mut non_view_error,
            &ProtocolError::InvalidInput("bad".to_owned()),
        );
        assert_eq!(eof_exit_code(Some(&non_view_error), true), Some(0));
        assert_eq!(eof_exit_code(None, false), None);

        let request = Bytes::from(format!("0032want {}\n00000009done\n", "5".repeat(40)));
        assert_eq!(take_ready_request(None, &request), None);
        let mut errored = upload_pack_state();
        errored.exit_code = Some(75);
        assert_eq!(take_ready_request(Some(&mut errored), &request), None);
        assert!(errored.data_combined.is_empty());
        let mut ready = upload_pack_state();
        assert_eq!(
            take_ready_request(Some(&mut ready), &request),
            Some(request.clone())
        );
        assert!(ready.data_combined.is_empty());

        errored.data_combined.extend_from_slice(b"residual");
        assert_eq!(take_eof_residual(&mut errored), None);
        let mut residual = upload_pack_state();
        residual.data_combined.extend_from_slice(b"residual");
        assert_eq!(
            take_eof_residual(&mut residual),
            Some(Bytes::from_static(b"residual"))
        );

        let mut out = RecordedOut::default();
        let mut state = upload_pack_state();
        state.exit_code = Some(75);
        let unavailable_again = ProtocolError::ViewUnavailable {
            filter_id: "3".repeat(64),
            reason: crate::common::errors::ViewUnavailableReason::WarmingUp,
        };
        write_data_error(&mut out, &mut state, &unavailable_again);
        write_data_error(
            &mut out,
            &mut state,
            &ProtocolError::InvalidInput("bad".to_owned()),
        );
        assert!(out.events.is_empty());
        assert_eq!(state.exit_code, Some(75));
    }

    #[tokio::test]
    async fn ssh_auth_none_accepts_only_when_storage_only_and_anonymous() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(
            dir.path(),
            GitConfig {
                anonymous_access: true,
                push_auth: Some(PushAuth::None),
                ssh_receive_pack: Some(false),
                push_tokens: Vec::new(),
            },
        )
        .await;
        let auth = server.auth_none("git").await.expect("auth_none");
        assert!(matches!(auth, Auth::Accept));
    }

    #[tokio::test]
    async fn ssh_auth_none_rejects_for_review_even_when_anonymous() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(
            dir.path(),
            GitConfig {
                anonymous_access: true,
                push_auth: None,
                ssh_receive_pack: None,
                push_tokens: Vec::new(),
            },
        )
        .await;
        let auth = server.auth_none("git").await.expect("auth_none");
        assert!(matches!(
            auth,
            Auth::Reject {
                partial_success: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn ssh_auth_none_rejects_when_anonymous_false() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(
            dir.path(),
            GitConfig {
                anonymous_access: false,
                push_auth: Some(PushAuth::Token),
                ssh_receive_pack: Some(false),
                push_tokens: Vec::new(),
            },
        )
        .await;
        let auth = server.auth_none("git").await.expect("auth_none");
        assert!(matches!(
            auth,
            Auth::Reject {
                partial_success: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn ssh_auth_publickey_rejects_storage_only_before_lookup() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(
            dir.path(),
            GitConfig {
                anonymous_access: true,
                push_auth: Some(PushAuth::Token),
                ssh_receive_pack: Some(false),
                push_tokens: Vec::new(),
            },
        )
        .await;
        let auth = server
            .auth_publickey("git", &sample_public_key())
            .await
            .expect("auth_publickey");
        assert!(matches!(
            auth,
            Auth::Reject {
                partial_success: false,
                ..
            }
        ));
        assert!(server.authenticated_user.is_none());
    }

    fn token_git_config(anonymous: bool) -> GitConfig {
        GitConfig {
            anonymous_access: anonymous,
            push_auth: Some(PushAuth::Token),
            ssh_receive_pack: Some(false),
            push_tokens: vec![crate::config::PushTokenConfig {
                name: "agent-ci".to_string(),
                token: "sp02-secret-token".to_string(),
                paths: None,
            }],
        }
    }

    #[tokio::test]
    async fn ssh_auth_password_token_hit_sets_name() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(dir.path(), token_git_config(false)).await;
        let auth = server
            .auth_password("ignored-user", "sp02-secret-token")
            .await
            .expect("auth_password");
        assert!(matches!(auth, Auth::Accept));
        assert_eq!(server.authenticated_user.as_deref(), Some("agent-ci"));
    }

    #[tokio::test]
    async fn ssh_auth_password_token_miss_rejects() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(dir.path(), token_git_config(false)).await;
        let auth = server
            .auth_password("git", "wrong-token")
            .await
            .expect("auth_password");
        assert!(matches!(
            auth,
            Auth::Reject {
                partial_success: false,
                ..
            }
        ));
        assert!(server.authenticated_user.is_none());
    }

    #[tokio::test]
    async fn ssh_auth_password_rejected_when_push_auth_none() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(
            dir.path(),
            GitConfig {
                anonymous_access: false,
                push_auth: Some(PushAuth::None),
                ssh_receive_pack: Some(false),
                push_tokens: Vec::new(),
            },
        )
        .await;
        let auth = server
            .auth_password("git", "sp02-secret-token")
            .await
            .expect("auth_password");
        assert!(matches!(
            auth,
            Auth::Reject {
                partial_success: false,
                ..
            }
        ));
        assert!(server.authenticated_user.is_none());
    }

    #[tokio::test]
    async fn ssh_auth_password_rejected_when_push_auth_omitted() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(
            dir.path(),
            GitConfig {
                anonymous_access: false,
                push_auth: None,
                ssh_receive_pack: None,
                push_tokens: vec![crate::config::PushTokenConfig {
                    name: "agent-ci".to_string(),
                    token: "sp02-secret-token".to_string(),
                    paths: None,
                }],
            },
        )
        .await;
        let auth = server
            .auth_password("git", "sp02-secret-token")
            .await
            .expect("auth_password");
        assert!(matches!(
            auth,
            Auth::Reject {
                partial_success: false,
                ..
            }
        ));
        assert!(server.authenticated_user.is_none());
    }

    #[tokio::test]
    async fn ssh_auth_password_ignores_ssh_username() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut server = ssh_server_with_git(dir.path(), token_git_config(false)).await;
        let auth = server
            .auth_password("not-the-token-name", "sp02-secret-token")
            .await
            .expect("auth_password");
        assert!(matches!(auth, Auth::Accept));
        assert_eq!(server.authenticated_user.as_deref(), Some("agent-ci"));
    }

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
        let empty = parse_ssh_exec_request("git-upload-pack ''").unwrap_err();
        assert!(empty.contains("repository path is empty"));
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
            pending_shallow_request: None,
            exit_code: None,
        };
        let mut channel_b = GitSshChannelState {
            smart_protocol: SmartSession::new(
                PathBuf::from("/b"),
                ServiceType::ReceivePack,
                TransportProtocol::Ssh,
            ),
            data_combined: BytesMut::new(),
            pending_shallow_request: None,
            exit_code: None,
        };

        channel_a.data_combined.extend_from_slice(b"payload-a");
        channel_b.data_combined.extend_from_slice(b"payload-b");

        assert_eq!(&channel_a.data_combined[..], b"payload-a");
        assert_eq!(&channel_b.data_combined[..], b"payload-b");
    }

    #[test]
    fn upload_pack_accepts_done_only_after_shallow_info() {
        assert!(!upload_pack_buffer_complete(b"0009done\n", false));
        assert!(upload_pack_buffer_complete(b"0009done\n", true));
        assert!(!upload_pack_buffer_complete(b"0009don", true));
        assert!(!upload_pack_buffer_complete(b"0009done", true));
        assert!(!upload_pack_buffer_complete(b"0000", true));
    }
}
