use std::{borrow::Cow, convert::Infallible, str::FromStr, sync::Arc};

use anyhow::Result;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Method, Request, Response, Uri},
    middleware::Next,
    response::IntoResponse,
};
use base64::Engine;
use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt};
use git_internal::hash::HashKind;
use http::header::AUTHORIZATION;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::{io::StreamReader, sync::CancellationToken};
use tower::ServiceExt;
use tower_http::decompression::RequestDecompression;

use crate::{
    api::oauth::{bearer_token_from_authorization_value, login_user_from_mono_access_token},
    ceres::{
        api_service::state::ProtocolApiState,
        pack::traces_repo::{resolve_trace_discovery, resolve_trace_fetch, resolve_trace_receive},
        protocol::{ServiceType, SmartSession, TransportProtocol, smart, v2},
        traces::{
            admission::TRACE_REQUEST_CAPACITY,
            auth::{PublishAuthority, TracesCaller},
            checkpoint_history::validate_checkpoint_history,
            ingest::decode_wire_pack,
            pack_encode::RawPackStream,
            publication::{PublicationProposal, prepare},
        },
    },
    common::errors::{MegaError, ProtocolError, TracesError},
    config::{PushAuth, PushTokenConfig},
    contract::{
        git_protocol::{
            InfoRefsParams, check_push_permission, check_upload_pack_access, lookup_push_token,
            path::{TracesLocator, ViewLocator, parse_git_protocol_path, parse_traces_locator},
            receive_pack_requires_http_auth, resolve_view_target,
        },
        policy::{builder::EntitySnapshot, enforcement::Enforcement},
    },
    jupiter::{
        service::{
            trace_fetch_service::fetch_raw_objects,
            trace_object_service::TraceObjectService,
            trace_publish_service::{PreparedScopeRequest, TracePublishService},
        },
        storage::{
            Storage,
            base_storage::StorageConnector,
            trace_stream_storage::{ExpectedOldTip, ScopeFacts},
            user_storage::AuthenticatedAccessToken,
        },
    },
};

#[derive(Clone)]
pub(crate) struct OriginalGitHttpUri(pub(crate) Uri);

#[derive(Clone)]
struct TraceContentEncoding(Option<HeaderValue>);

pub(crate) async fn reject_disabled_traces(
    State(storage): State<Storage>,
    mut request: Request<Body>,
    next: Next,
) -> Response<Body> {
    if (request.method() == Method::GET || request.method() == Method::POST)
        && !storage.config().traces.enabled
        && (reserved_traces_http_path(request.uri().path())
            || request
                .extensions()
                .get::<OriginalGitHttpUri>()
                .is_some_and(|original| reserved_traces_http_path(original.0.path())))
    {
        return ProtocolError::NotFound("traces locator not found".to_owned()).into_response();
    }
    if request.method() == Method::POST
        && parse_git_protocol_path(request.method(), request.uri().path())
            .map(|parsed| parsed.traces.is_some())
            .unwrap_or_else(|_| parse_traces_locator(request.uri().path()).is_err())
    {
        // The outer decompressor must not inspect traces bodies before authentication.
        let encoding = request.headers_mut().remove(http::header::CONTENT_ENCODING);
        request
            .extensions_mut()
            .insert(TraceContentEncoding(encoding));
    }
    next.run(request).await
}

fn reserved_traces_http_path(path: &str) -> bool {
    let raw = path.as_bytes();
    if reserved_traces_http_bytes(raw) {
        return true;
    }
    let decoded: Cow<'_, [u8]> = percent_encoding::percent_decode(raw).into();
    reserved_traces_http_bytes(&decoded)
}

fn reserved_traces_http_bytes(path: &[u8]) -> bool {
    if enters_traces_root(path) {
        return true;
    }
    ["/info/refs", "/git-upload-pack", "/git-receive-pack"]
        .iter()
        .filter_map(|suffix| path.strip_suffix(suffix.as_bytes()))
        .any(|repo| enters_traces_root(repo.strip_suffix(b".git").unwrap_or(repo)))
}

