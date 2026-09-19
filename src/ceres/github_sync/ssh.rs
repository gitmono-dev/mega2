use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use russh::{
    client,
    keys::{Algorithm, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate},
};

use crate::{common::errors::MegaError, config::GithubSyncConfig};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Established outbound SSH session after host-key pin and public-key auth.
pub struct SshSession {
    session: client::Handle<PinHandler>,
}

impl std::fmt::Debug for SshSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshSession").finish_non_exhaustive()
    }
}

/// Failure stage for an outbound github_sync SSH attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshStage {
    Connect,
    HostKey,
    Auth,
}

impl SshStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "ssh_connect",
            Self::HostKey => "host_key",
            Self::Auth => "auth",
        }
    }
}

/// Staged outbound SSH error (plan-20260916 GS-07).
#[derive(Debug)]
pub struct SshError {
    stage: SshStage,
    message: String,
    auth_attempted: bool,
}

impl SshError {
    fn new(stage: SshStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
            auth_attempted: false,
        }
    }

    fn connect(message: impl Into<String>) -> Self {
        Self::new(SshStage::Connect, message)
    }

    fn host_key(message: impl Into<String>) -> Self {
        Self::new(SshStage::HostKey, message)
    }

    fn auth(message: impl Into<String>) -> Self {
        Self::new(SshStage::Auth, message)
    }

    fn with_auth_flag(mut self, auth_attempted: bool) -> Self {
        self.auth_attempted = auth_attempted;
        self
    }

    pub fn stage(&self) -> SshStage {
        self.stage
    }

    pub fn stage_name(&self) -> &'static str {
        self.stage.as_str()
    }

    pub fn auth_attempted(&self) -> bool {
        self.auth_attempted
    }
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "github_sync stage={}: {}",
            self.stage.as_str(),
            self.message
        )
    }
}

impl std::error::Error for SshError {}

impl From<SshError> for MegaError {
    fn from(err: SshError) -> Self {
        MegaError::Other(err.to_string())
    }
}

impl From<russh::Error> for SshError {
    fn from(err: russh::Error) -> Self {
        match err {
            russh::Error::UnknownKey => SshError::host_key("host key does not match ssh_host_key"),
            other => SshError::connect(format!("ssh connect failed: {other}")),
        }
    }
}

struct PinHandler {
    pinned: String,
    auth_called: Arc<AtomicBool>,
}

impl client::Handler for PinHandler {
    type Error = SshError;

    async fn auth_banner(
        &mut self,
        _banner: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.auth_called.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                if host_key_matches(&self.pinned, key)? {
                    Ok(true)
                } else {
                    Err(SshError::host_key("host key does not match ssh_host_key"))
                }
            }
            PublicKeyOrCertificate::Certificate(_) => {
                Err(SshError::host_key("certificate host keys are not accepted"))
            }
        }
    }
}

fn host_key_matches(pinned: &str, presented: &PublicKey) -> Result<bool, SshError> {
    let pinned = pinned.trim();
    if pinned.is_empty() {
        return Err(SshError::host_key("ssh_host_key is empty"));
    }
    let encoded = presented
        .to_openssh()
        .map_err(|_| SshError::host_key("server host key could not be encoded"))?;
    if encoded.trim().as_bytes() == pinned.as_bytes() {
        return Ok(true);
    }
    let parsed = PublicKey::from_openssh(pinned)
        .map_err(|_| SshError::host_key("ssh_host_key is not a valid OpenSSH public key"))?;
    Ok(parsed.algorithm() == presented.algorithm() && parsed.key_data() == presented.key_data())
}

fn parse_endpoint(ssh_host: &str) -> Result<(String, u16), SshError> {
    let host = ssh_host.trim();
    if host.is_empty() {
        return Err(SshError::connect("ssh_host is empty"));
    }
    if let Ok(addr) = host.parse::<SocketAddr>() {
        return Ok((addr.ip().to_string(), addr.port()));
    }
    if let Some((name, port)) = host.rsplit_once(':')
        && !name.is_empty()
        && !name.contains(']')
        && port.bytes().all(|b| b.is_ascii_digit())
    {
        let port: u16 = port
            .parse()
            .map_err(|_| SshError::connect("ssh_host port is invalid"))?;
        return Ok((name.to_string(), port));
    }
    Ok((host.to_string(), 22))
}

/// Connect using the GS-05 process hold, pin `ssh_host_key`, then authenticate.
pub async fn connect(config: &GithubSyncConfig) -> Result<SshSession, SshError> {
    connect_inner(config, CONNECT_TIMEOUT).await
}

