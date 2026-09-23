//! `mega2 path provision` (plan-20260923 ADR-FU-05 item 4): a thin client of
//! `POST /api/v1/path/provision` on a running storage-only server. It reads
//! no config; the token comes only from `MEGA2_TOKEN` and is sent as
//! `Authorization: Bearer`.

use std::{error::Error as _, ffi::OsString, io::Read, time::Duration};

use clap::{Arg, ArgMatches, Command};
use reqwest::{
    Url,
    blocking::Client,
    header::{CONTENT_TYPE, LOCATION},
    redirect::Policy,
};
use serde::Deserialize;

use crate::{
    ceres::model::git::PathProvisionResult,
    commands::CommandContext,
    common::errors::{MegaError, MegaResult},
};

/// Environment variable holding the push token (never an argv option).
pub const TOKEN_ENV: &str = "MEGA2_TOKEN";

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
/// Upper bound on the response body read into memory.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// The server's `CommonResult<PathProvisionResult>` envelope.
#[derive(Deserialize)]
struct Envelope {
    req_result: bool,
    data: Option<PathProvisionResult>,
    #[serde(default)]
    err_message: String,
}

pub fn cli() -> Command {
    Command::new("path")
        .about("Monorepo path operations against a running mega2 server")
        .subcommand_required(true)
        .subcommand(
            Command::new("provision")
                .about(
                    "Create a monorepo path and its missing parent directories (idempotent); \
                     the push token is read from MEGA2_TOKEN",
                )
                .arg(
                    Arg::new("server")
                        .long("server")
                        .value_name("URL")
                        .required(true)
                        .help("Server base URL, e.g. http://127.0.0.1:9000"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .help("Canonical monorepo path, e.g. /project/team/demo"),
                ),
        )
}

pub(crate) fn exec(_ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("provision", sub)) => provision(sub),
        Some((other, _)) => Err(MegaError::Other(format!(
            "Unknown path subcommand: {other}"
        ))),
        None => Err(MegaError::Other("path requires a subcommand".to_owned())),
    }
}

fn provision(args: &ArgMatches) -> MegaResult {
    let server = args
        .get_one::<String>("server")
        .ok_or_else(|| MegaError::Other("--server is required".to_owned()))?;
    let path = args
        .get_one::<String>("path")
        .ok_or_else(|| MegaError::Other("PATH is required".to_owned()))?;
    let token = read_token(std::env::var_os(TOKEN_ENV))?;
    let direct = Url::parse(server).is_ok_and(|url| is_loopback(&url));
    let line = provision_request(
        &http_client(TIMEOUT, direct)?,
        server,
        path,
        token.as_deref(),
    )?;
    println!("{line}");
    Ok(())
}

/// The token from `MEGA2_TOKEN`: unset or empty means none; a value that is
/// not UTF-8 or holds control characters is an error that never echoes it.
fn read_token(value: Option<OsString>) -> Result<Option<String>, MegaError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let token = value
        .into_string()
        .map_err(|_| MegaError::cli_exit(1, format!("{TOKEN_ENV} must be valid UTF-8")))?;
    if token.contains(char::is_control) {
        return Err(MegaError::cli_exit(
            1,
            format!("{TOKEN_ENV} must not contain control characters"),
        ));
    }
    Ok(Some(token).filter(|token| !token.is_empty()))
}