fn enters_traces_root(path: &[u8]) -> bool {
    let mut depth = 0usize;
    for segment in path.split(|byte| *byte == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => depth = depth.saturating_sub(1),
            b".traces" if depth == 0 => return true,
            _ => depth += 1,
        }
    }
    false
}

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
    view: Option<ViewLocator>,
    headers: &http::HeaderMap,
) -> Result<Response<Body>, ProtocolError> {
    let service_name = params
        .service
        .ok_or_else(|| ProtocolError::InvalidInput("missing service parameter".to_owned()))?;
    let service_type = ServiceType::from_str(&service_name)
        .map_err(|err| ProtocolError::InvalidInput(err.to_string()))?;
    if let Some(locator) = trace_http_locator(&repo_path)? {
        return trace_http_result(trace_info_refs(state, &locator, service_type, headers).await);
    }
    let mut session =
        SmartSession::from_state(repo_path, service_type, TransportProtocol::Http, state)?;
    match service_type {
        ServiceType::UploadPack => {
            let _ = git_http_auth(state, &mut session, headers).await?;
            if check_upload_pack_access(&state.storage.config().git, &session.auth)
                .await
                .is_err()
            {
                return auth_failed();
            }
            if let Some(locator) = &view {
                session.view = Some(resolve_view_target(state, locator).await?);
            }
        }
        ServiceType::ReceivePack => {
            if receive_pack_requires_http_auth(&state.storage.config().git)
                && !git_http_auth(state, &mut session, headers).await?
            {
                return auth_failed();
            }
            check_push_permission(state, &session.auth, &session.repo_path).await?;
        }
    }

    if is_v2_request(headers) && service_type == ServiceType::UploadPack {
        let pkt_line_stream = v2::build_v2_capability_advertisement(session.hash_kind);
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

/// Authenticates a Git HTTP request.
///
/// `push_auth = "token"`: constant-time lookup over `[[git.push_tokens]]`;
/// a hit sets the actor to the token name. Omitted `push_auth` keeps the
/// existing OAuth / UserStorage chain. Supports Bearer and Basic (password
/// field). Returns `Ok(true)` on a valid credential, `Ok(false)` when no
/// header is present or the credential misses, and `Err` on lookup failures.
async fn git_http_auth(
    state: &ProtocolApiState,
    pack_protocol: &mut SmartSession,
    headers: &http::HeaderMap,
) -> Result<bool, ProtocolError> {
    let Some(token) = presented_git_http_token(headers) else {
        return Ok(false);
    };

    let git = &state.storage.config().git;
    if git.push_auth == Some(PushAuth::Token) {
        let Some(matched) = lookup_push_token(&git.push_tokens, &token) else {
            return Ok(false);
        };
        pack_protocol.set_authenticated_user(matched.name.clone());
        return Ok(true);
    }

    let Some(user) =
        login_user_from_mono_access_token(&state.storage.user_storage(), &token).await?
    else {
        return Ok(false);
    };

    pack_protocol.set_authenticated_user(user.username);
    Ok(true)
}

fn presented_git_http_token(headers: &http::HeaderMap) -> Option<String> {
    let auth_header = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    bearer_token_from_authorization_value(auth_header)
        .map(str::to_owned)
        .or_else(|| basic_auth_password_from_authorization_value(auth_header))
}

const TRACE_COMMAND_BYTES: usize = 64 * 1024;
const TRACE_PKT_BYTES: usize = 65_520;
const TRACE_REF: &str = "refs/libra/traces";

fn trace_http_locator(path: &std::path::Path) -> Result<Option<TracesLocator>, ProtocolError> {
    parse_traces_locator(
        path.to_str()
            .ok_or(ProtocolError::Traces(TracesError::UnknownScope))?,
    )
}

fn trace_invalid() -> MegaError {
    TracesError::InvalidParameter.into()
}

fn trace_http_result(
    result: Result<Response<Body>, MegaError>,
) -> Result<Response<Body>, ProtocolError> {
    match result {
        Ok(response) => Ok(response),
        Err(MegaError::Traces(TracesError::Unauthenticated)) => auth_failed(),
        Err(MegaError::Traces(error)) => Err(ProtocolError::Traces(error)),
        // Database and transport diagnostics must not enter a public Git response.
        Err(_) => Err(ProtocolError::Traces(TracesError::QueryTimeout)),
    }
}

fn trace_response(body: Body, content_type: &'static str) -> Response<Body> {
    let mut response = Response::new(body);
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

enum TraceCredential {
    Static(PushTokenConfig),
    Mono(AuthenticatedAccessToken),
}

struct TraceHttpAuth {
    credential: TraceCredential,
    enforcement: Enforcement,
    snapshot: Option<Arc<EntitySnapshot>>,
}

impl TraceHttpAuth {
    fn caller(&self) -> TracesCaller<'_> {
        match &self.credential {
            TraceCredential::Static(token) => TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token,
            },
            TraceCredential::Mono(token) => TracesCaller::MonoAccessToken {
                token,
                enforcement: self.enforcement,
                snapshot: self.snapshot.as_deref(),
            },
        }
    }
}

async fn trace_http_auth(
    state: &ProtocolApiState,
    headers: &http::HeaderMap,
) -> Result<TraceHttpAuth, MegaError> {
    let token = presented_git_http_token(headers).ok_or(TracesError::Unauthenticated)?;
    let config = state.storage.config();
    let credential = if config.git.push_auth == Some(PushAuth::Token) {
        TraceCredential::Static(
            lookup_push_token(&config.git.push_tokens, &token)
                .ok_or(TracesError::Unauthenticated)?
                .clone(),
        )
    } else {
        TraceCredential::Mono(
            state
                .storage
                .user_storage()
                .authenticate_traces_access_token(&token)
                .await?
                .ok_or(TracesError::Unauthenticated)?,
        )
    };
    let snapshot = if state.entity_store.is_dirty() {
        None
    } else {
        state.entity_store.snapshot()
    };
    Ok(TraceHttpAuth {
        credential,
        enforcement: Enforcement::parse(&config.cedar.enforcement)
            .ok_or(TracesError::UnknownScope)?,
        snapshot,
    })
}

async fn trace_read_tip(
    state: &ProtocolApiState,
    locator: &TracesLocator,
    auth: &TraceHttpAuth,
) -> Result<(HashKind, Option<String>), MegaError> {
    let config = state.storage.config();
    let mono = state.storage.mono_storage();
    let db = mono.get_connection();
    let discovery = resolve_trace_discovery(
        db,
        &config.traces,
        locator,
        auth.caller(),
        ServiceType::UploadPack,
    )
    .await?;
    if discovery.scope_id().is_none() {
        return Ok((discovery.hash_kind(), None));
    }
    let grant = resolve_trace_fetch(
        db,
        &config.traces,
        locator,
        auth.caller(),
        Some(&state.entity_store),
    )
    .await?;
    Ok((grant.hash_kind(), grant.tip_oid().map(str::to_owned)))
}

async fn trace_info_refs(
    state: &ProtocolApiState,
    locator: &TracesLocator,
    service: ServiceType,
    headers: &http::HeaderMap,
) -> Result<Response<Body>, MegaError> {
    let auth = trace_http_auth(state, headers).await?;
    let (kind, tip) = match service {
        ServiceType::UploadPack => trace_read_tip(state, locator, &auth).await?,
        ServiceType::ReceivePack => {
            let mono = state.storage.mono_storage();
            let scope = resolve_trace_receive(
                mono.get_connection(),
                &state.storage.config().traces,
                locator,
                auth.caller(),
            )
            .await?;
            (scope.hash_kind, scope.tip_oid)
        }
    };
    let mut body = BytesMut::new();
    let content_type = match service {
        ServiceType::UploadPack => "application/x-git-upload-pack-advertisement",
        ServiceType::ReceivePack => "application/x-git-receive-pack-advertisement",
    };
    if service == ServiceType::UploadPack && is_v2_request(headers) {
        for line in ["version 2\n", "ls-refs\n", "fetch\n"] {
            smart::add_pkt_line_string(&mut body, line.to_owned());
        }
        smart::add_pkt_line_string(&mut body, format!("object-format={}\n", kind.as_str()));
    } else {
        let (service_name, capabilities) = match service {
            ServiceType::UploadPack => ("git-upload-pack", "allow-reachable-sha1-in-want"),
            ServiceType::ReceivePack => ("git-receive-pack", "report-status no-thin"),
        };
        smart::add_pkt_line_string(&mut body, format!("# service={service_name}\n"));
        body.extend_from_slice(b"0000");
        let (oid, reference) = match tip {
            Some(tip) => (tip, TRACE_REF),
            None => ("0".repeat(kind.hex_len()), "capabilities^{}"),
        };
        smart::add_pkt_line_string(
            &mut body,
            format!(
                "{oid} {reference}\0{capabilities} object-format={}\n",
                kind.as_str()
            ),
        );
    }
    body.extend_from_slice(b"0000");
    Ok(trace_response(Body::from(body.freeze()), content_type))
}

fn trace_oid(raw: &str, kind: HashKind) -> Result<String, MegaError> {
    if raw.len() != kind.hex_len() || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(trace_invalid());
    }
    Ok(raw.to_ascii_lowercase())
}

