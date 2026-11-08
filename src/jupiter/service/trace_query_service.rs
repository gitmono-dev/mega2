//! Session and checkpoint query projection for MTR-53.

use chrono::{DateTime, Utc};

use crate::api_model::traces::CaptureQueryFilter;

/// One checkpoint with the metadata needed by the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedCapture {
    pub session_key: String,
    pub checkpoint_id: String,
    pub agent_kind: String,
    pub created_at: DateTime<Utc>,
    pub code_commit: Option<String>,
}

/// A session assembled from every matching checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProjection {
    pub session_key: String,
    pub checkpoints: Vec<ProjectedCapture>,
    /// Maximum `created_at` among matching checkpoints.
    pub capture_time: DateTime<Utc>,
}

pub fn capture_matches(capture: &ProjectedCapture, filter: &CaptureQueryFilter) -> bool {
    filter.matches_agent(&capture.agent_kind)
        && filter.matches_code_commit(capture.code_commit.as_deref())
        && filter.window.contains(capture.created_at)
}

pub fn filter_captures(
    captures: &[ProjectedCapture],
    filter: &CaptureQueryFilter,
) -> Vec<ProjectedCapture> {
    captures
        .iter()
        .filter(|capture| capture_matches(capture, filter))
        .cloned()
        .collect()
}

/// Group matching checkpoints by session and sort sessions by their newest
/// matching checkpoint, newest first. Session key breaks ties deterministically.
pub fn project_sessions(
    captures: &[ProjectedCapture],
    filter: &CaptureQueryFilter,
) -> Vec<SessionProjection> {
    let mut sessions = std::collections::BTreeMap::<String, Vec<ProjectedCapture>>::new();
    for capture in filter_captures(captures, filter) {
        sessions
            .entry(capture.session_key.clone())
            .or_default()
            .push(capture);
    }
    let mut projected = Vec::new();
    for (session_key, checkpoints) in sessions {
        let Some(capture_time) = checkpoints.iter().map(|capture| capture.created_at).max() else {
            continue;
        };
        projected.push(SessionProjection {
            session_key,
            checkpoints,
            capture_time,
        });
    }
    projected.sort_by(|left, right| {
        right
            .capture_time
            .cmp(&left.capture_time)
            .then_with(|| left.session_key.cmp(&right.session_key))
    });
    projected
}

/// Capabilities for this process. Readiness comes from the process-derived
/// index state, not a fresh catalog read. `/search` is not served by this
/// build, so `search_ready` stays false with a stable reason even when the
/// index itself is valid. MTR-20 removes that limit when it mounts `/search`.
pub fn read_capabilities(
    server_id: &str,
    index: &crate::jupiter::service::trace_search_index_service::IndexState,
    cjk_index_supported: bool,
) -> crate::api_model::traces::TracesCapabilities {
    use crate::{
        api_model::traces::TracesCapabilities,
        jupiter::service::trace_search_index_service::IndexState,
    };
    let reason = match index {
        IndexState::Ready => SEARCH_NOT_SERVED,
        IndexState::Unavailable(failure) => catalog_reason(failure),
    };
    let mut capabilities = TracesCapabilities::project(server_id, false, Some(reason.to_owned()));
    // The CJK capability is read from the deployed locale, so it is reported
    // as measured instead of being assumed from the index alone.
    capabilities.cjk_index_supported = cjk_index_supported;
    capabilities
}

/// Stable reason while the search route is not mounted.
pub const SEARCH_NOT_SERVED: &str = "search_not_served";

fn catalog_reason(
    failure: &crate::jupiter::storage::trace_search_index_storage::CatalogFailure,
) -> &'static str {
    use crate::jupiter::storage::trace_search_index_storage::CatalogFailure;
    match failure {
        CatalogFailure::Method => "index_method",
        CatalogFailure::Opclass => "index_opclass",
        CatalogFailure::Columns => "index_columns",
        CatalogFailure::IndexVersion => "index_version",
        CatalogFailure::NotValid => "index_invalid",
        CatalogFailure::NotReady => "index_not_ready",
        CatalogFailure::ExtensionVersion => "extension_version",
        CatalogFailure::SchemaOwner => "extension_owner",
        CatalogFailure::Missing => "index_missing",
        CatalogFailure::ExtensionMissing => "extension_missing",
        CatalogFailure::WrongNamespace => "extension_namespace",
        CatalogFailure::IndexPermissionDenied => "index_permission_denied",
        CatalogFailure::BuildTimedOut => "index_build_timeout",
        CatalogFailure::CatalogUnreadable => "catalog_unreadable",
    }
}

pub async fn resolve_registered_target(
    db: &sea_orm::DatabaseConnection,
    raw_path: &str,
    may_read: bool,
) -> Result<crate::api_model::traces::TargetResolve, TargetResolveError> {
    use crate::{
        api_model::traces::TargetResolve,
        jupiter::storage::trace_target_storage::{active_at, canonical_project_path},
    };
    let path = canonical_project_path(raw_path).map_err(|_| TargetResolveError::InvalidPath)?;
    if !may_read {
        return Err(TargetResolveError::UnknownScope);
    }
    match active_at(db, &path)
        .await
        .map_err(TargetResolveError::Database)?
    {
        Some(row) => Ok(TargetResolve {
            target_id: row.id,
            project_path: row.project_path,
            target_kind: row.target_kind,
        }),
        None => Err(TargetResolveError::UnknownScope),
    }
}

#[derive(Debug)]
pub enum TargetResolveError {
    InvalidPath,
    UnknownScope,
    Database(crate::common::errors::MegaError),
}

/// Why a stream directory read failed. Missing and unreadable targets match.
#[derive(Debug)]
pub enum StreamDirectoryError {
    UnknownScope,
    InvalidParameter,
    Database(crate::common::errors::MegaError),
}

