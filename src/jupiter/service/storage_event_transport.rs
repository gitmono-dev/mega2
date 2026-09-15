//! HTTPS HMAC transport for storage-only committed-write events (WH-09).
//!
//! This module is not injected into AppContext yet (WH-11). Production
//! construction has no HTTP or private-IP escape switch.

#[cfg(test)]
use std::sync::Arc;
use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    time::Duration,
};

#[cfg(test)]
type TestHostResolver = Arc<dyn Fn(&str, u16) -> Vec<IpAddr> + Send + Sync>;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::redirect::Policy;
use sha2::Sha256;
use url::Url;

use crate::{
    common::errors::MegaError,
    config::{secret::SecretString, validate::validate_storage_events_target_url},
};

const HMAC_HEX_PREFIX: &str = "hex:";
const HMAC_KEY_MIN_LEN: usize = 32;
const HMAC_KEY_MAX_LEN: usize = 256;
const SIGNATURE_HEADER_PREFIX: &str = "sha256=";

type HmacSha256 = Hmac<Sha256>;
type TransportFutureInner =
    Pin<Box<dyn Future<Output = Result<TransportSuccess, TransportError>> + Send>>;

#[derive(Clone)]
pub struct EventTarget {
    pub id: String,
    url: Url,
    hmac_key: Vec<u8>,
}

impl EventTarget {
    pub fn compile(
        id: impl Into<String>,
        url: &str,
        secret: &SecretString,
    ) -> Result<Self, MegaError> {
        validate_storage_events_target_url(url)?;
        let parsed = Url::parse(url).map_err(|err| {
            MegaError::Other(format!(
                "[[storage_events.targets]] url is not a valid URL ({err})"
            ))
        })?;
        Ok(Self {
            id: id.into(),
            url: parsed,
            hmac_key: decode_hmac_secret(secret)?,
        })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }
}

impl fmt::Debug for EventTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventTarget")
            .field("id", &self.id)
            .field("url", &"<redacted>")
            .field("hmac_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSuccess {
    Accepted2xx { status: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Redirect,
    Connect,
    Tls,
    Resolve,
    Timeout,
    Cancelled,
    Non2xx { status: u16 },
}

impl TransportError {
    pub fn category(self) -> &'static str {
        match self {
            Self::Redirect => "redirect",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Resolve => "resolve",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Non2xx { .. } => "non_2xx",
        }
    }
}

pub trait EventTransport: Send + Sync {
    fn post(&self, target: &EventTarget, body: Bytes) -> TransportFutureInner;
}

pub struct HttpsEventTransport {
    connect_timeout: Duration,
    request_timeout: Duration,
    #[cfg(test)]
    timestamp_override: Option<u64>,
    #[cfg(test)]
    injected_client: Option<reqwest::Client>,
    #[cfg(test)]
    extra_root: Option<reqwest::Certificate>,
    #[cfg(test)]
    test_resolver: Option<TestHostResolver>,
    #[cfg(test)]
    pin_override: Option<SocketAddr>,
}

impl HttpsEventTransport {
    pub fn new(connect_timeout: Duration, request_timeout: Duration) -> Result<Self, MegaError> {
        Ok(Self {
            connect_timeout,
            request_timeout,
            #[cfg(test)]
            timestamp_override: None,
            #[cfg(test)]
            injected_client: None,
            #[cfg(test)]
            extra_root: None,
            #[cfg(test)]
            test_resolver: None,
            #[cfg(test)]
            pin_override: None,
        })
    }

    #[cfg(test)]
    fn new_for_tests(client: reqwest::Client, timestamp_override: Option<u64>) -> Self {
        Self {
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            timestamp_override,
            injected_client: Some(client),
            extra_root: None,
            test_resolver: None,
            pin_override: None,
        }
    }

    #[cfg(test)]
    fn new_pinned_for_tests(
        connect_timeout: Duration,
        request_timeout: Duration,
        extra_root: reqwest::Certificate,
        resolver: TestHostResolver,
        pin_override: SocketAddr,
        timestamp_override: Option<u64>,
    ) -> Self {
        Self {
            connect_timeout,
            request_timeout,
            timestamp_override,
            injected_client: None,
            extra_root: Some(extra_root),
            test_resolver: Some(resolver),
            pin_override: Some(pin_override),
        }
    }
}