/// HTTP client for the provisioning call: bounded timeout, redirects followed
/// only within the same origin (scheme, host and port). A loopback server is
/// reached directly: no proxy can serve the caller's own loopback.
fn http_client(timeout: Duration, direct: bool) -> Result<Client, MegaError> {
    let builder = if direct {
        Client::builder().no_proxy()
    } else {
        Client::builder()
    };
    builder
        .timeout(timeout)
        .redirect(Policy::custom(|attempt| {
            let same_origin = attempt
                .previous()
                .first()
                .is_some_and(|first| same_origin(first, attempt.url()));
            if !same_origin {
                attempt.stop()
            } else if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|err| MegaError::cli_exit(1, format!("cannot build the HTTP client: {err}")))
}

/// A transport error with its cause chain (no URL, no headers).
fn transport_error(err: reqwest::Error) -> String {
    if err.is_timeout() {
        return format!("timed out after {} s", TIMEOUT.as_secs());
    }
    let err = err.without_url();
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Send one provisioning request and render the one-line outcome
/// (`created <path> (<commit>)` / `already exists <path>`). A server error
/// becomes exit code 1 with its `<CODE>: …` text on stderr. Error text never
/// carries the token (redacted), URL credentials (refused) or an arbitrary
/// response body (only the server's JSON `err_message` is shown); the body
/// read is bounded by [`MAX_RESPONSE_BYTES`].
fn provision_request(
    client: &Client,
    server: &str,
    path: &str,
    token: Option<&str>,
) -> Result<String, MegaError> {
    let redact = |text: String| match token {
        Some(token) if !token.is_empty() => text.replace(token, "***"),
        _ => text,
    };
    let fail = |message: String| MegaError::cli_exit(1, redact(message));
    let base = Url::parse(server)
        .map_err(|err| MegaError::cli_exit(1, format!("invalid --server URL: {err}")))?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(fail(format!(
            "invalid --server URL: scheme must be http or https, got {}",
            base.scheme()
        )));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(MegaError::cli_exit(
            1,
            "invalid --server URL: credentials in the URL are not supported; set MEGA2_TOKEN"
                .to_owned(),
        ));
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(fail(
            "invalid --server URL: it must not have a query or fragment".to_owned(),
        ));
    }
    let origin = base.origin().ascii_serialization();
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|()| fail("invalid --server URL: it cannot be a base URL".to_owned()))?
        .pop_if_empty()
        .extend(["api", "v1", "path", "provision"]);
    let mut request = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "path": path }).to_string());
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|err| fail(format!("cannot reach {origin}: {}", transport_error(err))))?;
    let status = response.status();
    if status.is_redirection() {
        let target = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|location| response.url().join(location).ok())
            .map(|url| format!(" to {}", url.origin().ascii_serialization()))
            .unwrap_or_default();
        return Err(fail(format!(
            "server answered {status}{target}; redirects to another origin are not followed"
        )));
    }
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|err| fail(format!("cannot read the response: {err}")))?;
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(fail(format!(
            "HTTP {status}: response larger than {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    let Ok(envelope) = serde_json::from_slice::<Envelope>(&body) else {
        return Err(fail(format!(
            "HTTP {status}: unexpected (non-JSON) response"
        )));
    };
    if !status.is_success() || !envelope.req_result {
        let message = if envelope.err_message.is_empty() {
            format!("HTTP {status}: request failed")
        } else {
            envelope.err_message
        };
        return Err(fail(message));
    }
    match envelope.data {
        Some(PathProvisionResult {
            path: provisioned,
            created: true,
            commit_id: Some(commit),
        }) if provisioned == path && !commit.is_empty() => {
            Ok(format!("created {provisioned} ({commit})"))
        }
        Some(PathProvisionResult {
            path: provisioned,
            created: false,
            commit_id: None,
        }) if provisioned == path => Ok(format!("already exists {provisioned}")),
        _ => Err(fail(format!(
            "HTTP {status}: response does not match the provisioning contract"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;

    /// A local HTTP test server; dropping it stops the accept loop and joins
    /// its thread, so no listener or socket outlives the test.
    struct TestServer {
        url: String,
        hits: Arc<AtomicUsize>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        addr: std::net::SocketAddr,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake the blocking accept so the loop sees the stop flag.
            let _ = std::net::TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Serves `responses` in order (the last one repeats); `{self}` in a
    /// response is replaced by the server's own base URL.
    fn serve_sequence(responses: Vec<String>) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (counter, stopped, own) = (Arc::clone(&hits), Arc::clone(&stop), url.clone());
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { break };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                read_request(&mut stream);
                let response = responses[n.min(responses.len() - 1)].replace("{self}", &own);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        TestServer {
            url,
            hits,
            stop,
            addr,
            handle: Some(handle),
        }
    }

    /// Answers every connection with `response`.
    fn serve(response: String) -> TestServer {
        serve_sequence(vec![response])
    }

    /// Read one request fully (headers, then `Content-Length` body bytes) so
    /// closing the socket never resets unread data.
    fn read_request(stream: &mut std::net::TcpStream) {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let Ok(n) = stream.read(&mut buf) else { return };
            if n == 0 {
                return;
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data);
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if data.len() >= end + 4 + length {
                    return;
                }
            }
        }
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn provision_does_not_follow_cross_host_redirects() {
        let target = serve(json_response(
            "200 OK",
            r#"{"req_result":true,"data":{"path":"/p","created":false,"commit_id":null},"err_message":""}"#,
        ));
        // Same port and scheme, different host name: `localhost` vs `127.0.0.1`.
        let other_host = target.url.replace("127.0.0.1", "localhost");
        let server = serve(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {other_host}/api/v1/path/provision\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ));
        let client = http_client(Duration::from_secs(5), true).unwrap();
        let err = provision_request(&client, &server.url, "/p", Some("secret-token"))
            .expect_err("cross-host redirect");
        let text = err.to_string();
        assert!(text.contains("not followed"), "{text}");
        assert!(text.contains("to http://localhost:"), "{text}");
        assert!(!text.contains("secret-token"), "{text}");
        assert_eq!(target.hits(), 0);
        assert_eq!(err.process_exit_code(), 1);
    }

    #[test]
    fn provision_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        // Accept but never answer (for a bounded while), then let go.
        let hold = thread::spawn(move || {
            let _conn = listener.accept();
            thread::sleep(Duration::from_secs(1));
        });
        let client = http_client(Duration::from_millis(300), true).unwrap();
        let started = std::time::Instant::now();
        let err = provision_request(&client, &server, "/p", None).expect_err("timeout");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(TIMEOUT, Duration::from_secs(30));
        hold.join().unwrap();
    }

    #[test]
    fn provision_renders_outcomes_and_server_errors() {
        let client = http_client(Duration::from_secs(5), true).unwrap();
        let server = serve(json_response(
            "200 OK",
            r#"{"req_result":true,"data":{"path":"/project/a","created":true,"commit_id":"abc123"},"err_message":""}"#,
        ));
        assert_eq!(
            provision_request(&client, &server.url, "/project/a", None).unwrap(),
            "created /project/a (abc123)"
        );
        let server = serve(json_response(
            "400 Bad Request",
            r#"{"req_result":false,"data":null,"err_message":"MONO_PATH_NOT_ALLOWED: cannot create \"/vendor/x\""}"#,
        ));
        let err = provision_request(&client, &server.url, "/vendor/x", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "MONO_PATH_NOT_ALLOWED: cannot create \"/vendor/x\""
        );
        assert_eq!(err.process_exit_code(), 1);
        let err = provision_request(&client, "ftp://example.invalid", "/p", None).unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    fn redirect_to_self() -> String {
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {self}/api/v1/path/provision\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
    }

    #[test]
    fn provision_follows_same_origin_redirects_up_to_a_cap() {
        let client = http_client(Duration::from_secs(5), true).unwrap();
        let server = serve_sequence(vec![
            redirect_to_self(),
            json_response(
                "200 OK",
                r#"{"req_result":true,"data":{"path":"/p","created":false,"commit_id":null},"err_message":""}"#,
            ),
        ]);
        assert_eq!(
            provision_request(&client, &server.url, "/p", Some("t")).unwrap(),
            "already exists /p"
        );
        assert_eq!(server.hits(), 2);

        let server = serve_sequence(vec![redirect_to_self()]);
        let err = provision_request(&client, &server.url, "/p", None).unwrap_err();
        assert!(err.to_string().contains("redirect"), "{err}");
        assert_eq!(server.hits(), MAX_REDIRECTS + 1);
    }

    #[test]
    fn provision_rejects_server_urls_it_cannot_use() {
        let client = http_client(Duration::from_secs(5), true).unwrap();
        for server in [
            "http://127.0.0.1:9/?x=1",
            "http://127.0.0.1:9/#frag",
            "mailto:ops@example.com",
        ] {
            let err = provision_request(&client, server, "/p", None).unwrap_err();
            assert!(
                err.to_string().starts_with("invalid --server URL"),
                "{server}: {err}"
            );
        }
    }

    #[test]
    fn token_is_read_from_the_environment_value_only() {
        assert_eq!(read_token(None).unwrap(), None);
        assert_eq!(read_token(Some(OsString::from(""))).unwrap(), None);
        assert_eq!(
            read_token(Some(OsString::from("abc"))).unwrap(),
            Some("abc".to_owned())
        );
        let err = read_token(Some(OsString::from("abc\r\nX-Evil: 1"))).unwrap_err();
        assert!(!err.to_string().contains("abc"), "{err}");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let err = read_token(Some(OsString::from_vec(vec![0x61, 0xff]))).unwrap_err();
            assert!(err.to_string().contains("UTF-8"), "{err}");
        }
    }

    #[test]
    fn provision_never_echoes_secrets() {
        let client = http_client(Duration::from_secs(5), true).unwrap();
        let token = "fu08-reflected-secret";
        // A JSON error that reflects the token is redacted.
        let server = serve(json_response(
            "403 Forbidden",
            &format!(
                r#"{{"req_result":false,"data":null,"err_message":"token {token} is not authorized"}}"#
            ),
        ));
        let err = provision_request(&client, &server.url, "/p", Some(token)).unwrap_err();
        assert_eq!(err.to_string(), "token *** is not authorized");
        // A non-JSON body is never echoed.
        let body = format!("upstream says {token}");
        let server = serve(format!(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        let err = provision_request(&client, &server.url, "/p", Some(token)).unwrap_err();
        assert!(!err.to_string().contains(token), "{err}");
        assert!(!err.to_string().contains("upstream"), "{err}");
        // Credentials in the URL are refused before any request.
        let err =
            provision_request(&client, "http://user:hunter2@127.0.0.1:9", "/p", None).unwrap_err();
        assert!(err.to_string().contains("credentials"), "{err}");
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn provision_bounds_the_response_body() {
        let client = http_client(Duration::from_secs(5), true).unwrap();
        let chunk = "x".repeat(16 * 1024);
        let mut response = String::from(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        );
        for _ in 0..8 {
            response.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
        }
        response.push_str("0\r\n\r\n");
        let server = serve(response);
        let err = provision_request(&client, &server.url, "/p", None).unwrap_err();
        assert!(err.to_string().contains("larger than"), "{err}");
    }

    #[test]
    fn provision_validates_the_success_contract() {
        let client = http_client(Duration::from_secs(5), true).unwrap();
        for body in [
            r#"{"req_result":false,"data":{"path":"/p","created":false,"commit_id":null},"err_message":""}"#,
            r#"{"req_result":true,"data":{"path":"/other","created":false,"commit_id":null},"err_message":""}"#,
            r#"{"req_result":true,"data":{"path":"/p","created":true,"commit_id":null},"err_message":""}"#,
            r#"{"req_result":true,"data":{"path":"/p","created":false,"commit_id":"abc"},"err_message":""}"#,
            r#"{"req_result":true,"data":null,"err_message":""}"#,
        ] {
            let server = serve(json_response("200 OK", body));
            assert!(
                provision_request(&client, &server.url, "/p", None).is_err(),
                "{body}"
            );
        }
    }

    /// With a proxy forced through the environment, a loopback server is still
    /// reached directly; the control client (no bypass) goes to the proxy.
    #[test]
    fn loopback_requests_skip_a_configured_proxy() {
        use crate::config::testing::{EnvVarGuard, env_lock};

        let lock = env_lock();
        let proxy = serve(json_response("502 Bad Gateway", "{}"));
        let server = serve(json_response(
            "200 OK",
            r#"{"req_result":true,"data":{"path":"/p","created":false,"commit_id":null},"err_message":""}"#,
        ));
        let _guards = [
            EnvVarGuard::set(&lock, "HTTP_PROXY", &proxy.url),
            EnvVarGuard::set(&lock, "http_proxy", &proxy.url),
            EnvVarGuard::remove(&lock, "NO_PROXY"),
            EnvVarGuard::remove(&lock, "no_proxy"),
            EnvVarGuard::remove(&lock, "ALL_PROXY"),
            EnvVarGuard::remove(&lock, "all_proxy"),
        ];
        let direct = http_client(Duration::from_secs(5), true).unwrap();
        assert_eq!(
            provision_request(&direct, &server.url, "/p", None).unwrap(),
            "already exists /p"
        );
        assert_eq!(proxy.hits(), 0);
        assert_eq!(server.hits(), 1);

        let proxied = http_client(Duration::from_secs(5), false).unwrap();
        assert!(provision_request(&proxied, &server.url, "/p", None).is_err());
        assert_eq!(proxy.hits(), 1, "control goes through the proxy");
        assert_eq!(server.hits(), 1);
    }

    #[test]
    fn loopback_servers_bypass_proxies() {
        for (server, loopback) in [
            ("http://127.0.0.1:9000", true),
            ("http://127.1.2.3", true),
            ("http://[::1]:9000", true),
            ("http://LOCALHOST:9000", true),
            ("https://mega2.example.com", false),
            ("http://10.0.0.1", false),
        ] {
            assert_eq!(
                is_loopback(&Url::parse(server).unwrap()),
                loopback,
                "{server}"
            );
        }
    }
}