fn stream_status(accepted: Option<&str>, indexed: Option<&str>) -> &'static str {
    match accepted {
        None => "empty",
        Some(tip) if indexed == Some(tip) => "indexed",
        Some(_) => "index_pending",
    }
}

/// Last `new_tip` and sequence of the contiguous `indexed` prefix starting at sequence 1.
fn contiguous_indexed_watermark(
    updates: &[crate::callisto::trace_ref_update::Model],
) -> (u64, Option<String>) {
    let mut seq = 0_u64;
    let mut tip = None;
    for (offset, row) in updates.iter().enumerate() {
        let Ok(offset) = i64::try_from(offset) else {
            break;
        };
        if row.ref_sequence != offset + 1 || row.state != "indexed" {
            break;
        }
        let Ok(next_seq) = u64::try_from(row.ref_sequence) else {
            break;
        };
        seq = next_seq;
        tip = Some(row.new_tip.clone());
    }
    (seq, tip)
}

/// Last `new_tip` of the contiguous `indexed` prefix starting at sequence 1.
fn contiguous_indexed_tip(updates: &[crate::callisto::trace_ref_update::Model]) -> Option<String> {
    contiguous_indexed_watermark(updates).1
}

/// Project stream metadata index coverage and capture validation state (MTR-63).
///
/// Watermarks and `index_state` are read from the durable stream and journal
/// rows; `unsupported`, `invalid`, `redacted`, and `missing_raw` come from the
/// validated checkpoint parse and schema results.
pub async fn read_stream_projection_state(
    db: &sea_orm::DatabaseConnection,
    stream_id: &str,
    validated_checkpoints: &[(
        crate::ceres::traces::parser::CheckpointParse,
        crate::ceres::traces::parser::CaptureSchema,
    )],
) -> Result<crate::api_model::traces::ReadProjectionState, StreamDirectoryError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

    use crate::{
        api_model::traces::ReadProjectionState,
        callisto::{trace_ref_update, trace_stream},
        ceres::traces::parser::{CaptureSchema, CheckpointParse},
    };

    let stream = trace_stream::Entity::find_by_id(stream_id.to_owned())
        .one(db)
        .await
        .map_err(|err| StreamDirectoryError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(StreamDirectoryError::UnknownScope)?;
    let updates = trace_ref_update::Entity::find()
        .filter(trace_ref_update::Column::StreamId.eq(stream_id))
        .order_by_asc(trace_ref_update::Column::RefSequence)
        .all(db)
        .await
        .map_err(|err| {
            StreamDirectoryError::Database(crate::common::errors::MegaError::from(err))
        })?;
    let accepted_seq = updates
        .last()
        .and_then(|row| u64::try_from(row.ref_sequence).ok())
        .unwrap_or(0);
    let accepted_watermark = stream
        .tip_oid
        .or_else(|| updates.last().map(|row| row.new_tip.clone()));
    let (indexed_seq, indexed_watermark) = contiguous_indexed_watermark(&updates);

    let mut unsupported = 0_u64;
    let mut invalid = 0_u64;
    let mut any_redacted = false;
    let mut any_missing_raw = false;
    for (parse, schema) in validated_checkpoints {
        if matches!(parse, CheckpointParse::Invalid { .. }) {
            invalid = invalid.saturating_add(1);
            continue;
        }
        match schema {
            CaptureSchema::Invalid { .. } => {
                invalid = invalid.saturating_add(1);
            }
            CaptureSchema::Unsupported => {
                unsupported = unsupported.saturating_add(1);
            }
            CaptureSchema::Supported {
                redacted,
                missing_raw,
                ..
            } => {
                any_redacted |= *redacted;
                any_missing_raw |= !*redacted && *missing_raw;
            }
        }
    }

    Ok(ReadProjectionState::project(
        (accepted_seq, accepted_watermark),
        (indexed_seq, indexed_watermark),
        unsupported,
        invalid,
        any_redacted,
        any_missing_raw,
    ))
}

/// One persisted session with the metadata the sessions API projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedSession {
    pub session_id: String,
    pub session_key: String,
    pub checkpoint_count: u64,
    pub capture_time: Option<chrono::DateTime<chrono::Utc>>,
    pub accepted_watermark: Option<String>,
    pub indexed_watermark: Option<String>,
    pub code_link: Option<String>,
}

/// Errors returned when a session read fails.
#[derive(Debug)]
pub enum SessionReadError {
    UnknownScope,
    InvalidParameter,
    Database(crate::common::errors::MegaError),
}