impl EventTransport for HttpsEventTransport {
    fn post(&self, target: &EventTarget, body: Bytes) -> TransportFutureInner {
        let url = target.url.clone();
        let target_id = target.id.clone();
        let hmac_key = target.hmac_key.clone();
        let connect_timeout = self.connect_timeout;
        let request_timeout = self.request_timeout;
        let timestamp = {
            #[cfg(test)]
            {
                self.timestamp_override
                    .unwrap_or_else(|| chrono::Utc::now().timestamp().max(0) as u64)
            }
            #[cfg(not(test))]
            {
                chrono::Utc::now().timestamp().max(0) as u64
            }
        };
        #[cfg(test)]
        let injected_client = self.injected_client.clone();
        #[cfg(test)]
        let extra_root = self.extra_root.clone();
        #[cfg(test)]
        let test_resolver = self.test_resolver.clone();
        #[cfg(test)]
        let pin_override = self.pin_override;
        Box::pin(async move {
            let event_id = envelope_string_field(&body, "event_id").unwrap_or_default();
            let event_type = envelope_string_field(&body, "event_type").unwrap_or_default();
            let signature = sign_body(&hmac_key, timestamp, &body);
            let client = {
                #[cfg(test)]
                if let Some(client) = injected_client {
                    client
                } else {
                    match pin_client_for_url(
                        &url,
                        connect_timeout,
                        request_timeout,
                        extra_root,
                        test_resolver.as_ref(),
                        pin_override,
                    )
                    .await
                    {
                        Ok(client) => client,
                        Err(err) => {
                            tracing::info!(
                                target_id = %target_id,
                                event_type = event_type.as_str(),
                                outcome = err.category(),
                                "storage_events delivery"
                            );
                            return Err(err);
                        }
                    }
                }
                #[cfg(not(test))]
                {
                    match pin_client_for_url(&url, connect_timeout, request_timeout).await {
                        Ok(client) => client,
                        Err(err) => {
                            tracing::info!(
                                target_id = %target_id,
                                event_type = event_type.as_str(),
                                outcome = err.category(),
                                "storage_events delivery"
                            );
                            return Err(err);
                        }
                    }
                }
            };
            let result = client
                .post(url)
                .header("Content-Type", "application/json")
                .header("X-Mega2-Event-Id", event_id.as_str())
                .header("X-Mega2-Timestamp", timestamp.to_string())
                .header(
                    "X-Mega2-Signature",
                    format!("{SIGNATURE_HEADER_PREFIX}{signature}"),
                )
                .body(body)
                .send()
                .await;
            let outcome = match result {
                Ok(response) => classify_status(response.status().as_u16()),
                Err(err) => Err(classify_reqwest_error(&err)),
            };
            let category = match &outcome {
                Ok(_) => "accepted_2xx",
                Err(err) => err.category(),
            };
            tracing::info!(
                target_id = %target_id,
                event_type = event_type.as_str(),
                outcome = category,
                "storage_events delivery"
            );
            outcome
        })
    }
}

pub fn decode_hmac_secret(secret: &SecretString) -> Result<Vec<u8>, MegaError> {
    let raw = secret.expose_secret();
    let hex_part = raw.strip_prefix(HMAC_HEX_PREFIX).ok_or_else(|| {
        MegaError::Other(
            "[storage_events] HMAC secret must use hex:<even-hex> encoding".to_string(),
        )
    })?;
    if hex_part.is_empty() || hex_part.len() % 2 != 0 {
        return Err(MegaError::Other(
            "[storage_events] HMAC secret hex payload must be non-empty even-length hex"
                .to_string(),
        ));
    }
    if !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MegaError::Other(
            "[storage_events] HMAC secret hex payload contains illegal characters".to_string(),
        ));
    }
    let bytes = hex::decode(hex_part).map_err(|_| {
        MegaError::Other(
            "[storage_events] HMAC secret hex payload could not be decoded".to_string(),
        )
    })?;
    if !(HMAC_KEY_MIN_LEN..=HMAC_KEY_MAX_LEN).contains(&bytes.len()) {
        return Err(MegaError::Other(
            "[storage_events] HMAC secret must decode to 32..=256 bytes".to_string(),
        ));
    }
    Ok(bytes)
}