fn trace_capabilities<'a>(
    caps: impl Iterator<Item = &'a str>,
    kind: HashKind,
    allowed: &[&str],
    require_sha256_format: bool,
) -> Result<(), MegaError> {
    let mut format = None;
    for cap in caps {
        if let Some(value) = cap.strip_prefix("object-format=") {
            if format.replace(value).is_some() || value != kind.as_str() {
                return Err(trace_invalid());
            }
        } else if !allowed.contains(&cap) {
            return Err(trace_invalid());
        }
    }
    if format.is_none() && kind != HashKind::Sha1 && require_sha256_format {
        return Err(trace_invalid());
    }
    Ok(())
}

struct TraceReceiveCommand {
    old: String,
    new: String,
    report: bool,
}

async fn trace_receive_commands(
    reader: &mut (impl AsyncRead + Unpin),
    kind: HashKind,
) -> Result<Option<TraceReceiveCommand>, MegaError> {
    let mut consumed = 0usize;
    let mut command = None;
    loop {
        let mut header = [0; 4];
        reader
            .read_exact(&mut header)
            .await
            .map_err(|_| trace_invalid())?;
        consumed += header.len();
        if !header.iter().all(u8::is_ascii_hexdigit) {
            return Err(trace_invalid());
        }
        let length = usize::from_str_radix(
            std::str::from_utf8(&header).map_err(|_| trace_invalid())?,
            16,
        )
        .map_err(|_| trace_invalid())?;
        if consumed > TRACE_COMMAND_BYTES {
            return Err(TracesError::ResponseBudget.into());
        }
        if length == 0 {
            if command.is_none() {
                let mut trailing = [0];
                if reader
                    .read(&mut trailing)
                    .await
                    .map_err(|_| trace_invalid())?
                    != 0
                {
                    return Err(trace_invalid());
                }
            }
            return Ok(command);
        }
        if !(4..=TRACE_PKT_BYTES).contains(&length) || command.is_some() {
            return Err(trace_invalid());
        }
        consumed += length - 4;
        if consumed > TRACE_COMMAND_BYTES {
            return Err(TracesError::ResponseBudget.into());
        }
        let mut bytes = vec![0; length - 4];
        reader
            .read_exact(&mut bytes)
            .await
            .map_err(|_| trace_invalid())?;
        let line = std::str::from_utf8(&bytes).map_err(|_| trace_invalid())?;
        let line = line.strip_suffix('\n').unwrap_or(line);
        let (update, caps) = line.split_once('\0').unwrap_or((line, ""));
        let mut fields = update.split(' ');
        let old = trace_oid(fields.next().ok_or_else(trace_invalid)?, kind)?;
        let new = trace_oid(fields.next().ok_or_else(trace_invalid)?, kind)?;
        if fields.next() != Some(TRACE_REF)
            || fields.next().is_some()
            || new.bytes().all(|b| b == b'0')
        {
            return Err(trace_invalid());
        }
        trace_capabilities(
            caps.split_ascii_whitespace(),
            kind,
            &["report-status", "no-thin"],
            true,
        )?;
        command = Some(TraceReceiveCommand {
            old,
            new,
            report: caps
                .split_ascii_whitespace()
                .any(|cap| cap == "report-status"),
        });
    }
}

