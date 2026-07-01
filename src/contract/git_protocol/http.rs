use std::{convert::Infallible, str::FromStr};

use anyhow::Result;
use axum::{
    body::Body,
    http::{HeaderValue, Request, Response},
};
use base64::Engine;
use bytes::{Bytes, BytesMut};
use futures::{StreamExt, stream};
use http::header::AUTHORIZATION;
use tokio::io::AsyncReadExt;

use crate::{
    api::oauth::{bearer_token_from_authorization_value, login_user_from_mono_access_token},
    ceres::{
        api_service::state::ProtocolApiState,
        protocol::{ServiceType, SmartSession, TransportProtocol, smart, v2},
    },
    common::errors::ProtocolError,
    contract::git_protocol::{InfoRefsParams, check_push_permission, check_upload_pack_access},
};

// # Discovering Reference
// HTTP clients that support the "smart" protocol (or both the "smart" and "dumb" protocols) MUST
// discover references by making a parameterized request for the info/refs file of the repository.
// The request MUST contain exactly one query parameter, service=$servicename,
// where $servicename MUST be the service name the client wishes to contact to complete the operation.
// The request MUST NOT contain additional query parameters.
fn is_v2_request(headers: &http::HeaderMap) -> bool {
    headers
        .get("Git-Protocol")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("version=2"))
        .unwrap_or(false)
}

pub async fn git_info_refs(
    state: &ProtocolApiState,
    params: InfoRefsParams,
    repo_path: std::path::PathBuf,
    headers: &http::HeaderMap,
) -> Result<Response<Body>, ProtocolError> {
    let service_name = params
        .service
        .ok_or_else(|| ProtocolError::InvalidInput("missing service parameter".to_owned()))?;
    let service_type = ServiceType::from_str(&service_name)
        .map_err(|err| ProtocolError::InvalidInput(err.to_string()))?;
    let mut session = SmartSession::new(repo_path, service_type, TransportProtocol::Http);
    match service_type {
        ServiceType::UploadPack => {
            let _ = git_http_auth(state, &mut session, headers).await?;
            if check_upload_pack_access(&state.storage.config().git, &session.auth)
                .await
                .is_err()
            {
                return auth_failed();
            }
        }
        ServiceType::ReceivePack => {
            if !git_http_auth(state, &mut session, headers).await? {
                return auth_failed();
            }
            check_push_permission(state, &session.auth, &session.repo_path).await?;
        }
    }

    if is_v2_request(headers) && service_type == ServiceType::UploadPack {
        let pkt_line_stream = v2::build_v2_capability_advertisement();
        let response = add_default_header(
            format!("application/x-{service_name}-advertisement"),
            Response::builder()
                .body(Body::from(pkt_line_stream.freeze()))
                .map_err(|e| {
                    ProtocolError::InvalidInput(format!("failed to build response: {e}"))
                })?,
        )?;
        return Ok(response);
    }

    let pkt_line_stream = session.git_info_refs(state).await?;

    let content_type = format!("application/x-{service_name}-advertisement");
    let response = add_default_header(
        content_type,
        Response::builder()
            .body(Body::from(pkt_line_stream.freeze()))
            .map_err(|e| ProtocolError::InvalidInput(format!("failed to build response: {e}")))?,
    )?;
    Ok(response)
}

fn auth_failed() -> Result<Response<Body>, ProtocolError> {
    let resp = Response::builder()
        .status(401)
        .header(
            http::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Mega\", Bearer realm=\"Mega\""),
        )
        .body(Body::empty())
        .map_err(|e| ProtocolError::InvalidInput(format!("failed to build response: {e}")))?;
    Ok(resp)
}

/// Parses Basic Auth header, returning the password (which is the token).
/// The username is ignored since we only care about the token in the password field.
fn basic_auth_password_from_authorization_value(value: &str) -> Option<String> {
    let stripped = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stripped.trim())
        .ok()?;
    let decoded_str = String::from_utf8(decoded).ok()?;
    // Basic auth format: "username:password"
    Some(decoded_str.split(':').nth(1)?.to_owned())
}