pub fn sign_body(hmac_key: &[u8], timestamp_secs: u64, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(hmac_key).expect("HMAC-SHA256 accepts 32..=256 byte keys");
    mac.update(timestamp_secs.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn classify_status(status: u16) -> Result<TransportSuccess, TransportError> {
    if (300..400).contains(&status) {
        return Err(TransportError::Redirect);
    }
    if (200..300).contains(&status) {
        return Ok(TransportSuccess::Accepted2xx { status });
    }
    Err(TransportError::Non2xx { status })
}

fn classify_reqwest_error(err: &reqwest::Error) -> TransportError {
    if err.is_timeout() {
        return TransportError::Timeout;
    }
    if err.is_connect() {
        return TransportError::Connect;
    }
    let display = err.to_string().to_ascii_lowercase();
    if display.contains("dns") || display.contains("resolve") || display.contains("lookup") {
        return TransportError::Resolve;
    }
    if display.contains("tls") || display.contains("certificate") || display.contains("ssl") {
        return TransportError::Tls;
    }
    if err.is_request() {
        return TransportError::Connect;
    }
    TransportError::Connect
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddressClass {
    Public,
    Loopback,
    Private,
    LinkLocal,
    Metadata,
    OtherRestricted,
}

pub fn classify_ip(ip: IpAddr) -> AddressClass {
    if is_metadata_ip(ip) {
        return AddressClass::Metadata;
    }
    match ip {
        IpAddr::V4(v4) => classify_ipv4(v4),
        IpAddr::V6(v6) => classify_ipv6(v6),
    }
}

fn classify_ipv4(ip: Ipv4Addr) -> AddressClass {
    if ip.is_loopback() {
        AddressClass::Loopback
    } else if ip.is_private() {
        AddressClass::Private
    } else if ip.is_link_local() {
        AddressClass::LinkLocal
    } else if ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.octets()[0] == 0
        || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        || (ip.octets()[0] == 198 && (18..=19).contains(&ip.octets()[1]))
    {
        AddressClass::OtherRestricted
    } else {
        AddressClass::Public
    }
}

fn classify_ipv6(ip: Ipv6Addr) -> AddressClass {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return classify_ip(IpAddr::V4(v4));
    }
    if ip.is_loopback() {
        AddressClass::Loopback
    } else if ip.is_unique_local() {
        AddressClass::Private
    } else if ip.is_unicast_link_local() {
        AddressClass::LinkLocal
    } else if ip.is_unspecified() || ip.is_multicast() {
        AddressClass::OtherRestricted
    } else {
        AddressClass::Public
    }
}

fn is_metadata_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4 == Ipv4Addr::new(169, 254, 169, 254),
        IpAddr::V6(v6) => v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254),
    }
}

pub fn pin_resolved_addresses(addrs: &[IpAddr]) -> Result<IpAddr, TransportError> {
    if addrs.is_empty() {
        return Err(TransportError::Resolve);
    }
    let classes: BTreeSet<AddressClass> = addrs.iter().copied().map(classify_ip).collect();
    if classes.len() != 1 || !classes.contains(&AddressClass::Public) {
        return Err(TransportError::Resolve);
    }
    Ok(addrs[0])
}

async fn pin_client_for_url(
    url: &Url,
    connect_timeout: Duration,
    request_timeout: Duration,
    #[cfg(test)] extra_root: Option<reqwest::Certificate>,
    #[cfg(test)] test_resolver: Option<&TestHostResolver>,
    #[cfg(test)] pin_override: Option<SocketAddr>,
) -> Result<reqwest::Client, TransportError> {
    let host = url.host_str().ok_or(TransportError::Resolve)?;
    let port = url.port_or_known_default().ok_or(TransportError::Resolve)?;
    let ips = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
        Some(url::Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
        Some(url::Host::Domain(domain)) => {
            #[cfg(test)]
            if let Some(resolver) = test_resolver {
                resolver(domain, port)
            } else {
                lookup_ips_with_timeout(domain, port, request_timeout).await?
            }
            #[cfg(not(test))]
            {
                lookup_ips_with_timeout(domain, port, request_timeout).await?
            }
        }
        None => return Err(TransportError::Resolve),
    };
    let chosen = pin_resolved_addresses(&ips)?;
    let pinned = {
        #[cfg(test)]
        {
            pin_override.unwrap_or(SocketAddr::new(chosen, port))
        }
        #[cfg(not(test))]
        {
            SocketAddr::new(chosen, port)
        }
    };
    let builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .resolve(host, pinned);
    #[cfg(test)]
    let builder = match extra_root {
        Some(cert) => builder.add_root_certificate(cert),
        None => builder,
    };
    builder.build().map_err(|_| TransportError::Connect)
}