async fn trace_receive_pack(
    state: &ProtocolApiState,
    locator: &TracesLocator,
    request: Request<Body>,
) -> Result<Response<Body>, MegaError> {
    let auth = trace_http_auth(state, request.headers()).await?;
    let config = state.storage.config();
    let mono = state.storage.mono_storage();
    let db = mono.get_connection();
    let scope = resolve_trace_receive(db, &config.traces, locator, auth.caller()).await?;
    let mut admission = TRACE_REQUEST_CAPACITY.try_receive()?;
    if trace_content_encoding(&request).is_some_and(|value| value != "identity") {
        return Err(trace_invalid());
    }
    let input = request
        .into_body()
        .into_data_stream()
        .map_err(|_| std::io::Error::other("traces request interrupted"));
    let mut reader = StreamReader::new(input);
    let command = admission
        .read_commands(trace_receive_commands(&mut reader, scope.hash_kind))
        .await?;
    let Some(command) = command else {
        return Ok(trace_response(
            Body::empty(),
            "application/x-git-receive-pack-result",
        ));
    };
    let old = if command.old.bytes().all(|b| b == b'0') {
        "absent"
    } else {
        &command.old
    };
    let proposal = PublicationProposal {
        stream_id: scope.stream_id.clone(),
        expected_old_tip: if old == "absent" {
            ExpectedOldTip::Absent
        } else {
            ExpectedOldTip::At(command.old.clone())
        },
        new_tip: command.new.clone(),
        ref_names: vec![TRACE_REF.to_owned()],
        delete: false,
        idempotency_key: format!("git:{}:{old}:{}", scope.stream_id, command.new),
    };
    let objects = TraceObjectService::new(state.storage.git_service.obj_storage.clone());
    let decoded = decode_wire_pack(&mut admission, scope.hash_kind, reader).await?;
    let prepared =
        validate_checkpoint_history(prepare(db, &objects, decoded, proposal).await?).await?;
    let authority = PublishAuthority {
        caller: auth.caller(),
        principal_id: &scope.principal_id,
        target_id: &scope.target_id,
        authorized_path: &scope.target_path,
        expected_generation: scope.expected_generation,
        policy: Some(&state.entity_store),
    };
    let publisher = TracePublishService::new(state.storage.git_service.obj_storage.clone());
    if scope.first_scope {
        let facts = ScopeFacts {
            stream_id: &scope.stream_id,
            principal_id: &scope.principal_id,
            target_id: &scope.target_id,
            source_repo_id: &scope.source_repo_id,
            epoch_id: &scope.epoch_id,
            hash_kind: scope.hash_kind.as_str(),
        };
        publisher
            .publish_prepared_first_scope(
                &config.monorepo,
                db,
                &mono,
                &PreparedScopeRequest {
                    path: &scope.target_path,
                    facts: &facts,
                    prepared: &prepared,
                    authority: &authority,
                },
            )
            .await?;
    } else {
        publisher
            .publish_prepared(db, &prepared, &authority)
            .await?;
    }
    drop(prepared);
    drop(admission);
    let mut report = BytesMut::new();
    if command.report {
        smart::add_pkt_line_string(&mut report, "unpack ok\n".to_owned());
        smart::add_pkt_line_string(&mut report, format!("ok {TRACE_REF}\n"));
        report.extend_from_slice(b"0000");
    }
    Ok(trace_response(
        Body::from(report.freeze()),
        "application/x-git-receive-pack-result",
    ))
}

