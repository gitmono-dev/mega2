// Loopback outbound SSH (plan-20260916 GS-07 / GS-08).
//
// Pins a generated Ed25519 host key and authenticates with the GS-05 hold
// against a local russh server. GS-08 adds a receive-pack advertisement.
// Does not talk to GitHub (DEFER-GS-08).

use std::{sync::Arc, time::Duration};

/// The process hold is global. C-group runs this binary with `--test-threads=8`.
static IT_HOLD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, PublicKey},
    server::{self, Auth, ChannelOpenHandle, Handler, Msg, Server as _, Session},
};
use tokio::net::TcpListener;

#[derive(Clone)]
struct LoopServer {
    user: String,
    public: PublicKey,
    expected_exec: Option<String>,
    advertisement: Option<Vec<u8>>,
}

impl server::Server for LoopServer {
    type Handler = Self;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl Handler for LoopServer {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        if user == self.user
            && public_key.algorithm() == self.public.algorithm()
            && public_key.key_data() == self.public.key_data()
        {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match &self.expected_exec {
            Some(expected) if data == expected.as_bytes() => {
                session.channel_success(channel)?;
                if let Some(advertisement) = &self.advertisement {
                    session.data(channel, advertisement.clone())?;
                }
                session.eof(channel)?;
            }
            Some(_) => {
                session.channel_failure(channel)?;
            }
            None => {}
        }
        Ok(())
    }
}

async fn connect_loopback(
    server: LoopServer,
    host_key: PrivateKey,
) -> mega2_core::github_sync::SshSession {
    let host_pub = host_key
        .public_key()
        .to_openssh()
        .expect("host public")
        .trim()
        .to_string();
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
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });
    mega2_core::github_sync::connect(&mega2_core::config::GithubSyncConfig {
        ssh_host: addr.to_string(),
        ssh_user: "git".to_string(),
        ssh_host_key: host_pub,
        ..Default::default()
    })
    .await
    .expect("loopback public-key auth")
}

fn install_client() -> PublicKey {
    mega2_core::github_sync::clear_held();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("client key");
    let openssh = client_key.to_openssh(LineEnding::LF).expect("encode hold");
    mega2_core::github_sync::install_openssh(openssh.as_str()).expect("install hold");
    client_key.public_key().clone()
}

fn pkt_line(body: &str) -> Vec<u8> {
    let total = body.len() + 4;
    let mut out = format!("{total:04x}").into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ssh_connect_authenticates() {
    let _hold = IT_HOLD.lock().await;
    let client_pub = install_client();
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key");
    let session = connect_loopback(
        LoopServer {
            user: "git".to_string(),
            public: client_pub,
            expected_exec: None,
            advertisement: None,
        },
        host_key,
    )
    .await;
    drop(session);
    mega2_core::github_sync::clear_held();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn loopback_advertise() {
    let _hold = IT_HOLD.lock().await;
    let client_pub = install_client();
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key");
    let oid = "0123456789abcdef0123456789abcdef01234567";
    let mut advertisement = pkt_line(&format!("{oid} refs/heads/main\0report-status\n"));
    advertisement.extend_from_slice(b"0000");
    let mut session = connect_loopback(
        LoopServer {
            user: "git".to_string(),
            public: client_pub,
            expected_exec: Some("git-receive-pack 'acme/app.git'".to_string()),
            advertisement: Some(advertisement),
        },
        host_key,
    )
    .await;
    let parsed = mega2_core::github_sync::advertise(&mut session, "acme/app")
        .await
        .expect("advertisement");
    assert_eq!(parsed.main_tip, oid);
    assert!(parsed.capabilities.contains("report-status"));
    drop(session);
    mega2_core::github_sync::clear_held();
}