/// Uses [`crate::api::oauth::login_user_from_mono_access_token`] (same as [`crate::api::oauth::AccessTokenUser`]).
/// Supports both Bearer tokens and Basic Auth (with token as password).
/// Returns `Ok(true)` if a valid token was found and the user was authenticated,
/// `Ok(false)` if no auth header was present, and `Err` on lookup failures.
async fn git_http_auth(
    state: &ProtocolApiState,
    pack_protocol: &mut SmartSession,
    headers: &http::HeaderMap,
) -> Result<bool, ProtocolError> {
    let auth_header = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());

    // Try Bearer token first
    let token = auth_header.and_then(bearer_token_from_authorization_value);

    // If no Bearer token, try Basic Auth (token as password)
    let token = token
        .map(String::from)
        .or_else(|| auth_header.and_then(basic_auth_password_from_authorization_value));

    let Some(token) = token else {
        return Ok(false);
    };

    let Some(user) =
        login_user_from_mono_access_token(&state.storage.user_storage(), &token).await?
    else {
        return Ok(false);
    };

    pack_protocol.set_authenticated_user(user.username);
    Ok(true)
}

/// Maximum body size accepted for Git HTTP upload-pack / receive-pack requests.
/// This bounds memory consumption on the server and prevents unbounded buffering
/// of malformed or malicious requests. Pushes larger than this must use chunked
/// or other transfer mechanisms not implemented here.
const GIT_HTTP_MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

async fn collect_body_data(body: Body, operation: &str) -> Result<BytesMut, ProtocolError> {
    collect_body_data_with_limit(body, operation, GIT_HTTP_MAX_BODY_BYTES).await
}

async fn collect_body_data_with_limit(
    body: Body,
    operation: &str,
    max_bytes: usize,
) -> Result<BytesMut, ProtocolError> {
    let mut stream = body.into_data_stream();
    let mut acc = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if acc.len() + chunk.len() > max_bytes {
                    return Err(ProtocolError::TooLarge(format!(
                        "{operation} body exceeds maximum allowed size of {max_bytes} bytes"
                    )));
                }
                acc.extend_from_slice(&chunk);
            }
            Err(err) => {
                return Err(ProtocolError::InvalidInput(format!(
                    "failed to read {operation} body: {err}"
                )));
            }
        }
    }
    Ok(acc)
}

/// # Handles a Git upload pack request and prepares the response.
///
/// The function takes a `req` parameter representing the HTTP request received and a `pack_protocol`
/// parameter containing the configuration for the Git pack protocol.
///
/// The function extracts the request body into a `BytesMut` buffer by iterating over the chunks
/// of the request body using `body.next().await`. The chunks are concatenated into the `upload_request`
/// buffer.
///
/// The `pack_protocol` is then used to process the `upload_request` using the `git_upload_pack` method.
/// It returns the `send_pack_data` and `buf` containing the response data.
///
/// A response header is constructed using the `build_res_header` function with a content type of
/// "application/x-git-upload-pack-result". The response body channel is created using `Body::channel()`.
///
/// The `buf` is sent as the initial data using the `sender` to establish the response body.
///
/// A new task is spawned to send the remaining `send_pack_data` using the `send_pack` function.
///
/// Finally, the constructed response with the response body is returned.
pub async fn git_upload_pack(
    state: &ProtocolApiState,
    req: Request<Body>,
    repo_path: std::path::PathBuf,
) -> Result<Response<Body>, ProtocolError> {
    let mut pack_protocol =
        SmartSession::new(repo_path, ServiceType::UploadPack, TransportProtocol::Http);
    let _ = git_http_auth(state, &mut pack_protocol, req.headers()).await?;
    check_upload_pack_access(&state.storage.config().git, &pack_protocol.auth).await?;
    let upload_request = collect_body_data(req.into_body(), "upload-pack").await?;
    tracing::debug!("Receive bytes: <-------- {:?}", upload_request);

    let mut body = upload_request.freeze();
    if v2::is_v2_upload_pack_request(&mut body) {
        return handle_v2_upload_pack(state, &mut pack_protocol, &mut body).await;
    }

    let (mut send_pack_data, protocol_buf) =
        pack_protocol.git_upload_pack(state, &mut body).await?;

    let body_stream = async_stream::stream! {
        tracing::info!("send ack/nak message buf: --------> {:?}", &protocol_buf);
        yield Ok::<_, Infallible>(Bytes::copy_from_slice(&protocol_buf));
        while let Some(chunk) = send_pack_data.next().await {
            let mut reader = chunk.as_slice();
            loop {
                let mut temp = BytesMut::new();
                temp.reserve(65500);
                let length = match reader.read_buf(&mut temp).await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::error!(error = %e, "read error in upload-pack sideband stream");
                        break;
                    }
                };
                if length == 0 {
                    break;
                }
                let bytes_out = pack_protocol.build_side_band_format(temp, length);
                yield Ok::<_, Infallible>(bytes_out.freeze());
            }
        }
        let bytes_out = Bytes::from_static(smart::PKT_LINE_END_MARKER);
        tracing::info!("send back pkt-flush line '0000', actually: {:?}", bytes_out);
        yield Ok::<_, Infallible>(bytes_out);
    };
    let response = add_default_header(
        String::from("application/x-git-upload-pack-result"),
        Response::builder()
            .body(Body::from_stream(body_stream))
            .map_err(|e| ProtocolError::InvalidInput(format!("failed to build response: {e}")))?,
    )?;
    Ok(response)
}

