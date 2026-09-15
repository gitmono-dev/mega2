//! Metadata-only committed-event projection and static target filters (WH-10).

use bytes::Bytes;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    common::errors::MegaError,
    config::{StorageEventsTargetConfig, validate::validate_storage_events_canonical_path},
};

pub const MAX_ENVELOPE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    RepoPush,
    OciManifestPublished,
    LfsObjectUploaded,
    LfsMediaFinalized,
    AgentCaptureEventsCommitted,
    AgentCaptureCheckpointCommitted,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepoPush => "repo.push",
            Self::OciManifestPublished => "oci.manifest.published",
            Self::LfsObjectUploaded => "lfs.object.uploaded",
            Self::LfsMediaFinalized => "lfs.media.finalized",
            Self::AgentCaptureEventsCommitted => "agent_capture.events.committed",
            Self::AgentCaptureCheckpointCommitted => "agent_capture.checkpoint.committed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Git,
    Oci,
    Lfs,
    AgentCapture,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Oci => "oci",
            Self::Lfs => "lfs",
            Self::AgentCapture => "agent_capture",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    Git {
        repo_path: String,
    },
    Oci {
        oci_repository: String,
    },
    LfsUnscoped,
    Media {
        repo_path: String,
    },
    AgentCapture {
        tenant_id: String,
        repo_path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventData {
    RepoPush {
        push_id: String,
        operation_id: String,
        ref_name: String,
        old_oid: String,
        requested_oid: String,
        landed_oid: String,
    },
    OciManifestPublished {
        digest: String,
        reference: String,
        media_type: String,
        size: u64,
    },
    LfsObjectUploaded {
        oid: String,
        size: u64,
    },
    LfsMediaFinalized {
        oid: String,
        size: u64,
        manifest_id: String,
    },
    AgentCaptureEventsCommitted {
        capture_id: String,
        receipt_id: String,
        new_event_count: u64,
        stream_kind: String,
        generation: u64,
        completeness: String,
    },
    AgentCaptureCheckpointCommitted {
        capture_id: String,
        checkpoint_id: String,
        completeness: String,
        raw_committed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedEvent {
    pub event_id: Uuid,
    pub event_type: EventType,
    pub occurred_at: u64,
    pub source: EventSource,
    pub scope: EventScope,
    pub data: EventData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionDrop {
    Size,
    InvalidEvent,
    InvalidScope,
}

pub fn validate_canonical_path(path: &str) -> Result<(), MegaError> {
    validate_storage_events_canonical_path(path)
}

pub fn path_filter_matches(filter: &str, path: &str) -> bool {
    if filter == "/" {
        return true;
    }
    path == filter || path.starts_with(&format!("{filter}/"))
}

pub fn project(event: &CommittedEvent) -> Result<Bytes, ProjectionDrop> {
    if !scope_matches_type(event) {
        return Err(ProjectionDrop::InvalidScope);
    }
    validate_scope_and_data(event)?;
    let envelope = Envelope::from_event(event)?;
    let body = serde_json::to_vec(&envelope).map_err(|_| ProjectionDrop::InvalidEvent)?;
    if body.len() > MAX_ENVELOPE_BYTES {
        return Err(ProjectionDrop::Size);
    }
    Ok(Bytes::from(body))
}

pub fn select_targets<'a>(
    event: &CommittedEvent,
    targets: &'a [StorageEventsTargetConfig],
) -> Vec<&'a StorageEventsTargetConfig> {
    targets
        .iter()
        .filter(|target| target_matches(event, target))
        .collect()
}

fn target_matches(event: &CommittedEvent, target: &StorageEventsTargetConfig) -> bool {
    if !target
        .events
        .iter()
        .any(|item| item == event.event_type.as_str())
    {
        return false;
    }
    match &event.scope {
        EventScope::Git { repo_path } | EventScope::Media { repo_path } => {
            let filters = if matches!(event.scope, EventScope::Media { .. }) {
                &target.lfs_paths
            } else {
                &target.git_paths
            };
            !filters.is_empty()
                && validate_canonical_path(repo_path).is_ok()
                && filters
                    .iter()
                    .any(|filter| path_filter_matches(filter, repo_path))
        }
        EventScope::Oci { oci_repository } => {
            !target.oci_repositories.is_empty()
                && target
                    .oci_repositories
                    .iter()
                    .any(|name| name == oci_repository)
        }
        EventScope::LfsUnscoped => target.include_unscoped_lfs,
        EventScope::AgentCapture {
            tenant_id,
            repo_path,
        } => {
            !target.agent_tenants.is_empty()
                && !target.agent_repo_paths.is_empty()
                && target.agent_tenants.iter().any(|item| item == tenant_id)
                && validate_canonical_path(repo_path).is_ok()
                && target
                    .agent_repo_paths
                    .iter()
                    .any(|filter| path_filter_matches(filter, repo_path))
        }
    }
}

fn scope_matches_type(event: &CommittedEvent) -> bool {
    matches!(
        (event.event_type, event.source, &event.scope),
        (
            EventType::RepoPush,
            EventSource::Git,
            EventScope::Git { .. }
        ) | (
            EventType::OciManifestPublished,
            EventSource::Oci,
            EventScope::Oci { .. }
        ) | (
            EventType::LfsObjectUploaded,
            EventSource::Lfs,
            EventScope::LfsUnscoped
        ) | (
            EventType::LfsMediaFinalized,
            EventSource::Lfs,
            EventScope::Media { .. }
        ) | (
            EventType::AgentCaptureEventsCommitted,
            EventSource::AgentCapture,
            EventScope::AgentCapture { .. }
        ) | (
            EventType::AgentCaptureCheckpointCommitted,
            EventSource::AgentCapture,
            EventScope::AgentCapture { .. }
        )
    )
}

fn validate_scope_and_data(event: &CommittedEvent) -> Result<(), ProjectionDrop> {
    match &event.scope {
        EventScope::Git { repo_path } | EventScope::Media { repo_path } => {
            validate_canonical_path(repo_path).map_err(|_| ProjectionDrop::InvalidScope)?;
            require_nonempty(repo_path)?;
        }
        EventScope::Oci { oci_repository } => require_nonempty(oci_repository)?,
        EventScope::LfsUnscoped => {}
        EventScope::AgentCapture {
            tenant_id,
            repo_path,
        } => {
            require_nonempty(tenant_id)?;
            require_nonempty(repo_path)?;
            validate_canonical_path(repo_path).map_err(|_| ProjectionDrop::InvalidScope)?;
        }
    }
    match &event.data {
        EventData::RepoPush {
            push_id,
            operation_id,
            ref_name,
            old_oid,
            requested_oid,
            landed_oid,
        } => {
            require_nonempty(push_id)?;
            require_nonempty(operation_id)?;
            require_nonempty(ref_name)?;
            require_oid(old_oid)?;
            require_oid(requested_oid)?;
            require_oid(landed_oid)?;
        }
        EventData::OciManifestPublished {
            digest,
            reference,
            media_type,
            size: _,
        } => {
            require_nonempty(digest)?;
            require_nonempty(reference)?;
            require_nonempty(media_type)?;
        }
        EventData::LfsObjectUploaded { oid, size: _ } => require_oid(oid)?,
        EventData::LfsMediaFinalized {
            oid,
            size: _,
            manifest_id,
        } => {
            require_oid(oid)?;
            require_nonempty(manifest_id)?;
        }
        EventData::AgentCaptureEventsCommitted {
            capture_id,
            receipt_id,
            stream_kind,
            completeness,
            ..
        } => {
            require_nonempty(capture_id)?;
            require_nonempty(receipt_id)?;
            require_nonempty(stream_kind)?;
            require_nonempty(completeness)?;
        }
        EventData::AgentCaptureCheckpointCommitted {
            capture_id,
            checkpoint_id,
            completeness,
            ..
        } => {
            require_nonempty(capture_id)?;
            require_nonempty(checkpoint_id)?;
            require_nonempty(completeness)?;
        }
    }
    Ok(())
}

fn require_nonempty(value: &str) -> Result<(), ProjectionDrop> {
    if value.is_empty() || value.len() > 4096 {
        Err(ProjectionDrop::InvalidEvent)
    } else {
        Ok(())
    }
}

fn require_oid(value: &str) -> Result<(), ProjectionDrop> {
    require_nonempty(value)?;
    if value.bytes().all(|b| b.is_ascii_hexdigit())
        && value.bytes().all(|b| !b.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(ProjectionDrop::InvalidEvent)
    }
}

#[derive(Serialize)]
struct Envelope {
    schema_version: u8,
    event_id: String,
    event_type: &'static str,
    occurred_at: u64,
    source: &'static str,
    scope: EnvelopeScope,
    data: serde_json::Value,
}

#[derive(Serialize)]
struct EnvelopeScope {
    tenant_id: Option<String>,
    repo_path: Option<String>,
    oci_repository: Option<String>,
}

impl Envelope {
    fn from_event(event: &CommittedEvent) -> Result<Self, ProjectionDrop> {
        Ok(Self {
            schema_version: 1,
            event_id: event.event_id.to_string(),
            event_type: event.event_type.as_str(),
            occurred_at: event.occurred_at,
            source: event.source.as_str(),
            scope: match &event.scope {
                EventScope::Git { repo_path } | EventScope::Media { repo_path } => EnvelopeScope {
                    tenant_id: None,
                    repo_path: Some(repo_path.clone()),
                    oci_repository: None,
                },
                EventScope::Oci { oci_repository } => EnvelopeScope {
                    tenant_id: None,
                    repo_path: None,
                    oci_repository: Some(oci_repository.clone()),
                },
                EventScope::LfsUnscoped => EnvelopeScope {
                    tenant_id: None,
                    repo_path: None,
                    oci_repository: None,
                },
                EventScope::AgentCapture {
                    tenant_id,
                    repo_path,
                } => EnvelopeScope {
                    tenant_id: Some(tenant_id.clone()),
                    repo_path: Some(repo_path.clone()),
                    oci_repository: None,
                },
            },
            data: data_value(&event.data)?,
        })
    }
}

fn data_value(data: &EventData) -> Result<serde_json::Value, ProjectionDrop> {
    match data {
        EventData::RepoPush {
            push_id,
            operation_id,
            ref_name,
            old_oid,
            requested_oid,
            landed_oid,
        } => Ok(serde_json::json!({
            "push_id": push_id,
            "operation_id": operation_id,
            "ref_name": ref_name,
            "old_oid": old_oid,
            "requested_oid": requested_oid,
            "landed_oid": landed_oid,
        })),
        EventData::OciManifestPublished {
            digest,
            reference,
            media_type,
            size,
        } => Ok(serde_json::json!({
            "digest": digest,
            "reference": reference,
            "media_type": media_type,
            "size": size,
        })),
        EventData::LfsObjectUploaded { oid, size } => Ok(serde_json::json!({
            "oid": oid,
            "size": size,
            "transfer": "basic",
        })),
        EventData::LfsMediaFinalized {
            oid,
            size,
            manifest_id,
        } => Ok(serde_json::json!({
            "oid": oid,
            "size": size,
            "manifest_id": manifest_id,
            "transfer": "fastcdc",
        })),
        EventData::AgentCaptureEventsCommitted {
            capture_id,
            receipt_id,
            new_event_count,
            stream_kind,
            generation,
            completeness,
        } => Ok(serde_json::json!({
            "capture_id": capture_id,
            "receipt_id": receipt_id,
            "new_event_count": new_event_count,
            "stream_kind": stream_kind,
            "generation": generation,
            "completeness": completeness,
        })),
        EventData::AgentCaptureCheckpointCommitted {
            capture_id,
            checkpoint_id,
            completeness,
            raw_committed,
        } => Ok(serde_json::json!({
            "capture_id": capture_id,
            "checkpoint_id": checkpoint_id,
            "completeness": completeness,
            "raw_committed": raw_committed,
        })),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::config::StorageEventsTargetConfig;

    fn uuid() -> Uuid {
        Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("uuid")
    }

    fn sample_target() -> StorageEventsTargetConfig {
        StorageEventsTargetConfig {
            id: "ops-main".to_string(),
            url: "https://events.example.invalid/ingest".to_string(),
            secret_ref: "vault://secret/config/example/storage_events/targets/ops-main/hmac#value"
                .to_string(),
            events: vec![
                "repo.push".to_string(),
                "oci.manifest.published".to_string(),
                "lfs.object.uploaded".to_string(),
                "lfs.media.finalized".to_string(),
                "agent_capture.events.committed".to_string(),
                "agent_capture.checkpoint.committed".to_string(),
            ],
            git_paths: vec!["/team/a".to_string()],
            oci_repositories: vec!["team/image".to_string()],
            lfs_paths: vec!["/team/a".to_string()],
            include_unscoped_lfs: true,
            agent_tenants: vec!["acme".to_string()],
            agent_repo_paths: vec!["/team/a".to_string()],
        }
    }

    fn assert_no_leak(body: &Bytes) {
        let value: serde_json::Value = serde_json::from_slice(body).expect("json");
        let text = std::str::from_utf8(body).expect("utf8");
        assert!(value.get("actor").is_none(), "{text}");
        assert!(value.get("object_key").is_none(), "{text}");
        assert!(value.get("prompt").is_none(), "{text}");
        assert!(value.pointer("/data/raw").is_none(), "{text}");
        assert!(!text.contains("https://"), "{text}");
        assert!(!text.contains("object_key"), "{text}");
    }

    #[test]
    fn metadata_projection_budget() {
        let events = [
            CommittedEvent {
                event_id: uuid(),
                event_type: EventType::RepoPush,
                occurred_at: 1_700_000_000,
                source: EventSource::Git,
                scope: EventScope::Git {
                    repo_path: "/team/a".to_string(),
                },
                data: EventData::RepoPush {
                    push_id: "p1".to_string(),
                    operation_id: "op1".to_string(),
                    ref_name: "refs/heads/main".to_string(),
                    old_oid: "aa".to_string(),
                    requested_oid: "bb".to_string(),
                    landed_oid: "cc".to_string(),
                },
            },
            CommittedEvent {
                event_id: uuid(),
                event_type: EventType::OciManifestPublished,
                occurred_at: 1_700_000_000,
                source: EventSource::Oci,
                scope: EventScope::Oci {
                    oci_repository: "team/image".to_string(),
                },
                data: EventData::OciManifestPublished {
                    digest: "sha256:abc".to_string(),
                    reference: "latest".to_string(),
                    media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                    size: 12,
                },
            },
            CommittedEvent {
                event_id: uuid(),
                event_type: EventType::LfsObjectUploaded,
                occurred_at: 1_700_000_000,
                source: EventSource::Lfs,
                scope: EventScope::LfsUnscoped,
                data: EventData::LfsObjectUploaded {
                    oid: "ab".to_string(),
                    size: 4,
                },
            },
            CommittedEvent {
                event_id: uuid(),
                event_type: EventType::LfsMediaFinalized,
                occurred_at: 1_700_000_000,
                source: EventSource::Lfs,
                scope: EventScope::Media {
                    repo_path: "/team/a".to_string(),
                },
                data: EventData::LfsMediaFinalized {
                    oid: "cd".to_string(),
                    size: 8,
                    manifest_id: "m1".to_string(),
                },
            },
            CommittedEvent {
                event_id: uuid(),
                event_type: EventType::AgentCaptureEventsCommitted,
                occurred_at: 1_700_000_000,
                source: EventSource::AgentCapture,
                scope: EventScope::AgentCapture {
                    tenant_id: "acme".to_string(),
                    repo_path: "/team/a".to_string(),
                },
                data: EventData::AgentCaptureEventsCommitted {
                    capture_id: "1".to_string(),
                    receipt_id: "r1".to_string(),
                    new_event_count: 2,
                    stream_kind: "external_capture".to_string(),
                    generation: 1,
                    completeness: "complete".to_string(),
                },
            },
            CommittedEvent {
                event_id: uuid(),
                event_type: EventType::AgentCaptureCheckpointCommitted,
                occurred_at: 1_700_000_000,
                source: EventSource::AgentCapture,
                scope: EventScope::AgentCapture {
                    tenant_id: "acme".to_string(),
                    repo_path: "/team/a".to_string(),
                },
                data: EventData::AgentCaptureCheckpointCommitted {
                    capture_id: "1".to_string(),
                    checkpoint_id: "ck1".to_string(),
                    completeness: "incomplete".to_string(),
                    raw_committed: false,
                },
            },
        ];
        for event in &events {
            let body = project(event).expect("project");
            assert!(body.len() <= MAX_ENVELOPE_BYTES);
            assert_no_leak(&body);
            let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
            assert_eq!(value["schema_version"], 1);
            assert!(value["scope"].is_object());
            assert!(value["scope"].get("tenant_id").is_some());
            assert!(value["scope"].get("repo_path").is_some());
            assert!(value["scope"].get("oci_repository").is_some());
            assert!(value.get("actor").is_none());
        }

        let lfs_body = project(&events[2]).expect("lfs golden");
        assert_eq!(
            std::str::from_utf8(&lfs_body).expect("utf8"),
            r#"{"schema_version":1,"event_id":"11111111-1111-4111-8111-111111111111","event_type":"lfs.object.uploaded","occurred_at":1700000000,"source":"lfs","scope":{"tenant_id":null,"repo_path":null,"oci_repository":null},"data":{"oid":"ab","size":4,"transfer":"basic"}}"#
        );

        let mut oversized = events[0].clone();
        oversized.data = EventData::RepoPush {
            push_id: "aa".repeat(2000),
            operation_id: "bb".repeat(2000),
            ref_name: "cc".repeat(2000),
            old_oid: "dd".repeat(2000),
            requested_oid: "ee".repeat(2000),
            landed_oid: "ff".repeat(2000),
        };
        assert_eq!(project(&oversized), Err(ProjectionDrop::Size));
    }

    #[test]
    fn scope_isolation_matrix() {
        assert!(validate_canonical_path("/").is_ok());
        assert!(validate_canonical_path("/team/a").is_ok());
        assert!(validate_canonical_path("/team/a/b").is_ok());
        assert!(validate_canonical_path("/team/ab").is_ok());
        assert!(validate_canonical_path("team/a").is_err());
        assert!(validate_canonical_path("/team/a/").is_err());
        assert!(validate_canonical_path("/team//a").is_err());
        assert!(validate_canonical_path("/team/./a").is_err());
        assert!(validate_canonical_path("/team/../a").is_err());
        assert!(validate_canonical_path("/team/%61").is_err());
        assert!(validate_canonical_path(" /team/a").is_err());

        assert!(path_filter_matches("/team/a", "/team/a"));
        assert!(path_filter_matches("/team/a", "/team/a/b"));
        assert!(!path_filter_matches("/team/a", "/team/ab"));
        assert!(path_filter_matches("/", "/team/a"));

        let mut target = sample_target();
        let git = CommittedEvent {
            event_id: uuid(),
            event_type: EventType::RepoPush,
            occurred_at: 1,
            source: EventSource::Git,
            scope: EventScope::Git {
                repo_path: "/team/a/b".to_string(),
            },
            data: EventData::RepoPush {
                push_id: "p1".to_string(),
                operation_id: "op1".to_string(),
                ref_name: "refs/heads/main".to_string(),
                old_oid: "aa".to_string(),
                requested_oid: "bb".to_string(),
                landed_oid: "cc".to_string(),
            },
        };
        assert_eq!(select_targets(&git, std::slice::from_ref(&target)).len(), 1);
        let sibling = CommittedEvent {
            scope: EventScope::Git {
                repo_path: "/team/ab".to_string(),
            },
            ..git.clone()
        };
        assert!(select_targets(&sibling, std::slice::from_ref(&target)).is_empty());

        let oci = CommittedEvent {
            event_id: uuid(),
            event_type: EventType::OciManifestPublished,
            occurred_at: 1,
            source: EventSource::Oci,
            scope: EventScope::Oci {
                oci_repository: "team/image".to_string(),
            },
            data: EventData::OciManifestPublished {
                digest: "sha256:ab".to_string(),
                reference: "latest".to_string(),
                media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                size: 1,
            },
        };
        assert_eq!(select_targets(&oci, std::slice::from_ref(&target)).len(), 1);
        let oci_prefix = CommittedEvent {
            scope: EventScope::Oci {
                oci_repository: "team/image/child".to_string(),
            },
            ..oci.clone()
        };
        assert!(select_targets(&oci_prefix, std::slice::from_ref(&target)).is_empty());

        let agent = CommittedEvent {
            event_id: uuid(),
            event_type: EventType::AgentCaptureEventsCommitted,
            occurred_at: 1,
            source: EventSource::AgentCapture,
            scope: EventScope::AgentCapture {
                tenant_id: "acme".to_string(),
                repo_path: "/team/a".to_string(),
            },
            data: EventData::AgentCaptureEventsCommitted {
                capture_id: "1".to_string(),
                receipt_id: "r".to_string(),
                new_event_count: 1,
                stream_kind: "external_capture".to_string(),
                generation: 1,
                completeness: "complete".to_string(),
            },
        };
        assert_eq!(
            select_targets(&agent, std::slice::from_ref(&target)).len(),
            1
        );
        let other_tenant = CommittedEvent {
            scope: EventScope::AgentCapture {
                tenant_id: "other".to_string(),
                repo_path: "/team/a".to_string(),
            },
            ..agent.clone()
        };
        assert!(select_targets(&other_tenant, std::slice::from_ref(&target)).is_empty());

        let lfs = CommittedEvent {
            event_id: uuid(),
            event_type: EventType::LfsObjectUploaded,
            occurred_at: 1,
            source: EventSource::Lfs,
            scope: EventScope::LfsUnscoped,
            data: EventData::LfsObjectUploaded {
                oid: "ab".to_string(),
                size: 1,
            },
        };
        assert_eq!(select_targets(&lfs, std::slice::from_ref(&target)).len(), 1);
        target.include_unscoped_lfs = false;
        assert!(select_targets(&lfs, std::slice::from_ref(&target)).is_empty());
    }
}