fn trace_content_encoding(request: &Request<Body>) -> Option<&HeaderValue> {
    request
        .extensions()
        .get::<TraceContentEncoding>()
        .and_then(|encoding| encoding.0.as_ref())
        .or_else(|| request.headers().get(http::header::CONTENT_ENCODING))
}

async fn trace_upload_body(mut request: Request<Body>) -> Result<Bytes, MegaError> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let encoding = trace_content_encoding(&request).cloned();
    if encoding
        .as_ref()
        .is_some_and(|value| value != "identity" && value != "gzip")
    {
        return Err(trace_invalid());
    }
    if let Some(encoding) = encoding {
        request
            .headers_mut()
            .insert(http::header::CONTENT_ENCODING, encoding);
    }
    let (parts, body) = request.into_parts();
    let mut input = body.into_data_stream();
    let exceeded = Arc::new(AtomicBool::new(false));
    let input_exceeded = exceeded.clone();
    let bounded = async_stream::stream! {
        let mut total = 0usize;
        while let Some(chunk) = input.next().await {
            match chunk {
                Ok(chunk) if chunk.len() <= TRACE_COMMAND_BYTES.saturating_sub(total) => {
                    total += chunk.len();
                    yield Ok::<_, std::io::Error>(chunk);
                }
                Ok(_) => {
                    input_exceeded.store(true, Ordering::Release);
                    yield Err(std::io::Error::other("traces request budget exceeded"));
                    return;
                }
                Err(_) => {
                    yield Err(std::io::Error::other("traces request interrupted"));
                    return;
                }
            }
        }
    };
    let request = Request::from_parts(parts, Body::from_stream(bounded));
    let decompressor =
        RequestDecompression::new(tower::service_fn(|request: Request<_>| async move {
            Ok::<_, Infallible>(Response::new(request.into_body()))
        }));
    let response = match decompressor.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    if !response.status().is_success() {
        return Err(trace_invalid());
    }
    let mut decoded = Body::new(response.into_body()).into_data_stream();
    let mut bytes = BytesMut::new();
    while let Some(chunk) = decoded.next().await {
        let chunk = chunk.map_err(|_| {
            if exceeded.load(Ordering::Acquire) {
                MegaError::Traces(TracesError::ResponseBudget)
            } else {
                trace_invalid()
            }
        })?;
        if chunk.len() > TRACE_COMMAND_BYTES.saturating_sub(bytes.len()) {
            return Err(TracesError::ResponseBudget.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes.freeze())
}

fn trace_packet(input: &mut Bytes) -> Result<smart::PktLine, MegaError> {
    let header = input.get(..4).ok_or_else(trace_invalid)?;
    if !header.iter().all(u8::is_ascii_hexdigit) {
        return Err(trace_invalid());
    }
    let length = usize::from_str_radix(
        std::str::from_utf8(header).map_err(|_| trace_invalid())?,
        16,
    )
    .map_err(|_| trace_invalid())?;
    match length {
        0..=2 => {
            let _ = input.split_to(4);
            Ok(match length {
                0 => smart::PktLine::Flush,
                1 => smart::PktLine::Delim,
                _ => smart::PktLine::ResponseEnd,
            })
        }
        4..=TRACE_PKT_BYTES if length <= input.len() => {
            let _ = input.split_to(4);
            Ok(smart::PktLine::Data(input.split_to(length - 4)))
        }
        _ => Err(trace_invalid()),
    }
}

fn trace_line(bytes: &[u8]) -> Result<&str, MegaError> {
    let line = std::str::from_utf8(bytes).map_err(|_| trace_invalid())?;
    let line = line.strip_suffix('\n').unwrap_or(line);
    if line.is_empty() || line.contains(['\0', '\n', '\r']) {
        return Err(trace_invalid());
    }
    Ok(line)
}

enum TraceUploadCommand {
    Empty,
    LsRefs(Vec<String>),
    Fetch {
        wants: Vec<String>,
        haves: Vec<String>,
        done: bool,
        v2: bool,
    },
}

fn trace_upload_command(mut input: Bytes, kind: HashKind) -> Result<TraceUploadCommand, MegaError> {
    let first = trace_packet(&mut input)?;
    let smart::PktLine::Data(first) = first else {
        return if first == smart::PktLine::Flush && input.is_empty() {
            Ok(TraceUploadCommand::Empty)
        } else {
            Err(trace_invalid())
        };
    };
    let first = trace_line(&first)?;
    if let Some(command) = first.strip_prefix("command=") {
        let mut caps = Vec::new();
        loop {
            match trace_packet(&mut input)? {
                smart::PktLine::Data(bytes) => caps.push(trace_line(&bytes)?.to_owned()),
                smart::PktLine::Delim => break,
                _ => return Err(trace_invalid()),
            }
        }
        trace_capabilities(caps.iter().map(String::as_str), kind, &[], true)?;
        if command != "ls-refs" && command != "fetch" {
            return Err(trace_invalid());
        }
        let mut prefixes = Vec::new();
        let mut wants = Vec::new();
        let mut haves = Vec::new();
        let mut done = false;
        loop {
            let bytes = match trace_packet(&mut input)? {
                smart::PktLine::Data(bytes) => bytes,
                smart::PktLine::Flush if input.is_empty() => break,
                _ => return Err(trace_invalid()),
            };
            let line = trace_line(&bytes)?;
            if command == "ls-refs" {
                match line {
                    "peel" | "symrefs" => {}
                    _ => prefixes.push(
                        line.strip_prefix("ref-prefix ")
                            .ok_or_else(trace_invalid)?
                            .to_owned(),
                    ),
                }
            } else if let Some(oid) = line.strip_prefix("want ") {
                wants.push(trace_oid(oid, kind)?);
            } else if let Some(oid) = line.strip_prefix("have ") {
                haves.push(trace_oid(oid, kind)?);
            } else {
                match line {
                    "done" if !done => done = true,
                    "thin-pack" | "ofs-delta" | "no-progress" | "include-tag" => {}
                    _ => return Err(trace_invalid()),
                }
            }
        }
        return if command == "ls-refs" {
            Ok(TraceUploadCommand::LsRefs(prefixes))
        } else if wants.is_empty() {
            Err(trace_invalid())
        } else {
            Ok(TraceUploadCommand::Fetch {
                wants,
                haves,
                done,
                v2: true,
            })
        };
    }
    let mut fields = first
        .strip_prefix("want ")
        .ok_or_else(trace_invalid)?
        .split(' ');
    let mut wants = vec![trace_oid(fields.next().ok_or_else(trace_invalid)?, kind)?];
    trace_capabilities(fields, kind, &["allow-reachable-sha1-in-want"], false)?;
    loop {
        match trace_packet(&mut input)? {
            smart::PktLine::Data(bytes) => wants.push(trace_oid(
                trace_line(&bytes)?
                    .strip_prefix("want ")
                    .ok_or_else(trace_invalid)?,
                kind,
            )?),
            smart::PktLine::Flush => break,
            _ => return Err(trace_invalid()),
        }
    }
    let mut haves = Vec::new();
    let mut done = false;
    while !input.is_empty() {
        match trace_packet(&mut input)? {
            smart::PktLine::Data(bytes) => {
                let line = trace_line(&bytes)?;
                if line == "done" && input.is_empty() {
                    done = true;
                } else {
                    haves.push(trace_oid(
                        line.strip_prefix("have ").ok_or_else(trace_invalid)?,
                        kind,
                    )?);
                }
            }
            smart::PktLine::Flush if input.is_empty() => {}
            _ => return Err(trace_invalid()),
        }
    }
    Ok(TraceUploadCommand::Fetch {
        wants,
        haves,
        done,
        v2: false,
    })
}

async fn trace_upload_pack(
    state: &ProtocolApiState,
    locator: &TracesLocator,
    request: Request<Body>,
) -> Result<Response<Body>, MegaError> {
    let auth = trace_http_auth(state, request.headers()).await?;
    let (kind, _) = trace_read_tip(state, locator, &auth).await?;
    let body = trace_upload_body(request).await?;
    let command = trace_upload_command(body, kind)?;
    let mut response = BytesMut::new();
    let (wants, haves, done, v2) = match command {
        TraceUploadCommand::Empty => {
            return Ok(trace_response(
                Body::from("0000"),
                "application/x-git-upload-pack-result",
            ));
        }
        TraceUploadCommand::LsRefs(prefixes) => {
            let (_, tip) = trace_read_tip(state, locator, &auth).await?;
            if let Some(tip) = tip
                && (prefixes.is_empty()
                    || prefixes.iter().any(|prefix| TRACE_REF.starts_with(prefix)))
            {
                smart::add_pkt_line_string(&mut response, format!("{tip} {TRACE_REF}\n"));
            }
            response.extend_from_slice(b"0000");
            return Ok(trace_response(
                Body::from(response.freeze()),
                "application/x-git-upload-pack-result",
            ));
        }
        TraceUploadCommand::Fetch {
            wants,
            haves,
            done,
            v2,
        } => (wants, haves, done, v2),
    };
    let permit = TRACE_REQUEST_CAPACITY.try_fetch()?;
    let mono = state.storage.mono_storage();
    let grant = resolve_trace_fetch(
        mono.get_connection(),
        &state.storage.config().traces,
        locator,
        auth.caller(),
        Some(&state.entity_store),
    )
    .await?;
    let kind = grant.hash_kind();
    let objects = TraceObjectService::new(state.storage.git_service.obj_storage.clone());
    let source =
        fetch_raw_objects(grant, &objects, &wants, &haves, CancellationToken::new()).await?;
    if !done {
        drop(source);
        drop(permit);
        if v2 {
            smart::add_pkt_line_string(&mut response, "acknowledgments\n".to_owned());
        }
        smart::add_pkt_line_string(&mut response, "NAK\n".to_owned());
        if v2 {
            response.extend_from_slice(b"0000");
        }
        return Ok(trace_response(
            Body::from(response.freeze()),
            "application/x-git-upload-pack-result",
        ));
    }
    let mut pack = RawPackStream::new(source, kind, permit)?;
    smart::add_pkt_line_string(
        &mut response,
        if v2 { "packfile\n" } else { "NAK\n" }.to_owned(),
    );
    let body = async_stream::stream! {
        yield Ok::<_, std::io::Error>(response.freeze());
        loop {
            match pack.next().await {
                Ok(Some(chunk)) if v2 => {
                    for payload in chunk.chunks(TRACE_PKT_BYTES - 5) {
                        let mut framed = BytesMut::with_capacity(payload.len() + 5);
                        framed.extend_from_slice(format!("{:04x}", payload.len() + 5).as_bytes());
                        framed.extend_from_slice(&[1]);
                        framed.extend_from_slice(payload);
                        yield Ok(framed.freeze());
                    }
                }
                Ok(Some(chunk)) => yield Ok(chunk),
                Ok(None) => {
                    if v2 { yield Ok(Bytes::from_static(b"0000")); }
                    break;
                }
                Err(_) => {
                    yield Err(std::io::Error::other("traces pack stream failed"));
                    break;
                }
            }
        }
    };
    Ok(trace_response(
        Body::from_stream(body),
        "application/x-git-upload-pack-result",
    ))
}

/// Maximum body size accepted for Git HTTP upload-pack / receive-pack requests.
/// This bounds memory consumption on the server and prevents unbounded buffering
/// of malformed or malicious requests. Monorepo initial pushes carry gigabytes
/// of pack data, so the limit is generous; it exists to cap the buffered
/// request, not to cap repository size.
const GIT_HTTP_MAX_BODY_BYTES: usize = 4 * 1024 * 1024 * 1024;

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
    view: Option<ViewLocator>,
) -> Result<Response<Body>, ProtocolError> {
    if let Some(locator) = trace_http_locator(&repo_path)? {
        return trace_http_result(trace_upload_pack(state, &locator, req).await);
    }
    let mut pack_protocol = SmartSession::from_state(
        repo_path,
        ServiceType::UploadPack,
        TransportProtocol::Http,
        state,
    )?;
    let _ = git_http_auth(state, &mut pack_protocol, req.headers()).await?;
    check_upload_pack_access(&state.storage.config().git, &pack_protocol.auth).await?;
    if let Some(locator) = &view {
        pack_protocol.view = Some(resolve_view_target(state, locator).await?);
    }
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
        if protocol_buf.ends_with(smart::PKT_LINE_END_MARKER) {
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
            let v2::V2FetchResponse {
                pack_data: mut send_pack_data,
                protocol_buf,
                has_packfile,
            } = v2::handle_v2_fetch(session, state, body).await?;

            let body_stream = async_stream::stream! {
                let mut protocol_buf = protocol_buf;
                if !has_packfile {
                    // Negotiation round: the `acknowledgments` section already
                    // closed the response with its flush packet.
                    yield Ok::<_, Infallible>(Bytes::copy_from_slice(&protocol_buf));
                    return;
                }
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
    if let Some(locator) = trace_http_locator(&repo_path)? {
        return trace_http_result(trace_receive_pack(state, &locator, req).await);
    }
    let mut pack_protocol = SmartSession::from_state(
        repo_path,
        ServiceType::ReceivePack,
        TransportProtocol::Http,
        state,
    )?;
    if receive_pack_requires_http_auth(&state.storage.config().git)
        && !git_http_auth(state, &mut pack_protocol, req.headers()).await?
    {
        return auth_failed();
    }
    check_push_permission(state, &pack_protocol.auth, &pack_protocol.repo_path).await?;
    let receive_request = match collect_body_data(req.into_body(), "receive-pack").await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, "receive-pack: reading request body failed");
            return Err(e);
        }
    };
    tracing::info!(
        bytes = receive_request.len(),
        "receive-pack: request body received"
    );

    // A body of exactly one flush-pkt is git's large-push probe
    // (remote-curl's probe_rpc posts "0000" with Content-Length: 4 before
    // committing to a chunked transfer). The probe expects HTTP 200 — a 400
    // makes the client abandon the push outright, so acknowledge with an
    // empty report; no ref was touched and no pack followed.
    if receive_request.as_ref() == smart::PKT_LINE_END_MARKER {
        tracing::info!("receive-pack: large-push probe acknowledged");
        let response = Response::builder()
            .body(Body::from(Bytes::from_static(smart::PKT_LINE_END_MARKER)))
            .map_err(|e| ProtocolError::InvalidInput(format!("failed to build response: {e}")))?;
        return add_default_header(
            String::from("application/x-git-receive-pack-result"),
            response,
        );
    }

    let (commands, pack_bytes) =
        match pack_protocol.split_receive_pack_request(receive_request.freeze()) {
            Ok(split) => split,
            Err(e) => {
                tracing::error!(error = %e, "receive-pack: request split failed");
                return Err(e);
            }
        };
    tracing::info!(
        commands = commands.len(),
        pack_bytes = pack_bytes.len(),
        "receive-pack: request split"
    );
    let report_status = pack_protocol
        .git_receive_pack_stream(state, commands, pack_bytes)
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