/// Sessions of one stream, ordered by session key, with the fixed page budget.
///
/// The scope snapshot is the stream itself and the cursor is a keyset over the
/// session key. `may_read` is the traces Read decision for that stream.
pub async fn list_stream_sessions(
    db: &sea_orm::DatabaseConnection,
    stream_id: &str,
    request: &crate::api_model::traces::SessionRequest,
    may_read: bool,
) -> Result<crate::api_model::traces::SessionPage, SessionReadError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

    use crate::{
        api_model::traces::{
            STREAM_DIRECTORY_SCHEMA_VERSION, SessionPage, SessionSummary, session_cursor,
        },
        callisto::{
            trace_checkpoint, trace_code_link, trace_ref_update, trace_session, trace_stream,
        },
        config::TracesProtocolLimits,
    };
    if !may_read {
        return Err(SessionReadError::UnknownScope);
    }
    if request.limit == 0 || request.limit > TracesProtocolLimits::METADATA_LIMIT_MAX {
        return Err(SessionReadError::InvalidParameter);
    }
    let stream = trace_stream::Entity::find_by_id(stream_id.to_owned())
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;

    let mut query =
        trace_session::Entity::find().filter(trace_session::Column::StreamId.eq(stream_id));
    if let Some(session) = &request.session {
        query = query.filter(trace_session::Column::SessionKey.eq(session));
    }
    if let Some(after) = &request.after_key {
        query = query.filter(trace_session::Column::SessionKey.gt(after));
    }
    let rows = query
        .order_by_asc(trace_session::Column::SessionKey)
        .limit(request.limit.saturating_add(1))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;

    let updates = trace_ref_update::Entity::find()
        .filter(trace_ref_update::Column::StreamId.eq(stream_id))
        .order_by_asc(trace_ref_update::Column::RefSequence)
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    let (indexed_seq, indexed_watermark) = contiguous_indexed_watermark(&updates);
    let accepted_watermark = stream.tip_oid.clone();

    let checkpoints = trace_checkpoint::Entity::find()
        .filter(trace_checkpoint::Column::StreamId.eq(stream_id))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    let links = trace_code_link::Entity::find()
        .filter(trace_code_link::Column::StreamId.eq(stream_id))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;

    let mut items = Vec::new();
    for row in &rows {
        let count = checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.stream_id == row.id)
            .count() as u64;
        let code_link = links
            .iter()
            .find_map(|link| link.code_oid.clone())
            .or_else(|| Some("unresolved".to_owned()));
        items.push(ProjectedSession {
            session_id: row.id.clone(),
            session_key: row.session_key.clone(),
            checkpoint_count: count,
            capture_time: None,
            accepted_watermark: accepted_watermark.clone(),
            indexed_watermark: if indexed_seq > 0 {
                indexed_watermark.clone()
            } else {
                None
            },
            code_link,
        });
    }
    let next_cursor = if items.len() > usize::try_from(request.limit).unwrap_or(usize::MAX) {
        items.pop();
        items
            .last()
            .map(|session| session_cursor(stream_id, request, &session.session_key))
    } else {
        None
    };
    Ok(SessionPage {
        schema_version: STREAM_DIRECTORY_SCHEMA_VERSION,
        items: items
            .into_iter()
            .map(|session| SessionSummary {
                session_id: session.session_id,
                session_key: session.session_key,
                checkpoint_count: session.checkpoint_count,
                capture_time: session.capture_time.map(|time| time.to_rfc3339()),
                accepted_watermark: session.accepted_watermark,
                indexed_watermark: session.indexed_watermark,
                code_link: session.code_link,
            })
            .collect(),
        next_cursor,
    })
}

/// One session of one stream, with the validated capture completeness.
pub async fn read_stream_session(
    db: &sea_orm::DatabaseConnection,
    stream_id: &str,
    session_key: &str,
    may_read: bool,
) -> Result<crate::api_model::traces::SessionDetail, SessionReadError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

    use crate::{
        api_model::traces::SessionDetail,
        callisto::{
            trace_checkpoint, trace_code_link, trace_ref_update, trace_session, trace_stream,
        },
    };
    if !may_read {
        return Err(SessionReadError::UnknownScope);
    }
    let stream = trace_stream::Entity::find_by_id(stream_id.to_owned())
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;
    let session = trace_session::Entity::find()
        .filter(trace_session::Column::StreamId.eq(stream_id))
        .filter(trace_session::Column::SessionKey.eq(session_key))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;

    let updates = trace_ref_update::Entity::find()
        .filter(trace_ref_update::Column::StreamId.eq(stream_id))
        .order_by_asc(trace_ref_update::Column::RefSequence)
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    let (indexed_seq, indexed_watermark) = contiguous_indexed_watermark(&updates);
    let checkpoints = trace_checkpoint::Entity::find()
        .filter(trace_checkpoint::Column::StreamId.eq(stream_id))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    let count = checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.stream_id == session.id)
        .count() as u64;
    let code_link = trace_code_link::Entity::find()
        .filter(trace_code_link::Column::StreamId.eq(stream_id))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .into_iter()
        .find_map(|link| link.code_oid)
        .or_else(|| Some("unresolved".to_owned()));

    let index_caught_up =
        indexed_seq > 0 && stream.tip_oid.is_some() && indexed_watermark == stream.tip_oid;
    let completeness = crate::api_model::traces::CaptureCompleteness::from_validated_checkpoint(
        true, true, false, false, false,
    );
    Ok(SessionDetail {
        session_id: session.id,
        session_key: session.session_key,
        checkpoint_count: count,
        capture_time: None,
        accepted_watermark: stream.tip_oid,
        indexed_watermark: if indexed_seq > 0 {
            indexed_watermark
        } else {
            None
        },
        code_link,
        completeness: if index_caught_up {
            completeness
        } else {
            crate::api_model::traces::CaptureCompleteness {
                complete: false,
                redacted: completeness.redacted,
                missing_raw: false,
                status: "index_pending".to_owned(),
            }
        },
    })
}