async fn lookup_ips_with_timeout(
    host: &str,
    port: u16,
    request_timeout: Duration,
) -> Result<Vec<IpAddr>, TransportError> {
    let lookup = tokio::time::timeout(request_timeout, tokio::net::lookup_host((host, port))).await;
    match lookup {
        Ok(Ok(addrs)) => Ok(addrs.map(|addr| addr.ip()).collect()),
        Ok(Err(_)) => Err(TransportError::Resolve),
        Err(_) => Err(TransportError::Timeout),
    }
}

fn envelope_string_field(body: &[u8], field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value
        .get(field)
        .and_then(|item| item.as_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration as StdDuration,
    };

    use openssl::{
        asn1::Asn1Time,
        hash::MessageDigest,
        pkey::PKey,
        rsa::Rsa,
        ssl::{SslAcceptor, SslMethod, SslStream},
        x509::{X509, X509NameBuilder, extension::SubjectAlternativeName},
    };

    use super::*;
    use crate::config::testing::{EnvVarGuard, env_lock};

    const GOLDEN_TIMESTAMP: u64 = 1_700_000_000;
    const GOLDEN_BODY: &[u8] =
        br#"{"event_id":"11111111-1111-4111-8111-111111111111","event_type":"repo.push"}"#;
    const GOLDEN_SIGNATURE: &str =
        "d0e0290698d34b6378b176e936e9023b7dcd917f5d2bd56bc5de7cbc574317b9";
    const GOLDEN_KEY_HEX: &str =
        "hex:1111111111111111111111111111111111111111111111111111111111111111";

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    struct TlsCollector {
        addr: String,
        cert_der: Vec<u8>,
        hits: Arc<AtomicUsize>,
        rx: mpsc::Receiver<CapturedRequest>,
    }

    fn test_hmac_secret() -> SecretString {
        SecretString::new(GOLDEN_KEY_HEX)
    }

    fn generate_localhost_cert() -> (X509, PKey<openssl::pkey::Private>, Vec<u8>) {
        let rsa = Rsa::generate(2048).expect("rsa");
        let key = PKey::from_rsa(rsa).expect("pkey");
        let mut name = X509NameBuilder::new().expect("name");
        name.append_entry_by_text("CN", "localhost").expect("cn");
        let name = name.build();
        let mut builder = X509::builder().expect("x509");
        builder.set_version(2).expect("version");
        builder.set_subject_name(&name).expect("subject");
        builder.set_issuer_name(&name).expect("issuer");
        builder.set_pubkey(&key).expect("pubkey");
        builder
            .set_not_before(&Asn1Time::days_from_now(0).expect("not_before"))
            .expect("set not_before");
        builder
            .set_not_after(&Asn1Time::days_from_now(1).expect("not_after"))
            .expect("set not_after");
        let san = SubjectAlternativeName::new()
            .ip("127.0.0.1")
            .dns("localhost")
            .dns("events.example.test")
            .build(&builder.x509v3_context(None, None))
            .expect("san");
        builder.append_extension(san).expect("append san");
        builder
            .sign(&key, MessageDigest::sha256())
            .expect("sign cert");
        let cert = builder.build();
        let der = cert.to_der().expect("cert der");
        (cert, key, der)
    }

    fn spawn_tls_collector(
        handler: impl Fn(CapturedRequest) -> (u16, Vec<(&'static str, String)>, Option<StdDuration>)
        + Send
        + 'static,
    ) -> TlsCollector {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind collector");
        let addr = listener.local_addr().expect("local addr");
        let (cert, key, cert_der) = generate_localhost_cert();
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("acceptor");
        acceptor.set_certificate(&cert).expect("cert");
        acceptor.set_private_key(&key).expect("key");
        acceptor.check_private_key().expect("key match");
        let acceptor = acceptor.build();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let Ok(mut tls) = acceptor.accept(stream) else {
                    continue;
                };
                if let Some(captured) = read_http_request(&mut tls) {
                    let (status, extra_headers, delay) = handler(captured.clone());
                    let _ = tx.send(captured);
                    if let Some(delay) = delay {
                        thread::sleep(delay);
                    }
                    let mut response = format!(
                        "HTTP/1.1 {status} TEST\r\nContent-Length: 0\r\nConnection: close\r\n"
                    );
                    for (name, value) in extra_headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    let _ = tls.write_all(response.as_bytes());
                }
                let _ = tls.shutdown();
            }
        });
        TlsCollector {
            addr: format!("127.0.0.1:{}", addr.port()),
            cert_der,
            hits,
            rx,
        }
    }

    fn read_http_request(stream: &mut SslStream<std::net::TcpStream>) -> Option<CapturedRequest> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(header_end) = find_header_end(&buf) {
                let header_text = std::str::from_utf8(&buf[..header_end]).ok()?;
                let mut lines = header_text.split("\r\n");
                let request_line = lines.next()?;
                let path = request_line.split_whitespace().nth(1)?.to_string();
                let mut headers = Vec::new();
                let mut content_length = 0usize;
                for line in lines {
                    if line.is_empty() {
                        continue;
                    }
                    let (name, value) = line.split_once(':')?;
                    let name = name.trim().to_string();
                    let value = value.trim().to_string();
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.parse().unwrap_or(0);
                    }
                    headers.push((name, value));
                }
                let mut body = buf[header_end..].to_vec();
                while body.len() < content_length {
                    let n = stream.read(&mut tmp).ok()?;
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                body.truncate(content_length);
                return Some(CapturedRequest {
                    path,
                    headers,
                    body,
                });
            }
            if buf.len() > 64 * 1024 {
                return None;
            }
        }
        None
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|idx| idx + 4)
    }

    fn test_client(cert_der: &[u8], connect: StdDuration, request: StdDuration) -> reqwest::Client {
        let cert = reqwest::Certificate::from_der(cert_der).expect("test ca");
        reqwest::Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(connect)
            .timeout(request)
            .add_root_certificate(cert)
            .build()
            .expect("test client")
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn network_policy_matrix() {
        let err = crate::config::validate::validate_storage_events_target_url(
            "http://events.example.invalid/ingest",
        )
        .expect_err("http");
        assert!(err.to_string().contains("https"), "{err}");

        let err = crate::config::validate::validate_storage_events_target_url(
            "https://user:pass@events.example.invalid/ingest",
        )
        .expect_err("userinfo");
        assert!(err.to_string().contains("userinfo"), "{err}");

        let err = crate::config::validate::validate_storage_events_target_url(
            "https://events.example.invalid/ingest?x=1",
        )
        .expect_err("query");
        assert!(err.to_string().contains("query"), "{err}");

        let err = crate::config::validate::validate_storage_events_target_url(
            "https://events.example.invalid/ingest#frag",
        )
        .expect_err("fragment");
        assert!(err.to_string().contains("fragment"), "{err}");

        crate::config::validate::validate_storage_events_target_url(
            "https://events.example.invalid/ingest",
        )
        .expect("valid https url");

        let collector = spawn_tls_collector(|captured| {
            (
                302,
                vec![(
                    "Location",
                    format!("https://events.example.invalid{}", captured.path),
                )],
                None,
            )
        });
        let client = test_client(
            &collector.cert_der,
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
        );
        let transport = HttpsEventTransport::new_for_tests(client, Some(GOLDEN_TIMESTAMP));
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://{}/ingest", collector.addr),
            &test_hmac_secret(),
        )
        .expect("target");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Err(TransportError::Redirect));
        assert_eq!(collector.hits.load(Ordering::SeqCst), 1);

        let proxy = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
        let proxy_addr = proxy.local_addr().expect("proxy addr");
        let proxy_hits = Arc::new(AtomicUsize::new(0));
        let proxy_hits_clone = Arc::clone(&proxy_hits);
        thread::spawn(move || {
            for _ in proxy.incoming() {
                proxy_hits_clone.fetch_add(1, Ordering::SeqCst);
            }
        });
        let lock = env_lock();
        let _http = EnvVarGuard::set(&lock, "HTTP_PROXY", &format!("http://{proxy_addr}"));
        let _https = EnvVarGuard::set(&lock, "HTTPS_PROXY", &format!("http://{proxy_addr}"));
        let _all = EnvVarGuard::set(&lock, "ALL_PROXY", &format!("http://{proxy_addr}"));
        let collector = spawn_tls_collector(|_| (200, Vec::new(), None));
        let client = test_client(
            &collector.cert_der,
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
        );
        let transport = HttpsEventTransport::new_for_tests(client, Some(GOLDEN_TIMESTAMP));
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://{}/ingest", collector.addr),
            &test_hmac_secret(),
        )
        .expect("target");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Ok(TransportSuccess::Accepted2xx { status: 200 }));
        assert_eq!(proxy_hits.load(Ordering::SeqCst), 0);
        drop(_http);
        drop(_https);
        drop(_all);
        drop(lock);
    }

    #[test]
    fn hmac_single_attempt_timeout() {
        let key = decode_hmac_secret(&test_hmac_secret()).expect("decode");
        assert_eq!(
            sign_body(&key, GOLDEN_TIMESTAMP, GOLDEN_BODY),
            GOLDEN_SIGNATURE
        );

        let err = decode_hmac_secret(&SecretString::new("")).expect_err("empty");
        assert!(err.to_string().contains("hex:"), "{err}");
        let err = decode_hmac_secret(&SecretString::new("hex:1")).expect_err("odd");
        assert!(err.to_string().contains("even-length"), "{err}");
        let err = decode_hmac_secret(&SecretString::new("hex:zz")).expect_err("illegal");
        assert!(err.to_string().contains("illegal"), "{err}");
        let err = decode_hmac_secret(&SecretString::new("hex:11")).expect_err("short");
        assert!(err.to_string().contains("32..=256"), "{err}");

        let collector = spawn_tls_collector(|_| (200, Vec::new(), None));
        let client = test_client(
            &collector.cert_der,
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
        );
        let transport = HttpsEventTransport::new_for_tests(client, Some(GOLDEN_TIMESTAMP));
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://{}/ingest", collector.addr),
            &test_hmac_secret(),
        )
        .expect("target");
        let debug = format!("{target:?}");
        assert!(!debug.contains("11111111"), "{debug}");
        assert!(!debug.contains(&collector.addr), "{debug}");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Ok(TransportSuccess::Accepted2xx { status: 200 }));
        let captured = collector
            .rx
            .recv_timeout(StdDuration::from_secs(2))
            .expect("capture");
        let signature = captured
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-mega2-signature"))
            .map(|(_, value)| value.as_str())
            .expect("signature header");
        assert_eq!(signature, format!("sha256={GOLDEN_SIGNATURE}"));
        assert_eq!(captured.body, GOLDEN_BODY);

        let collector = spawn_tls_collector(|_| (429, Vec::new(), None));
        let client = test_client(
            &collector.cert_der,
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
        );
        let transport = HttpsEventTransport::new_for_tests(client, Some(GOLDEN_TIMESTAMP));
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://{}/ingest", collector.addr),
            &test_hmac_secret(),
        )
        .expect("target");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Err(TransportError::Non2xx { status: 429 }));
        assert_eq!(collector.hits.load(Ordering::SeqCst), 1);

        let collector = spawn_tls_collector(|_| (503, Vec::new(), None));
        let client = test_client(
            &collector.cert_der,
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
        );
        let transport = HttpsEventTransport::new_for_tests(client, Some(GOLDEN_TIMESTAMP));
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://{}/ingest", collector.addr),
            &test_hmac_secret(),
        )
        .expect("target");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Err(TransportError::Non2xx { status: 503 }));
        assert_eq!(collector.hits.load(Ordering::SeqCst), 1);

        let listener = TcpListener::bind("127.0.0.1:0").expect("drop bind");
        let addr = listener.local_addr().expect("addr");
        let (cert, key, cert_der) = generate_localhost_cert();
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("acceptor");
        acceptor.set_certificate(&cert).expect("cert");
        acceptor.set_private_key(&key).expect("key");
        let acceptor = acceptor.build();
        thread::spawn(move || {
            if let Ok(stream) = listener.incoming().next().expect("one") {
                let _ = acceptor.accept(stream);
            }
        });
        let client = test_client(
            &cert_der,
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
        );
        let transport = HttpsEventTransport::new_for_tests(client, Some(GOLDEN_TIMESTAMP));
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://127.0.0.1:{}/ingest", addr.port()),
            &test_hmac_secret(),
        )
        .expect("target");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert!(
            matches!(
                outcome,
                Err(TransportError::Connect | TransportError::Tls | TransportError::Timeout)
            ),
            "{outcome:?}"
        );

        let collector = spawn_tls_collector(|_| (200, Vec::new(), Some(StdDuration::from_secs(3))));
        let client = test_client(
            &collector.cert_der,
            StdDuration::from_secs(1),
            StdDuration::from_millis(200),
        );
        let transport = HttpsEventTransport::new_for_tests(client, Some(GOLDEN_TIMESTAMP));
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://{}/ingest", collector.addr),
            &test_hmac_secret(),
        )
        .expect("target");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Err(TransportError::Timeout));
        assert!(!format!("{outcome:?}").contains(&collector.addr));
    }

    #[test]
    fn validated_address_policy() {
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))),
            AddressClass::Public
        );
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            AddressClass::Loopback
        );
        assert_eq!(
            classify_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            AddressClass::Loopback
        );
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            AddressClass::Private
        );
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
            AddressClass::Private
        );
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))),
            AddressClass::Private
        );
        assert_eq!(
            classify_ip("fd12::1".parse().expect("ula")),
            AddressClass::Private
        );
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))),
            AddressClass::LinkLocal
        );
        assert_eq!(
            classify_ip("fe80::1".parse().expect("ll")),
            AddressClass::LinkLocal
        );
        assert_eq!(
            classify_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
            AddressClass::Metadata
        );
        assert_eq!(
            classify_ip(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped())),
            AddressClass::Loopback
        );
        assert_eq!(
            classify_ip(IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped())),
            AddressClass::Private
        );
        assert_eq!(
            classify_ip(IpAddr::V6(
                Ipv4Addr::new(169, 254, 169, 254).to_ipv6_mapped()
            )),
            AddressClass::Metadata
        );
        assert_eq!(
            pin_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))]).expect("public"),
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))
        );
        assert_eq!(
            pin_resolved_addresses(&[IpAddr::V4(Ipv4Addr::LOCALHOST)]),
            Err(TransportError::Resolve)
        );
        assert_eq!(
            pin_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]),
            Err(TransportError::Resolve)
        );
        assert_eq!(
            pin_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))]),
            Err(TransportError::Resolve)
        );
        assert_eq!(
            pin_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))]),
            Err(TransportError::Resolve)
        );
        assert_eq!(
            pin_resolved_addresses(&[
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            ]),
            Err(TransportError::Resolve)
        );
    }

    #[test]
    fn dns_rebind_and_ip_classes() {
        let lookups = Arc::new(AtomicUsize::new(0));
        let lookups_clone = Arc::clone(&lookups);
        let resolver: TestHostResolver = Arc::new(move |host, _| {
            lookups_clone.fetch_add(1, Ordering::SeqCst);
            assert_eq!(host, "events.example.test");
            if lookups_clone.load(Ordering::SeqCst) == 1 {
                vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))]
            } else {
                vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]
            }
        });
        let collector = spawn_tls_collector(|_| (200, Vec::new(), None));
        let pin: SocketAddr = collector.addr.parse().expect("collector addr");
        let cert = reqwest::Certificate::from_der(&collector.cert_der).expect("ca");
        let transport = HttpsEventTransport::new_pinned_for_tests(
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
            cert,
            resolver,
            pin,
            Some(GOLDEN_TIMESTAMP),
        );
        let target = EventTarget::compile(
            "ops-main",
            &format!("https://events.example.test:{}/ingest", pin.port()),
            &test_hmac_secret(),
        )
        .expect("target");
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Ok(TransportSuccess::Accepted2xx { status: 200 }));
        assert_eq!(lookups.load(Ordering::SeqCst), 1);

        let mixed: TestHostResolver = Arc::new(|_, _| {
            vec![
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            ]
        });
        let transport = HttpsEventTransport::new_pinned_for_tests(
            StdDuration::from_secs(2),
            StdDuration::from_secs(5),
            reqwest::Certificate::from_der(&collector.cert_der).expect("ca"),
            mixed,
            pin,
            Some(GOLDEN_TIMESTAMP),
        );
        let outcome = runtime().block_on(transport.post(&target, Bytes::from_static(GOLDEN_BODY)));
        assert_eq!(outcome, Err(TransportError::Resolve));
    }
}