pub(crate) fn pack_rejected_response(message: &str) -> Response<Body> {
    let mut body = BytesMut::new();
    smart::add_pkt_line_string(&mut body, format!("ERR {message}\n"));

    let mut response = Response::new(Body::from(body.freeze()));
    response.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("application/x-git-upload-pack-result"),
    );
    response.headers_mut().insert(
        "Cache-Control",
        HeaderValue::from_static("no-cache, max-age=0, must-revalidate"),
    );
    response
}

#[cfg(test)]
mod tests {
    use futures::stream;
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[test]
    fn sha256_capability_negotiation_distinguishes_v0_upload() {
        let kind = HashKind::Sha256;
        assert!(trace_capabilities(std::iter::empty(), kind, &[], false).is_ok());
        assert!(trace_capabilities(["object-format=sha256"].into_iter(), kind, &[], false).is_ok());
        for capabilities in [
            vec!["object-format=sha1"],
            vec!["object-format=sha256", "object-format=sha256"],
            vec!["server-option=unknown"],
        ] {
            assert!(trace_capabilities(capabilities.into_iter(), kind, &[], false).is_err());
        }
        assert!(trace_capabilities(std::iter::empty(), kind, &[], true).is_err());
        assert!(trace_capabilities(["object-format=sha256"].into_iter(), kind, &[], true).is_ok());
    }

