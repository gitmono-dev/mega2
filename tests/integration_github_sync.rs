// Loopback outbound SSH session (plan-20260916 GS-07).
//
// Pins a generated Ed25519 host key and authenticates with the GS-05 hold
// against a local russh server. Does not talk to GitHub (DEFER-GS-08).

use std::{sync::Arc, time::Duration};

use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use russh::{
    keys::{Algorithm, PrivateKey, PublicKey},
    server::{self, Auth, Server as _},
};
use tokio::net::TcpListener;

#[derive(Clone)]
struct AcceptServer {
    user: String,
    public: PublicKey,
}

impl server::Server for AcceptServer {
    type Handler = Self;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl server::Handler for AcceptServer {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ssh_connect_authenticates() {
    mega2_core::github_sync::clear_held();

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("host key");
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("client key");
    let openssh = client_key.to_openssh(LineEnding::LF).expect("encode hold");
    mega2_core::github_sync::install_openssh(openssh.as_str()).expect("install hold");
    let client_pub = client_key.public_key().clone();

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
    let mut server = AcceptServer {
        user: "git".to_string(),
        public: client_pub,
    };
    let _join = tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });

    let session = mega2_core::github_sync::connect(&mega2_core::config::GithubSyncConfig {
        ssh_host: addr.to_string(),
        ssh_user: "git".to_string(),
        ssh_host_key: host_pub,
        ..Default::default()
    })
    .await
    .expect("loopback public-key auth");
    drop(session);
    mega2_core::github_sync::clear_held();
}