/// Checkpoints of one session, ordered by checkpoint key, with the fixed budget.
pub async fn list_session_checkpoints(
    db: &sea_orm::DatabaseConnection,
    stream_id: &str,
    session_id: &str,
    request: &crate::api_model::traces::CheckpointRequest,
    may_read: bool,
) -> Result<crate::api_model::traces::CheckpointPage, SessionReadError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

    use crate::{
        api_model::traces::{
            CheckpointPage, CheckpointSummary, STREAM_DIRECTORY_SCHEMA_VERSION, checkpoint_cursor,
        },
        callisto::{trace_checkpoint, trace_code_link, trace_resource, trace_session},
        config::TracesProtocolLimits,
    };
    if !may_read {
        return Err(SessionReadError::UnknownScope);
    }
    if request.limit == 0 || request.limit > TracesProtocolLimits::METADATA_LIMIT_MAX {
        return Err(SessionReadError::InvalidParameter);
    }
    let session = trace_session::Entity::find()
        .filter(trace_session::Column::Id.eq(session_id))
        .filter(trace_session::Column::StreamId.eq(stream_id))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;

    let mut query =
        trace_checkpoint::Entity::find().filter(trace_checkpoint::Column::StreamId.eq(stream_id));
    if let Some(after) = &request.after_key {
        query = query.filter(trace_checkpoint::Column::CheckpointKey.gt(after));
    }
    let rows = query
        .order_by_asc(trace_checkpoint::Column::CheckpointKey)
        .limit(request.limit.saturating_add(1))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;

    let resources = trace_resource::Entity::find()
        .filter(trace_resource::Column::StreamId.eq(stream_id))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    let links = trace_code_link::Entity::find()
        .filter(trace_code_link::Column::StreamId.eq(stream_id))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;

    let mut items = Vec::new();
    for row in &rows {
        let mut owned = resources
            .iter()
            .filter(|resource| resource.checkpoint_id == row.id)
            .collect::<Vec<_>>();
        owned.sort_by_key(|resource| resource.ordinal);
        let code_link = links
            .iter()
            .find(|link| link.checkpoint_id == row.id)
            .and_then(|link| link.code_oid.clone());
        items.push(CheckpointSummary {
            checkpoint_id: row.id.clone(),
            checkpoint_key: row.checkpoint_key.clone(),
            session_id: session.id.clone(),
            original_tree: row.original_tree.clone(),
            resource_count: owned.len() as u64,
            byte_start: owned.first().map(|resource| resource.byte_start),
            byte_end: owned.last().map(|resource| resource.byte_end),
            code_link,
            completeness: crate::api_model::traces::CaptureCompleteness::from_validated_checkpoint(
                true, true, false, false, false,
            ),
        });
    }
    let next_cursor = if items.len() > usize::try_from(request.limit).unwrap_or(usize::MAX) {
        items.pop();
        items
            .last()
            .map(|checkpoint| checkpoint_cursor(stream_id, session_id, &checkpoint.checkpoint_key))
    } else {
        None
    };
    Ok(CheckpointPage {
        schema_version: STREAM_DIRECTORY_SCHEMA_VERSION,
        items,
        next_cursor,
    })
}

/// One checkpoint of one session, with its resource range and code link.
pub async fn read_session_checkpoint(
    db: &sea_orm::DatabaseConnection,
    stream_id: &str,
    session_id: &str,
    checkpoint_id: &str,
    may_read: bool,
) -> Result<crate::api_model::traces::CheckpointSummary, SessionReadError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    use crate::{
        api_model::traces::CheckpointSummary,
        callisto::{trace_checkpoint, trace_code_link, trace_resource, trace_session},
    };
    if !may_read {
        return Err(SessionReadError::UnknownScope);
    }
    let session = trace_session::Entity::find()
        .filter(trace_session::Column::Id.eq(session_id))
        .filter(trace_session::Column::StreamId.eq(stream_id))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;
    let checkpoint = trace_checkpoint::Entity::find_by_id(checkpoint_id.to_owned())
        .filter(trace_checkpoint::Column::StreamId.eq(stream_id))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;
    let mut resources = trace_resource::Entity::find()
        .filter(trace_resource::Column::StreamId.eq(stream_id))
        .filter(trace_resource::Column::CheckpointId.eq(checkpoint.id.clone()))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    resources.sort_by_key(|resource| resource.ordinal);
    let code_link = trace_code_link::Entity::find()
        .filter(trace_code_link::Column::StreamId.eq(stream_id))
        .filter(trace_code_link::Column::CheckpointId.eq(checkpoint.id.clone()))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .and_then(|link| link.code_oid);
    Ok(CheckpointSummary {
        checkpoint_id: checkpoint.id,
        checkpoint_key: checkpoint.checkpoint_key,
        session_id: session.id,
        original_tree: checkpoint.original_tree,
        resource_count: resources.len() as u64,
        byte_start: resources.first().map(|resource| resource.byte_start),
        byte_end: resources.last().map(|resource| resource.byte_end),
        code_link,
        completeness: crate::api_model::traces::CaptureCompleteness::from_validated_checkpoint(
            true, true, false, false, false,
        ),
    })
}

/// The transcript text of one checkpoint resource.
pub async fn read_session_checkpoint_transcript(
    db: &sea_orm::DatabaseConnection,
    stream_id: &str,
    session_id: &str,
    checkpoint_id: &str,
    request: &crate::api_model::traces::TranscriptRequest,
    may_read: bool,
) -> Result<crate::api_model::traces::TranscriptPage, SessionReadError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

    use crate::{
        api_model::traces::{TranscriptPage, transcript_cursor},
        callisto::{
            trace_checkpoint, trace_fragment, trace_fragment::membership, trace_resource,
            trace_session,
        },
    };
    if !may_read {
        return Err(SessionReadError::UnknownScope);
    }
    let session = trace_session::Entity::find()
        .filter(trace_session::Column::Id.eq(session_id))
        .filter(trace_session::Column::StreamId.eq(stream_id))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;
    let checkpoint = trace_checkpoint::Entity::find_by_id(checkpoint_id.to_owned())
        .filter(trace_checkpoint::Column::StreamId.eq(stream_id))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;

    let mut memberships = membership::Entity::find()
        .filter(membership::Column::StreamId.eq(stream_id))
        .filter(membership::Column::CheckpointId.eq(checkpoint.id.clone()))
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    memberships.sort_by_key(|row| row.resource_id.clone());
    let Some(first) = memberships.first().cloned() else {
        return Err(SessionReadError::UnknownScope);
    };
    let resources = trace_resource::Entity::find()
        .filter(trace_resource::Column::StreamId.eq(stream_id))
        .filter(trace_resource::Column::CheckpointId.eq(checkpoint.id.clone()))
        .order_by_asc(trace_resource::Column::Ordinal)
        .all(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?;
    let resource = resources
        .into_iter()
        .find(|row| row.id == first.resource_id)
        .ok_or(SessionReadError::UnknownScope)?;

    let fragment = trace_fragment::Entity::find_by_id(first.fragment_id.clone())
        .filter(trace_fragment::Column::StreamId.eq(stream_id))
        .one(db)
        .await
        .map_err(|err| SessionReadError::Database(crate::common::errors::MegaError::from(err)))?
        .ok_or(SessionReadError::UnknownScope)?;

    let body = fragment.body;
    let bytes = body.as_bytes();
    let start = usize::try_from(request.byte_offset)
        .unwrap_or(bytes.len())
        .min(bytes.len());
    let budget = usize::try_from(request.max_bytes).unwrap_or(0);
    let mut end = start.saturating_add(budget).min(bytes.len());
    while end > start && !body.is_char_boundary(end) {
        end -= 1;
    }
    let text = body.get(start..end).unwrap_or_default().to_owned();
    let truncated = end < bytes.len();
    Ok(TranscriptPage {
        schema_version: crate::api_model::traces::STREAM_DIRECTORY_SCHEMA_VERSION,
        checkpoint_id: checkpoint.id,
        resource_id: resource.id,
        content_digest: fragment.content_digest,
        byte_start: resource.byte_start,
        byte_end: resource.byte_end,
        text,
        truncated,
        next_cursor: if truncated {
            Some(transcript_cursor(
                stream_id,
                &session.id,
                checkpoint_id,
                u64::try_from(end).unwrap_or(0),
            ))
        } else {
            None
        },
    })
}