    #[tokio::test]
    async fn sha256_receive_command_requires_explicit_format() {
        let old = "0".repeat(64);
        let new = "a".repeat(64);
        for (capabilities, accepted) in [
            ("report-status", false),
            ("report-status object-format=sha256", true),
        ] {
            let mut command = BytesMut::new();
            smart::add_pkt_line_string(
                &mut command,
                format!("{old} {new} {TRACE_REF}\0{capabilities}\n"),
            );
            command.extend_from_slice(b"0000");
            let (mut writer, mut reader) = tokio::io::duplex(512);
            writer.write_all(&command).await.unwrap();
            drop(writer);
            let result = trace_receive_commands(&mut reader, HashKind::Sha256).await;
            assert_eq!(result.is_ok(), accepted, "capabilities {capabilities}");
            if accepted {
                assert!(result.unwrap().is_some());
            }
        }
    }

    #[test]
    fn disabled_traces_namespace_rejects_root_aliases() {
        for path in [
            "/.traces/by-scope/00000000-0000-4000-8000-000000000001.git/info/refs",
            "/.traces/by-path/Lw/self/agent/session/epoch/sha256.git/git-upload-pack",
            "/.traces",
            "/.traces.git/info/refs",
            "/.traces.git/git-upload-pack",
            "/.traces.git/git-receive-pack",
            "//.traces/by-scope/bad.git/info/refs",
            "/./.traces/",
            "/other/../.traces/",
            "/.traces/../ordinary/git-upload-pack",
            "//.traces/../ordinary/info/refs",
            "/%2etraces/by-scope/bad.git/info/refs",
            "/.%74races/git-receive-pack",
            "/%2Etraces/",
            "/.traces%2Fby-scope/bad",
            "/%2f.traces/",
            "/other/%2e%2e/.traces/",
            "/.traces/%FF/bad",
            "/.traces/%GG/bad",
            "/other/../.traces.git/git-receive-pack",
        ] {
            assert!(
                reserved_traces_http_path(path),
                "missed reserved path: {path}"
            );
        }
    }