async fn handle_v2_upload_pack(
    state: &ProtocolApiState,
    session: &mut SmartSession,
    body: &mut Bytes,
) -> Result<Response<Body>, ProtocolError> {
    let (command, _caps) = v2::parse_v2_command(body)?;

    match command.as_str() {
        "ls-refs" => {
            let refs = v2::handle_v2_ls_refs(session, state, body).await?;
            let response = add_default_header(
                String::from("application/x-git-upload-pack-result"),
                Response::builder()
                    .body(Body::from(refs.freeze()))
                    .map_err(|e| {
                        ProtocolError::InvalidInput(format!("failed to build response: {e}"))
                    })?,
            )?;
            Ok(response)
        }
        "fetch" => {
            let (mut send_pack_data, protocol_buf) =
                v2::handle_v2_fetch(session, state, body).await?;

            let body_stream = async_stream::stream! {
                let mut protocol_buf = protocol_buf;
                v2::add_packfile_section_header(&mut protocol_buf);
                yield Ok::<_, Infallible>(Bytes::copy_from_slice(&protocol_buf));
                while let Some(chunk) = send_pack_data.next().await {
                    let mut reader = chunk.as_slice();
                    loop {
                        let mut temp = BytesMut::new();
                        temp.reserve(65500);
                        let length = match reader.read_buf(&mut temp).await {
                            Ok(n) => n,
                            Err(e) => {
                                tracing::error!(error = %e, "read error in v2 upload-pack sideband stream");
                                break;
                            }
                        };
                        if length == 0 {
                            break;
                        }
                        let bytes_out = v2::build_packfile_data_packet(temp, length);
                        yield Ok::<_, Infallible>(bytes_out.freeze());
                    }
                }
                let bytes_out = Bytes::from_static(smart::PKT_LINE_END_MARKER);
                yield Ok::<_, Infallible>(bytes_out);
            };
            let response = add_default_header(
                String::from("application/x-git-upload-pack-result"),
                Response::builder()
                    .body(Body::from_stream(body_stream))
                    .map_err(|e| {
                        ProtocolError::InvalidInput(format!("failed to build response: {e}"))
                    })?,
            )?;
            Ok(response)
        }
        other => Err(ProtocolError::InvalidInput(format!(
            "unsupported v2 command: {other}"
        ))),
    }
}

