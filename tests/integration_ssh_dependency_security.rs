use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use russh::{
    client,
    keys::{
        Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate,
        agent::{AgentIdentity, Constraint, client::AgentClient, server as agent_server},
        ssh_key::private::RsaKeypair,
    },
    server,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::oneshot,
    time::{sleep, timeout},
};

const CASE_TIMEOUT: Duration = Duration::from_secs(20);

struct ObservedStream {
    stream: DuplexStream,
    closed: Option<oneshot::Sender<()>>,
}

impl Drop for ObservedStream {
    fn drop(&mut self) {
        if let Some(closed) = self.closed.take() {
            let _ = closed.send(());
        }
    }
}

impl AsyncRead for ObservedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for ObservedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

async fn private_agent() -> (AgentClient<DuplexStream>, oneshot::Receiver<()>) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (closed, observed) = oneshot::channel();
    agent_server::serve(
        futures::stream::iter([Ok(ObservedStream {
            stream: server,
            closed: Some(closed),
        })]),
        (),
    )
    .await
    .expect("start private agent");
    (AgentClient::connect(client), observed)
}

struct PinnedServerKey(PublicKey);

impl client::Handler for PinnedServerKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(key.public_key() == self.0)
    }
}

struct AcceptFixtureKey {
    key: PublicKey,
    authenticated: Arc<AtomicUsize>,
}

impl server::Handler for AcceptFixtureKey {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        Ok(if user == "fixture" && key == &self.key {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        assert_eq!(user, "fixture");
        assert_eq!(key, &self.key);
        self.authenticated.fetch_add(1, Ordering::SeqCst);
        Ok(server::Auth::Accept)
    }
}

struct Sha1Signer(AgentClient<DuplexStream>);

impl russh::Signer for Sha1Signer {
    type Error = russh::AgentAuthError;

    async fn auth_sign(
        &mut self,
        key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, Self::Error> {
        assert_eq!(hash_alg, Some(HashAlg::Sha512));
        self.0
            .sign_request(key, None, data)
            .await
            .map_err(Into::into)
    }
}

async fn authenticate_fixture(mismatched: bool) -> (bool, usize) {
    let private = Arc::new(PrivateKey::from(
        RsaKeypair::random(&mut rand::rng(), 2048).expect("temporary RSA key"),
    ));
    let host = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let pinned = PinnedServerKey(host.public_key().clone());
    let authenticated = Arc::new(AtomicUsize::new(0));
    let handler = AcceptFixtureKey {
        key: private.public_key().clone(),
        authenticated: authenticated.clone(),
    };
    let config = server::Config {
        keys: vec![host],
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::from_millis(1)),
        ..Default::default()
    };
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        server::run_stream(Arc::new(config), server_io, handler)
            .await
            .unwrap()
            .await
    });
    let mut session =
        client::connect_stream(Arc::new(client::Config::default()), client_io, pinned)
            .await
            .unwrap();
    let success = if mismatched {
        let (mut agent, closed) = private_agent().await;
        agent.add_identity(&private, &[]).await.unwrap();
        let mut signer = Sha1Signer(agent);
        let result = session
            .authenticate_publickey_with(
                "fixture",
                private.public_key().clone(),
                Some(HashAlg::Sha512),
                &mut signer,
            )
            .await
            .unwrap();
        drop(signer);
        closed.await.expect("agent connection exited");
        result.success()
    } else {
        session
            .authenticate_publickey(
                "fixture",
                PrivateKeyWithHashAlg::new(private, Some(HashAlg::Sha512)),
            )
            .await
            .unwrap()
            .success()
    };
    session
        .disconnect(russh::Disconnect::ByApplication, "fixture complete", "en")
        .await
        .unwrap();
    let client_exit = session.await;
    let server_exit = server.await.expect("server task joined");
    assert!(
        matches!(client_exit, Err(russh::Error::Disconnect)),
        "locally initiated disconnect must finish the client task: {client_exit:?}"
    );
    server_exit.expect("server session exited");
    (success, authenticated.load(Ordering::SeqCst))
}

#[tokio::test]
async fn publickey_mismatched_algorithm_rejected() {
    let (success, callbacks) = timeout(CASE_TIMEOUT, authenticate_fixture(true))
        .await
        .expect("bounded SSH fixture");
    assert!(
        !success,
        "SHA-1 signature must not authenticate a SHA-512 request"
    );
    assert_eq!(
        callbacks, 0,
        "rejected signature must not reach auth callback"
    );
}

#[tokio::test]
async fn publickey_matching_rsa_sha512_accepted() {
    let (success, callbacks) = timeout(CASE_TIMEOUT, authenticate_fixture(false))
        .await
        .expect("bounded SSH fixture");
    assert!(success, "matching RSA-SHA2-512 must authenticate");
    assert_eq!(callbacks, 1);
}

#[tokio::test]
async fn agent_duplicate_lifetime_rejected_without_identity() {
    timeout(CASE_TIMEOUT, async {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let (mut agent, closed) = private_agent().await;
        let result = agent
            .add_identity(
                &key,
                &[
                    Constraint::KeyLifetime { seconds: 1 },
                    Constraint::KeyLifetime { seconds: 1 },
                ],
            )
            .await;
        let identities = agent.request_identities().await.unwrap();
        // Let the old version's two bounded timers finish before its RED assertion.
        sleep(Duration::from_millis(1100)).await;
        drop(agent);
        closed.await.expect("agent connection exited");
        assert!(
            matches!(result, Err(russh::keys::Error::AgentFailure)),
            "duplicate lifetime must return AgentFailure"
        );
        assert!(identities.is_empty(), "rejected key must not be inserted");
    })
    .await
    .expect("bounded agent fixture");
}

#[tokio::test]
async fn agent_single_lifetime_expires() {
    timeout(CASE_TIMEOUT, async {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let (mut agent, closed) = private_agent().await;
        agent
            .add_identity(&key, &[Constraint::KeyLifetime { seconds: 2 }])
            .await
            .unwrap();
        let identities = agent.request_identities().await.unwrap();
        assert_eq!(identities.len(), 1);
        assert!(matches!(&identities[0], AgentIdentity::PublicKey { key: actual, .. } if actual == key.public_key()));
        timeout(Duration::from_secs(4), async {
            while !agent.request_identities().await.unwrap().is_empty() {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("lifetime must expire");
        drop(agent);
        closed.await.expect("agent connection exited");
    })
    .await
    .expect("bounded agent fixture");
}