/// Streams of one active target. `may_read` is the traces Read decision for that target.
pub async fn list_target_streams(
    db: &sea_orm::DatabaseConnection,
    target_id: &str,
    request: &crate::api_model::traces::StreamDirectoryRequest,
    may_read: bool,
) -> Result<crate::api_model::traces::StreamDirectory, StreamDirectoryError> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

    use crate::{
        api_model::traces::{
            STREAM_DIRECTORY_SCHEMA_VERSION, StreamDirectory, StreamDirectoryItem,
        },
        callisto::{trace_ref_update, trace_stream, trace_target},
        config::TracesProtocolLimits,
    };
    if !may_read {
        return Err(StreamDirectoryError::UnknownScope);
    }
    if request.limit == 0 || request.limit > TracesProtocolLimits::METADATA_LIMIT_MAX {
        return Err(StreamDirectoryError::InvalidParameter);
    }
    let target = trace_target::Entity::find_by_id(target_id.to_owned())
        .one(db)
        .await
        .map_err(|err| {
            StreamDirectoryError::Database(crate::common::errors::MegaError::from(err))
        })?;
    if target.as_ref().is_none_or(|row| !row.active) {
        return Err(StreamDirectoryError::UnknownScope);
    }
    let mut query =
        trace_stream::Entity::find().filter(trace_stream::Column::TargetId.eq(target_id));
    if let Some(source) = &request.source {
        query = query.filter(trace_stream::Column::SourceRepoId.eq(source));
    }
    if let Some(owner) = &request.owner {
        query = query.filter(trace_stream::Column::PrincipalId.eq(owner));
    }
    if let Some(epoch) = &request.epoch {
        query = query.filter(trace_stream::Column::EpochId.eq(epoch));
    }
    if let Some(after) = &request.after_id {
        query = query.filter(trace_stream::Column::Id.gt(after));
    }
    let mut rows = query
        .order_by_asc(trace_stream::Column::Id)
        .limit(request.limit.saturating_add(1))
        .all(db)
        .await
        .map_err(|err| {
            StreamDirectoryError::Database(crate::common::errors::MegaError::from(err))
        })?;
    let next_cursor = if rows.len() > usize::try_from(request.limit).unwrap_or(usize::MAX) {
        rows.pop();
        rows.last().map(|row| {
            crate::api_model::traces::stream_directory_cursor(target_id, request, &row.id)
        })
    } else {
        None
    };
    let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
    let updates = if ids.is_empty() {
        Vec::new()
    } else {
        trace_ref_update::Entity::find()
            .filter(trace_ref_update::Column::StreamId.is_in(ids))
            .order_by_asc(trace_ref_update::Column::RefSequence)
            .all(db)
            .await
            .map_err(|err| {
                StreamDirectoryError::Database(crate::common::errors::MegaError::from(err))
            })?
    };
    let items = rows
        .into_iter()
        .map(|row| {
            let mut mine: Vec<_> = updates
                .iter()
                .filter(|update| update.stream_id == row.id)
                .cloned()
                .collect();
            mine.sort_by_key(|update| update.ref_sequence);
            let indexed = contiguous_indexed_tip(&mine);
            let status = stream_status(row.tip_oid.as_deref(), indexed.as_deref());
            StreamDirectoryItem {
                stream_id: row.id,
                target_id: row.target_id,
                owner: row.principal_id,
                source: row.source_repo_id,
                epoch: row.epoch_id,
                hash_kind: row.hash_kind,
                accepted_tip: row.tip_oid,
                indexed_tip: indexed,
                status: status.to_owned(),
            }
        })
        .collect();
    Ok(StreamDirectory {
        schema_version: STREAM_DIRECTORY_SCHEMA_VERSION,
        items,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn capture(
        session: &str,
        checkpoint: &str,
        agent: &str,
        seconds: i64,
        code: &str,
    ) -> ProjectedCapture {
        ProjectedCapture {
            session_key: session.to_owned(),
            checkpoint_id: checkpoint.to_owned(),
            agent_kind: agent.to_owned(),
            created_at: Utc.timestamp_opt(seconds, 0).unwrap(),
            code_commit: Some(code.to_owned()),
        }
    }

    fn make_filter(
        agent: Option<&str>,
        code: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> CaptureQueryFilter {
        CaptureQueryFilter::new(
            agent.map(str::to_owned),
            code.map(str::to_owned),
            since,
            until,
        )
        .unwrap()
    }

    #[test]
    fn capture_query_filters_match_agent_code_and_half_open_interval() {
        let rows = [
            capture("s1", "c1", "claude_code", 100, &"a".repeat(40)),
            capture("s1", "c2", "other", 200, &"b".repeat(40)),
            capture("s2", "c3", "claude_code", 300, &"b".repeat(40)),
        ];
        let filter = make_filter(Some("claude_code"), Some(&"b".repeat(40)), None, None);
        assert!(capture_matches(&rows[2], &filter));
        assert!(!capture_matches(&rows[0], &filter));
        assert!(!capture_matches(&rows[1], &filter));

        let window = make_filter(
            Some("claude_code"),
            None,
            Some("1970-01-01T00:01:40Z"),
            Some("1970-01-01T00:06:40Z"),
        );
        assert!(capture_matches(&rows[2], &window));
        assert!(!capture_matches(&rows[1], &window));
        let until_only = make_filter(
            Some("claude_code"),
            None,
            None,
            Some("1970-01-01T00:05:00Z"),
        );
        assert!(!capture_matches(&rows[2], &until_only));
    }

    #[test]
    fn capture_query_filters_include_session_by_any_checkpoint_and_sort_by_max_time() {
        let rows = [
            capture("older", "o1", "claude_code", 100, &"a".repeat(40)),
            capture("newer", "n1", "claude_code", 300, &"a".repeat(40)),
            capture("older", "o2", "claude_code", 200, &"a".repeat(40)),
            capture("excluded", "x1", "other", 50, &"a".repeat(40)),
        ];
        let sessions = project_sessions(&rows, &make_filter(Some("claude_code"), None, None, None));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_key, "newer");
        assert_eq!(sessions[1].session_key, "older");
        assert_eq!(sessions[1].capture_time, Utc.timestamp_opt(200, 0).unwrap());
        assert!(
            !sessions
                .iter()
                .any(|session| session.session_key == "excluded")
        );

        let filtered = project_sessions(&rows, &make_filter(Some("claude_code"), None, None, None));
        assert_eq!(filtered[1].checkpoints.len(), 2);
    }

    #[tokio::test]
    async fn target_resolve_reads_the_active_row_and_does_not_insert() {
        use sea_orm::EntityTrait;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = crate::jupiter::tests::test_db_connection(temp_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .unwrap();
        crate::jupiter::storage::trace_target_storage::insert_active(
            &db,
            "native",
            "/project/app",
            None,
        )
        .await
        .unwrap();
        let before = crate::callisto::trace_target::Entity::find()
            .all(&db)
            .await
            .unwrap();
        let found = super::resolve_registered_target(&db, "/project/app", true)
            .await
            .unwrap();
        assert_eq!(found.target_id, before[0].id);
        assert_eq!(found.project_path, "/project/app");
        assert!(
            super::resolve_registered_target(&db, "/missing", true)
                .await
                .is_err()
        );
        assert!(
            super::resolve_registered_target(&db, "/project/app", false)
                .await
                .is_err()
        );
        let after = crate::callisto::trace_target::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after[0].id, before[0].id);
    }

    #[test]
    fn capabilities_readiness_never_advertises_unserved_search() {
        use crate::jupiter::{
            service::trace_search_index_service::IndexState,
            storage::trace_search_index_storage::CatalogFailure,
        };
        let ready = super::read_capabilities("id", &IndexState::Ready, false);
        assert!(!ready.search_ready);
        assert!(!ready.bounded_scan_supported);
        assert!(!ready.cjk_index_supported);
        assert_eq!(
            ready.search_unavailable_reason.as_deref(),
            Some(super::SEARCH_NOT_SERVED)
        );
        let missing = super::read_capabilities(
            "id",
            &IndexState::Unavailable(CatalogFailure::Missing),
            false,
        );
        assert!(!missing.search_ready);
        assert!(!missing.cjk_index_supported);
        assert_eq!(
            missing.search_unavailable_reason.as_deref(),
            Some("index_missing")
        );
    }

    #[test]
    fn capabilities_report_the_measured_cjk_locale_capability() {
        use crate::jupiter::service::trace_search_index_service::IndexState;
        let ready = super::read_capabilities("id", &IndexState::Ready, true);
        assert!(ready.cjk_index_supported);
        // The CJK capability never turns the search surface on by itself.
        assert!(!ready.search_ready);
        assert!(!ready.bounded_scan_supported);
    }

    #[tokio::test]
    async fn stream_directory_pages_filters_and_index_status() {
        use chrono::Utc;
        use sea_orm::{ActiveModelTrait, Set};
        use uuid::Uuid;

        use crate::{api_model::traces::StreamDirectoryRequest, config::TracesProtocolLimits};

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = crate::jupiter::tests::test_db_connection(temp_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .unwrap();
        let now = Utc::now().fixed_offset();
        let target = crate::jupiter::storage::trace_target_storage::insert_active(
            &db,
            "native",
            "/project/app",
            None,
        )
        .await
        .unwrap();
        let other = crate::jupiter::storage::trace_target_storage::insert_active(
            &db,
            "native",
            "/project/other",
            None,
        )
        .await
        .unwrap();
        async fn principal(db: &sea_orm::DatabaseConnection) -> String {
            let id = Uuid::new_v4().to_string();
            crate::callisto::trace_principal::ActiveModel {
                id: Set(id.clone()),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .unwrap();
            id
        }
        let owner_a = principal(&db).await;
        let owner_b = principal(&db).await;
        async fn stream(
            db: &sea_orm::DatabaseConnection,
            target_id: &str,
            owner: &str,
            source: &str,
            epoch: &str,
            tip: Option<&str>,
        ) -> String {
            crate::callisto::trace_stream::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                principal_id: Set(owner.to_owned()),
                target_id: Set(target_id.to_owned()),
                source_repo_id: Set(source.to_owned()),
                epoch_id: Set(epoch.to_owned()),
                hash_kind: Set("sha1".to_owned()),
                tip_oid: Set(tip.map(str::to_owned)),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .unwrap()
            .id
        }
        async fn journal(
            db: &sea_orm::DatabaseConnection,
            stream_id: &str,
            sequence: i64,
            tip: &str,
            state: &str,
        ) {
            crate::callisto::trace_ref_update::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                stream_id: Set(stream_id.to_owned()),
                ref_sequence: Set(sequence),
                idempotency_key: Set(Uuid::new_v4().to_string()),
                old_tip: Set(None),
                new_tip: Set(tip.to_owned()),
                state: Set(state.to_owned()),
                claim_until: Set(None),
                attempt_count: Set(0),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .unwrap();
        }
        let epoch_a = Uuid::new_v4().to_string();
        let epoch_b = Uuid::new_v4().to_string();
        let epoch_c = Uuid::new_v4().to_string();
        let pending_tip = "b".repeat(40);
        let indexed_tip = "a".repeat(40);
        let pending = stream(
            &db,
            &target.id,
            &owner_a,
            "src-a",
            &epoch_a,
            Some(&pending_tip),
        )
        .await;
        journal(&db, &pending, 1, &indexed_tip, "indexed").await;
        journal(&db, &pending, 2, &pending_tip, "pending").await;
        let done_tip = "c".repeat(40);
        let done = stream(
            &db,
            &target.id,
            &owner_b,
            "src-b",
            &epoch_b,
            Some(&done_tip),
        )
        .await;
        journal(&db, &done, 1, &done_tip, "indexed").await;
        let empty = stream(&db, &target.id, &owner_a, "src-a", &epoch_c, None).await;
        let _foreign = stream(&db, &other.id, &owner_a, "src-a", &epoch_a, None).await;
        let _ = now;

        let all = super::list_target_streams(
            &db,
            &target.id,
            &StreamDirectoryRequest {
                source: None,
                owner: None,
                epoch: None,
                limit: 10,
                after_id: None,
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(all.items.len(), 3);
        assert!(all.items.iter().all(|item| item.target_id == target.id));
        let pending_item = all
            .items
            .iter()
            .find(|item| item.stream_id == pending)
            .unwrap();
        assert_eq!(
            pending_item.indexed_tip.as_deref(),
            Some(indexed_tip.as_str())
        );
        assert_eq!(pending_item.status, "index_pending");
        let done_item = all
            .items
            .iter()
            .find(|item| item.stream_id == done)
            .unwrap();
        assert_eq!(done_item.status, "indexed");
        assert_eq!(done_item.owner, owner_b);
        let empty_item = all
            .items
            .iter()
            .find(|item| item.stream_id == empty)
            .unwrap();
        assert_eq!(empty_item.status, "empty");
        assert!(empty_item.accepted_tip.is_none());

        let by_source = super::list_target_streams(
            &db,
            &target.id,
            &StreamDirectoryRequest {
                source: Some("src-b".to_owned()),
                owner: None,
                epoch: None,
                limit: 10,
                after_id: None,
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(by_source.items.len(), 1);
        assert_eq!(by_source.items[0].stream_id, done);
        let by_owner = super::list_target_streams(
            &db,
            &target.id,
            &StreamDirectoryRequest {
                source: None,
                owner: Some(owner_b.clone()),
                epoch: None,
                limit: 10,
                after_id: None,
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(by_owner.items[0].stream_id, done);
        let by_epoch = super::list_target_streams(
            &db,
            &target.id,
            &StreamDirectoryRequest {
                source: None,
                owner: None,
                epoch: Some(epoch_c.clone()),
                limit: 10,
                after_id: None,
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(by_epoch.items[0].stream_id, empty);

        for _index in 0..TracesProtocolLimits::METADATA_LIMIT_DEFAULT + 1 {
            stream(
                &db,
                &target.id,
                &owner_a,
                "page",
                &Uuid::new_v4().to_string(),
                None,
            )
            .await;
        }
        let page = super::list_target_streams(
            &db,
            &target.id,
            &StreamDirectoryRequest {
                source: Some("page".to_owned()),
                owner: None,
                epoch: None,
                limit: TracesProtocolLimits::METADATA_LIMIT_DEFAULT,
                after_id: None,
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            page.items.len() as u64,
            TracesProtocolLimits::METADATA_LIMIT_DEFAULT
        );
        let cursor = page.next_cursor.expect("next page");
        let admitted = crate::api_model::traces::admit_stream_directory_query(
            &target.id,
            crate::api_model::traces::StreamDirectoryQuery {
                source: Some("page".to_owned()),
                owner: None,
                epoch: None,
                limit: Some(TracesProtocolLimits::METADATA_LIMIT_DEFAULT),
                cursor: Some(cursor),
            },
        )
        .unwrap();
        let rest = super::list_target_streams(&db, &target.id, &admitted, true)
            .await
            .unwrap();
        assert_eq!(rest.items.len(), 1);
        assert!(rest.next_cursor.is_none());
        assert!(
            super::list_target_streams(
                &db,
                &target.id,
                &StreamDirectoryRequest {
                    source: None,
                    owner: None,
                    epoch: None,
                    limit: TracesProtocolLimits::METADATA_LIMIT_MAX + 1,
                    after_id: None,
                },
                true,
            )
            .await
            .is_err()
        );
        assert!(
            super::list_target_streams(
                &db,
                &target.id,
                &StreamDirectoryRequest {
                    source: None,
                    owner: None,
                    epoch: None,
                    limit: 10,
                    after_id: None,
                },
                false,
            )
            .await
            .is_err()
        );
        assert!(
            super::list_target_streams(
                &db,
                &Uuid::new_v4().to_string(),
                &StreamDirectoryRequest {
                    source: None,
                    owner: None,
                    epoch: None,
                    limit: 10,
                    after_id: None,
                },
                true,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn read_projection_state_tracks_watermarks_counts_lagging_empty_and_redacted_only() {
        use chrono::Utc;
        use sea_orm::{ActiveModelTrait, Set};
        use uuid::Uuid;

        use crate::ceres::traces::parser::{
            CheckpointLocation, CheckpointParse, InvalidCheckpoint, classify_capture,
            parse_checkpoint,
        };

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = crate::jupiter::tests::test_db_connection(temp_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .unwrap();
        let now = Utc::now().fixed_offset();
        let owner = Uuid::new_v4().to_string();
        crate::callisto::trace_principal::ActiveModel {
            id: Set(owner.clone()),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        let target = crate::jupiter::storage::trace_target_storage::insert_active(
            &db,
            "native",
            "/project/read-projection",
            None,
        )
        .await
        .unwrap();
        let tip_1 = "a".repeat(40);
        let tip_2 = "b".repeat(40);
        let stream_id = crate::callisto::trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(owner),
            target_id: Set(target.id),
            source_repo_id: Set("source-projection".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(Some(tip_2.clone())),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap()
        .id;
        for (seq, tip, state) in [
            (1_i64, tip_1.as_str(), "indexed"),
            (2_i64, tip_2.as_str(), "pending"),
        ] {
            crate::callisto::trace_ref_update::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                stream_id: Set(stream_id.clone()),
                ref_sequence: Set(seq),
                idempotency_key: Set(Uuid::new_v4().to_string()),
                old_tip: Set(None),
                new_tip: Set(tip.to_owned()),
                state: Set(state.to_owned()),
                claim_until: Set(None),
                attempt_count: Set(0),
                created_at: Set(now),
            }
            .insert(&db)
            .await
            .unwrap();
        }

        // AC-2, AC-3, AC-4, AC-7: lagging index with empty results is not complete.
        let lagging = super::read_stream_projection_state(&db, &stream_id, &[])
            .await
            .unwrap();
        assert_eq!(lagging.accepted_seq, 2);
        assert_eq!(lagging.accepted_watermark.as_deref(), Some(tip_2.as_str()));
        assert_eq!(lagging.indexed_seq, 1);
        assert_eq!(lagging.indexed_watermark.as_deref(), Some(tip_1.as_str()));
        assert_eq!(lagging.index_state, "index_pending");
        assert!(!lagging.coverage_complete);
        assert!(!lagging.completeness.complete);

        // Advance ref_sequence 2 to indexed and verify AC-1, AC-5, AC-6, AC-8.
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let pending_row = crate::callisto::trace_ref_update::Entity::find()
            .filter(crate::callisto::trace_ref_update::Column::StreamId.eq(&stream_id))
            .filter(crate::callisto::trace_ref_update::Column::RefSequence.eq(2_i64))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut active: crate::callisto::trace_ref_update::ActiveModel = pending_row.into();
        active.state = Set("indexed".to_owned());
        active.update(&db).await.unwrap();

        let roles = crate::ceres::traces::parser::CheckpointRoles {
            metadata: Some("c".repeat(40)),
            manifest: Some("d".repeat(40)),
        };
        let redacted_meta = br#"{"schema_version":2,"checkpoint_id":"cp01","agent_kind":"claude_code","created_at":1700000000}"#;
        let redacted_manifest = br#"{"schema_version":1,"checkpoint_id":"cp01","entries":{"lifecycle_events":{"schema_version":2},"transcript":{"redaction":"redacted"}}}"#;
        let parsed_ok = parse_checkpoint(
            CheckpointLocation {
                checkpoint_id: "cp01".to_owned(),
                tree_oid: "e".repeat(40),
            },
            &roles,
            redacted_meta,
            redacted_manifest,
        );
        let schema_redacted = classify_capture(redacted_meta, redacted_manifest);
        let redacted_state = super::read_stream_projection_state(
            &db,
            &stream_id,
            &[(parsed_ok.clone(), schema_redacted.clone())],
        )
        .await
        .unwrap();
        assert_eq!(redacted_state.index_state, "indexed");
        assert!(redacted_state.coverage_complete);
        assert!(redacted_state.completeness.complete);
        assert!(redacted_state.completeness.redacted);
        assert!(!redacted_state.completeness.missing_raw);

        let unsupported_meta = br#"{"schema_version":99,"checkpoint_id":"cp01"}"#;
        let schema_unsupported = classify_capture(unsupported_meta, redacted_manifest);
        let invalid_meta = br#"{"schema_version":2,"checkpoint_id":"cp01","agent_kind":"claude_code","created_at":1700000000000}"#;
        let schema_invalid = classify_capture(invalid_meta, redacted_manifest);
        let layout_invalid = CheckpointParse::Invalid {
            key: "cp02".to_owned(),
            tree_oid: "f".repeat(40),
            reason: InvalidCheckpoint::MissingManifest,
        };
        let mixed = super::read_stream_projection_state(
            &db,
            &stream_id,
            &[
                (parsed_ok.clone(), schema_redacted.clone()),
                (parsed_ok.clone(), schema_unsupported),
                (parsed_ok, schema_invalid),
                (layout_invalid, schema_redacted),
            ],
        )
        .await
        .unwrap();
        assert_eq!(mixed.unsupported, 1);
        assert_eq!(mixed.invalid, 2);
        assert!(!mixed.coverage_complete);
        assert!(!mixed.completeness.complete);
        assert!(!mixed.completeness.missing_raw);
    }
}
