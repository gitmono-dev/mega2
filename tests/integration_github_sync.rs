// Loopback outbound SSH (plan-20260916 GS-07 / GS-08 / GS-24).
//
// Pins a generated Ed25519 host key and authenticates with the GS-05 hold
// against a local russh server. GS-08 adds a receive-pack advertisement.
// GS-24 writes the command frame and a synthetic pack. Does not talk to
// GitHub (DEFER-GS-08).

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, PublicKey},
    server::{self, Auth, ChannelOpenHandle, Handler, Msg, Server as _, Session},
};
use tokio::net::TcpListener;

/// The process hold is global. C-group runs this binary with `--test-threads=8`.
static IT_HOLD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone)]
struct LoopServer {
    user: String,
    public: PublicKey,
    expected_exec: Option<String>,
    advertisement: Option<Vec<u8>>,
    eof_after_advertisement: bool,
    inbound: Option<Arc<Mutex<Vec<u8>>>>,
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
                if self.eof_after_advertisement {
                    session.eof(channel)?;
                }
            }
            Some(_) => {
                session.channel_failure(channel)?;
            }
            None => {}
        }
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(inbound) = &self.inbound {
            inbound.lock().expect("inbound").extend_from_slice(data);
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

fn advertise_bytes(oid: &str, caps: &str) -> Vec<u8> {
    let mut advertisement = pkt_line(&format!("{oid} refs/heads/main\0{caps}\n"));
    advertisement.extend_from_slice(b"0000");
    advertisement
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
            eof_after_advertisement: true,
            inbound: None,
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
    let mut session = connect_loopback(
        LoopServer {
            user: "git".to_string(),
            public: client_pub,
            expected_exec: Some("git-receive-pack 'acme/app.git'".to_string()),
            advertisement: Some(advertise_bytes(oid, "report-status")),
            eof_after_advertisement: true,
            inbound: None,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn loopback_receive_pack() {
    let _hold = IT_HOLD.lock().await;
    let client_pub = install_client();
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key");
    let old = "0123456789abcdef0123456789abcdef01234567";
    let new = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let inbound = Arc::new(Mutex::new(Vec::new()));
    let mut session = connect_loopback(
        LoopServer {
            user: "git".to_string(),
            public: client_pub,
            expected_exec: Some("git-receive-pack 'acme/app.git'".to_string()),
            advertisement: Some(advertise_bytes(old, "report-status side-band-64k")),
            eof_after_advertisement: false,
            inbound: Some(inbound.clone()),
        },
        host_key,
    )
    .await;
    let mut receive = mega2_core::github_sync::begin(&mut session, "acme/app")
        .await
        .expect("begin");
    assert_eq!(receive.advertisement.main_tip, old);
    let window = mega2_core::github_sync::PACK_WRITE_WINDOW;
    // One oversize caller chunk (3 windows + 11 bytes). Not collected into a pack Vec.
    let fat: Vec<u8> = (0..(window * 3 + 11))
        .map(|i| u8::try_from(i % 251).expect("tag"))
        .collect();
    let expected_pack = fat.len();
    let stats = receive
        .write_pack(new, std::iter::once(fat.as_slice()))
        .await
        .expect("write pack");
    assert_eq!(stats.peak_chunk, window);
    assert_eq!(stats.pack_bytes, expected_pack as u64);
    assert!(stats.peak_chunk < expected_pack);
    assert!(!stats.missing_sideband);
    let plan = mega2_core::github_sync::build_command_frame(old, new, &receive.advertisement)
        .expect("plan");
    let expected = plan.bytes.len() + expected_pack;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if inbound.lock().expect("inbound").len() >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("server collected streamed pack");
    let got = inbound.lock().expect("inbound").clone();
    assert!(
        got.starts_with(&plan.bytes),
        "command+flush must lead the write"
    );
    assert_eq!(got.len(), expected);
    assert_eq!(&got[plan.bytes.len()..], fat.as_slice());
    drop(receive);
    drop(session);
    mega2_core::github_sync::clear_held();
}