async fn connect_inner(config: &GithubSyncConfig, limit: Duration) -> Result<SshSession, SshError> {
    let key = super::key::held().ok_or_else(|| {
        SshError::auth(
            "github_sync ssh key is not held; enable [github_sync] so startup can install it",
        )
    })?;
    if key.algorithm() != Algorithm::Ed25519 {
        return Err(SshError::auth(
            "github_sync ssh key must be Ed25519; delete the vault entry and restart one replica",
        ));
    }
    if config.ssh_user.trim().is_empty() {
        return Err(SshError::auth("ssh_user is empty"));
    }
    if config.ssh_host_key.trim().is_empty() {
        return Err(SshError::host_key("ssh_host_key is empty"));
    }

    let (host, port) = parse_endpoint(&config.ssh_host)?;
    let auth_called = Arc::new(AtomicBool::new(false));
    let handler = PinHandler {
        pinned: config.ssh_host_key.clone(),
        auth_called: auth_called.clone(),
    };
    // Finite so a timed-out `connect` cannot leak the russh session task
    // that `client::connect` spawns before KEX finishes. Successful
    // sessions stay live while they see traffic within this bound.
    let client_config = Arc::new(client::Config {
        inactivity_timeout: Some(limit),
        nodelay: true,
        ..Default::default()
    });

    let user = config.ssh_user.trim().to_string();
    let private = Arc::new(key.private_key().clone());
    let work = async {
        let mut session = russh::client::connect(client_config, (host.as_str(), port), handler)
            .await
            .map_err(|err| err.with_auth_flag(auth_called.load(Ordering::SeqCst)))?;
        auth_called.store(true, Ordering::SeqCst);
        let hash = session
            .best_supported_rsa_hash()
            .await
            .map_err(|err| {
                SshError::auth(format!("publickey authentication failed: {err}"))
                    .with_auth_flag(true)
            })?
            .flatten();
        let result = session
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(private, hash))
            .await
            .map_err(|err| {
                SshError::auth(format!("publickey authentication failed: {err}"))
                    .with_auth_flag(true)
            })?;
        if !result.success() {
            return Err(SshError::auth("publickey authentication rejected").with_auth_flag(true));
        }
        Ok(SshSession { session })
    };

    match tokio::time::timeout(limit, work).await {
        Err(_) => Err(SshError::connect(format!(
            "ssh connect timed out after {}ms",
            limit.as_millis()
        ))
        .with_auth_flag(auth_called.load(Ordering::SeqCst))),
        Ok(result) => result,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use russh::{
        keys::PrivateKey,
        server::{self, Auth, Server as _},
    };
    use tokio::net::TcpListener;

    use super::*;
    use crate::ceres::github_sync::key::{
        GithubSyncKey, HOLDER_TEST_SERIAL as TEST_SERIAL, clear_held_for_it, install_key_for_it,
        install_openssh_for_it,
    };

    #[derive(Clone)]
    struct LoopServer {
        accept: bool,
        expected_user: String,
        expected_key: Option<PublicKey>,
        auth_hits: Arc<AtomicUsize>,
    }

    impl server::Server for LoopServer {
        type Handler = Self;

        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
            self.clone()
        }
    }

    impl server::Handler for LoopServer {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            user: &str,
            public_key: &PublicKey,
        ) -> Result<Auth, Self::Error> {
            self.auth_hits.fetch_add(1, Ordering::SeqCst);
            let user_ok = user == self.expected_user;
            let key_ok = self.expected_key.as_ref().is_none_or(|expected| {
                expected.algorithm() == public_key.algorithm()
                    && expected.key_data() == public_key.key_data()
            });
            if self.accept && user_ok && key_ok {
                Ok(Auth::Accept)
            } else {
                Ok(Auth::reject())
            }
        }
    }

    struct Loopback {
        addr: SocketAddr,
        host_pub: String,
        auth_hits: Arc<AtomicUsize>,
        _join: tokio::task::JoinHandle<()>,
    }

    async fn spawn_loopback(host_key: PrivateKey, server: LoopServer) -> Loopback {
        let host_pub = host_key
            .public_key()
            .to_openssh()
            .expect("host public key")
            .trim()
            .to_string();
        let auth_hits = server.auth_hits.clone();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let config = Arc::new(server::Config {
            auth_rejection_time: Duration::from_millis(0),
            auth_rejection_time_initial: Some(Duration::from_millis(0)),
            keys: vec![host_key],
            inactivity_timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        });
        let mut server = server;
        let join = tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        Loopback {
            addr,
            host_pub,
            auth_hits,
            _join: join,
        }
    }

    fn random_ed25519() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("ed25519")
    }

    fn install_ed25519(key: &PrivateKey) -> GithubSyncKey {
        let openssh = key
            .to_openssh(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .expect("encode client key");
        install_openssh_for_it(openssh.as_str()).expect("install hold")
    }

    fn sync_config(addr: SocketAddr, user: &str, host_key: &str) -> GithubSyncConfig {
        GithubSyncConfig {
            ssh_host: addr.to_string(),
            ssh_user: user.to_string(),
            ssh_host_key: host_key.to_string(),
            ..GithubSyncConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn host_key_mismatch_aborts_before_auth() {
        let _serial = TEST_SERIAL.lock().await;
        clear_held_for_it();
        let client_key = random_ed25519();
        install_ed25519(&client_key);
        let server_host = random_ed25519();
        let other_host = random_ed25519();
        let other_pub = other_host
            .public_key()
            .to_openssh()
            .expect("other host pub")
            .trim()
            .to_string();
        let auth_hits = Arc::new(AtomicUsize::new(0));
        let loopback = spawn_loopback(
            server_host,
            LoopServer {
                accept: true,
                expected_user: "git".to_string(),
                expected_key: Some(client_key.public_key().clone()),
                auth_hits: auth_hits.clone(),
            },
        )
        .await;
        let err = connect(&sync_config(loopback.addr, "git", &other_pub))
            .await
            .expect_err("mismatched host key must fail");
        assert_eq!(err.stage(), SshStage::HostKey, "{err}");
        assert_eq!(err.stage_name(), "host_key", "{err}");
        assert!(!err.auth_attempted(), "client auth callback was invoked");
        assert_eq!(
            loopback.auth_hits.load(Ordering::SeqCst),
            0,
            "server auth callback was invoked"
        );
        clear_held_for_it();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn stage_errors_are_distinguishable() {
        let _serial = TEST_SERIAL.lock().await;
        clear_held_for_it();

        let refused = connect(&GithubSyncConfig {
            ssh_host: "127.0.0.1:1".to_string(),
            ssh_user: "git".to_string(),
            ssh_host_key: "ssh-ed25519 AAAA".to_string(),
            ..GithubSyncConfig::default()
        })
        .await
        .expect_err("held key missing is auth; install first after this check");

        // No hold yet: auth stage, before TCP.
        assert_eq!(refused.stage(), SshStage::Auth, "{refused}");
        assert!(!refused.auth_attempted(), "{refused}");

        let client_key = random_ed25519();
        install_ed25519(&client_key);

        let connect_refused = connect(&GithubSyncConfig {
            ssh_host: "127.0.0.1:1".to_string(),
            ssh_user: "git".to_string(),
            ssh_host_key: "ssh-ed25519 AAAA".to_string(),
            ..GithubSyncConfig::default()
        })
        .await
        .expect_err("refused TCP");
        assert_eq!(
            connect_refused.stage(),
            SshStage::Connect,
            "{connect_refused}"
        );
        assert_eq!(connect_refused.stage_name(), "ssh_connect");
        assert!(!connect_refused.auth_attempted());

        let hanging = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("hanging listener");
        let hang_addr = hanging.local_addr().expect("hang addr");
        let timeout_err = connect_inner(
            &sync_config(hang_addr, "git", "ssh-ed25519 AAAA"),
            Duration::from_millis(200),
        )
        .await
        .expect_err("handshake timeout");
        drop(hanging);
        assert_eq!(timeout_err.stage(), SshStage::Connect, "{timeout_err}");
        assert!(
            timeout_err.to_string().contains("timed out"),
            "{timeout_err}"
        );
        assert!(!timeout_err.auth_attempted());

        let stalling = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stalling listener");
        let stall_addr = stalling.local_addr().expect("stall addr");
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = stalling.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(b"SSH-2.0-gs07-stall\r\n").await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        let stall_err = connect_inner(
            &sync_config(stall_addr, "git", "ssh-ed25519 AAAA"),
            Duration::from_millis(400),
        )
        .await
        .expect_err("stalled kex after banner");
        assert_eq!(stall_err.stage(), SshStage::Connect, "{stall_err}");
        assert!(stall_err.to_string().contains("timed out"), "{stall_err}");
        assert!(!stall_err.auth_attempted());

        let server_host = random_ed25519();
        let other_host = random_ed25519();
        let other_pub = other_host
            .public_key()
            .to_openssh()
            .expect("other host pub")
            .trim()
            .to_string();
        let mismatch = spawn_loopback(
            server_host,
            LoopServer {
                accept: true,
                expected_user: "git".to_string(),
                expected_key: None,
                auth_hits: Arc::new(AtomicUsize::new(0)),
            },
        )
        .await;
        let host_err = connect(&sync_config(mismatch.addr, "git", &other_pub))
            .await
            .expect_err("host key");
        assert_eq!(host_err.stage(), SshStage::HostKey, "{host_err}");
        assert_eq!(host_err.stage_name(), "host_key");

        let accept_host = random_ed25519();
        let reject = spawn_loopback(
            accept_host,
            LoopServer {
                accept: false,
                expected_user: "git".to_string(),
                expected_key: None,
                auth_hits: Arc::new(AtomicUsize::new(0)),
            },
        )
        .await;
        let auth_err = connect(&sync_config(reject.addr, "git", &reject.host_pub))
            .await
            .expect_err("rejected auth");
        assert_eq!(auth_err.stage(), SshStage::Auth, "{auth_err}");
        assert_eq!(auth_err.stage_name(), "auth");
        assert!(auth_err.auth_attempted(), "{auth_err}");

        let rsa = PrivateKey::random(&mut rand::rng(), Algorithm::Rsa { hash: None }).expect("rsa");
        install_key_for_it(GithubSyncKey::from_private_key_for_test(rsa));
        let algo_err = connect(&sync_config(reject.addr, "git", &reject.host_pub))
            .await
            .expect_err("non-ed25519");
        assert_eq!(algo_err.stage(), SshStage::Auth, "{algo_err}");
        assert!(algo_err.to_string().contains("Ed25519"), "{algo_err}");
        assert!(!algo_err.auth_attempted());

        clear_held_for_it();
    }
}