    #[test]
    fn disabled_traces_namespace_preserves_ordinary_paths() {
        for path in [
            "/",
            "/info/refs",
            "/project/git-upload-pack",
            "/.traces-backup/repo.git/info/refs",
            "/.tracesx/repo.git/info/refs",
            "/project/.traces/repo.git/info/refs",
            "/project/%2etraces/repo.git/info/refs",
            "/.TRACES/repo.git/info/refs",
            "/.traces.git/ordinary.git/info/refs",
            "/.traces.git.git/info/refs",
            "/project/a/../.traces/repo.git/info/refs",
            "/%252etraces/repo.git/info/refs",
            "/.traces%ZZ/repo.git/info/refs",
            "/project/.traces/../../ordinary/git-upload-pack",
        ] {
            assert!(
                !reserved_traces_http_path(path),
                "changed ordinary path: {path}"
            );
        }
    }

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

    #[test]
    fn presented_git_http_token_reads_bearer_and_basic_password() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer static-token"),
        );
        assert_eq!(
            presented_git_http_token(&headers).as_deref(),
            Some("static-token")
        );

        let encoded = base64::engine::general_purpose::STANDARD.encode("ignored:basic-secret");
        let mut basic = http::HeaderMap::new();
        basic.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert_eq!(
            presented_git_http_token(&basic).as_deref(),
            Some("basic-secret")
        );

        assert!(presented_git_http_token(&http::HeaderMap::new()).is_none());
    }
}