/// Handles the Git receive-pack protocol for receiving and processing data from a client.
///
/// This asynchronous function processes an HTTP request to handle the Git "receive-pack" service,
/// which is used for receiving data when pushing changes to a Git repository. The function reads
/// data from the request body, processes it according to the Git smart protocol, and sends back
/// a response indicating the status of the operation.
///
/// # Parameters
/// - `req`: The incoming HTTP request containing the body stream with the Git data.
/// - `pack_protocol`: A mutable instance of `SmartProtocol` used to process the Git receive-pack protocol.
///
/// # Returns
/// A `Result` containing either:
/// - `Response<Body>`: The HTTP response with the result of the receive-pack operation.
/// - `(StatusCode, String)`: A tuple with an HTTP status code and an error message in case of failure.
pub async fn git_receive_pack(
    state: &ProtocolApiState,
    req: Request<Body>,
    repo_path: std::path::PathBuf,
) -> Result<Response<Body>, ProtocolError> {
    let mut pack_protocol =
        SmartSession::new(repo_path, ServiceType::ReceivePack, TransportProtocol::Http);
    if !git_http_auth(state, &mut pack_protocol, req.headers()).await? {
        return auth_failed();
    }
    check_push_permission(state, &pack_protocol.auth, &pack_protocol.repo_path).await?;
    let receive_request = collect_body_data(req.into_body(), "receive-pack").await?;

    let (commands, pack_bytes) =
        pack_protocol.split_receive_pack_request(receive_request.freeze())?;
    let pack_stream = stream::once(async { Ok(pack_bytes) });
    let report_status = pack_protocol
        .git_receive_pack_stream(state, commands, Box::pin(pack_stream))
        .await?;

    tracing::info!("report status:{:?}", report_status);
    let response = Response::builder()
        .body(Body::from(report_status))
        .map_err(|e| ProtocolError::InvalidInput(format!("failed to build response: {e}")))?;
    let response = add_default_header(
        String::from("application/x-git-receive-pack-result"),
        response,
    )?;
    Ok(response)
}

/// # Build Response headers for Smart Server.
/// Clients MUST NOT reuse or revalidate a cached response.
/// Servers MUST include sufficient Cache-Control headers to prevent caching of the response.
fn add_default_header<T>(
    content_type: String,
    mut response: Response<T>,
) -> Result<Response<T>, ProtocolError> {
    response.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_str(&content_type).map_err(|e| {
            ProtocolError::InvalidInput(format!("invalid content-type header: {e}"))
        })?,
    );
    response.headers_mut().insert(
        "Cache-Control",
        HeaderValue::from_static("no-cache, max-age=0, must-revalidate"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn collect_body_data_maps_stream_errors_to_protocol_error() {
        let body = Body::from_stream(stream::once(async {
            Err::<Bytes, std::io::Error>(std::io::Error::other("boom"))
        }));

        let err = collect_body_data(body, "upload-pack")
            .await
            .expect_err("body stream error should be mapped");

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("failed to read upload-pack body"));
    }

    #[tokio::test]
    async fn collect_body_data_rejects_oversized_bodies() {
        let err = collect_body_data_with_limit(
            Body::from(Bytes::from(vec![0u8; 1025])),
            "upload-pack",
            1024,
        )
        .await
        .expect_err("oversized body should be rejected");

        assert!(matches!(err, ProtocolError::TooLarge(_)));
        assert!(
            err.to_string()
                .contains("exceeds maximum allowed size of 1024 bytes")
        );
    }

    #[tokio::test]
    async fn collect_body_data_accepts_bodies_within_limit() {
        let data = collect_body_data_with_limit(
            Body::from(Bytes::from(vec![0u8; 1024])),
            "upload-pack",
            1024,
        )
        .await
        .expect("body within limit should be accepted");

        assert_eq!(data.len(), 1024);
    }

    #[test]
    fn auth_failed_returns_401_response() {
        let resp = auth_failed().unwrap();

        assert_eq!(resp.status(), 401);
        assert!(resp.headers().get(http::header::WWW_AUTHENTICATE).is_some());
    }

    #[test]
    fn add_default_header_rejects_invalid_content_type() {
        let response = Response::builder().body(()).unwrap();
        let err = add_default_header("bad\ncontent-type".to_string(), response).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("invalid content-type header"));
    }
}
