//! Storage-owned identity for a durable native metadata installation.
//! This plan grants no publication, lease or content-retention authority.

use std::collections::{BTreeMap, BTreeSet};

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES};
use sha2::{Digest, Sha256};

use super::{
    error::{SnapshotError, SnapshotErrorCode},
    retention_dag::{MetadataDagLimits, MetadataPageId, ValidatedMetadataDag},
};

const DOMAIN: &[u8] = b"mega.mst2.metadata-install.v1\0";
pub(crate) const MAX_PLAN_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetadataInstallIdentity {
    pub source_domain: String,
    pub tagged_root_tree_oid: String,
    pub scope: String,
    pub schema_version: u16,
    pub metadata_codec: u16,
    pub materialization_policy: u16,
    pub fs_semantics: u16,
    pub access_projection: u16,
    pub verification_revision: i32,
    pub projection_revision: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetadataInstallPlan {
    pub identity: MetadataInstallIdentity,
    pub root: MetadataPageId,
    pub pages: BTreeMap<MetadataPageId, u64>,
    pub edges: BTreeSet<(MetadataPageId, MetadataPageId)>,
    pub total_bytes: u64,
}

impl MetadataInstallPlan {
    pub(super) fn from_validated(
        identity: MetadataInstallIdentity,
        dag: &ValidatedMetadataDag,
    ) -> Result<Self, SnapshotError> {
        dag.check_limits(MetadataDagLimits::default())?;
        let edges = dag
            .edges()
            .iter()
            .map(|edge| Ok((parse_node_id(&edge.parent)?, parse_node_id(&edge.child)?)))
            .collect::<Result<_, SnapshotError>>()?;
        let plan = Self {
            identity,
            root: dag.root(),
            pages: dag
                .payloads()
                .iter()
                .map(|page| (page.id, page.size))
                .collect(),
            edges,
            total_bytes: dag.payload_bytes(),
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn digest(&self) -> Result<[u8; 32], SnapshotError> {
        Ok(Sha256::digest(self.encode()?).into())
    }

    pub fn encode(&self) -> Result<Vec<u8>, SnapshotError> {
        self.validate()?;
        let mut bytes = DOMAIN.to_vec();
        bytes.extend_from_slice(&1u16.to_be_bytes());
        for value in [
            &self.identity.source_domain,
            &self.identity.tagged_root_tree_oid,
            &self.identity.scope,
        ] {
            bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
        for value in [
            self.identity.schema_version,
            self.identity.metadata_codec,
            self.identity.materialization_policy,
            self.identity.fs_semantics,
            self.identity.access_projection,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&self.identity.verification_revision.to_be_bytes());
        bytes.extend_from_slice(&self.identity.projection_revision.to_be_bytes());
        bytes.extend_from_slice(&self.root);
        bytes.extend_from_slice(&(self.pages.len() as u32).to_be_bytes());
        for (id, size) in &self.pages {
            bytes.extend_from_slice(id);
            bytes.extend_from_slice(&size.to_be_bytes());
        }
        bytes.extend_from_slice(&(self.edges.len() as u32).to_be_bytes());
        for (parent, child) in &self.edges {
            bytes.extend_from_slice(parent);
            bytes.extend_from_slice(child);
        }
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(limit("metadata installation plan exceeds its byte budget"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8], expected_digest: &[u8; 32]) -> Result<Self, SnapshotError> {
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(limit(
                "stored metadata installation plan exceeds its byte budget",
            ));
        }
        if Sha256::digest(bytes).as_slice() != expected_digest {
            return Err(integrity(
                "stored metadata installation plan digest mismatch",
            ));
        }
        let mut reader = PlanReader(bytes);
        if reader.take(DOMAIN.len())? != DOMAIN || reader.u16()? != 1 {
            return Err(integrity("unsupported metadata installation plan encoding"));
        }
        let identity = MetadataInstallIdentity {
            source_domain: reader.string(64)?,
            tagged_root_tree_oid: reader.string(128)?,
            scope: reader.string(4096)?,
            schema_version: reader.u16()?,
            metadata_codec: reader.u16()?,
            materialization_policy: reader.u16()?,
            fs_semantics: reader.u16()?,
            access_projection: reader.u16()?,
            verification_revision: i32::from_be_bytes(reader.array()?),
            projection_revision: reader.u16()?,
        };
        let root = reader.array()?;
        let count = reader.count(MetadataDagLimits::default().nodes)?;
        let mut pages = BTreeMap::new();
        let mut total_bytes = 0u64;
        let mut last = None;
        for _ in 0..count {
            let id = reader.array()?;
            let size = reader.u64()?;
            if last.is_some_and(|previous| previous >= id) {
                return Err(integrity(
                    "metadata installation pages are not uniquely ordered",
                ));
            }
            last = Some(id);
            total_bytes = total_bytes
                .checked_add(size)
                .ok_or_else(|| limit("metadata installation byte count overflow"))?;
            pages.insert(id, size);
        }
        let count = reader.count(MetadataDagLimits::default().edges)?;
        let mut edges = BTreeSet::new();
        let mut last = None;
        for _ in 0..count {
            let edge = (reader.array()?, reader.array()?);
            if last.is_some_and(|previous| previous >= edge) {
                return Err(integrity(
                    "metadata installation edges are not uniquely ordered",
                ));
            }
            last = Some(edge);
            edges.insert(edge);
        }
        if !reader.0.is_empty() {
            return Err(integrity("metadata installation plan has trailing bytes"));
        }
        let plan = Self {
            identity,
            root,
            pages,
            edges,
            total_bytes,
        };
        plan.validate()?;
        if plan.encode()?.as_slice() != bytes {
            return Err(integrity("metadata installation plan is not canonical"));
        }
        Ok(plan)
    }

    fn validate(&self) -> Result<(), SnapshotError> {
        let identity = &self.identity;
        if identity.source_domain != "native-git"
            || identity.schema_version != mst2_codec::descriptor::SCHEMA_VERSION
            || identity.metadata_codec != mst2_codec::descriptor::METADATA_CODEC
            || identity.materialization_policy
                != mst2_codec::descriptor::MATERIALIZATION_POLICY_GIT_RAW_V1
            || identity.fs_semantics != mst2_codec::descriptor::FS_SEMANTICS_LINUX_CODE_V1
            || identity.access_projection != mst2_codec::descriptor::ACCESS_PROJECTION_EXACT_FULL
            || identity.verification_revision
                != crate::jupiter::storage::mono_storage::MST2_VERIFICATION_VERSION
            || identity.projection_revision != 1
        {
            return Err(integrity(
                "unsupported stored native metadata installation profile",
            ));
        }
        let tagged = identity.tagged_root_tree_oid.as_str();
        let (kind, hex) = tagged
            .split_once(':')
            .ok_or_else(|| integrity("untagged fixed root tree"))?;
        let length = match kind {
            "sha1" => 40,
            "sha256" | "blake3" => 64,
            _ => return Err(integrity("unsupported fixed root tree hash kind")),
        };
        if hex.len() != length
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(integrity("noncanonical fixed root tree identity"));
        }
        super::view::validate_scope_relative_path(&identity.scope)
            .map_err(|_| integrity("noncanonical metadata installation scope"))?;
        let limits = MetadataDagLimits::default();
        if self.pages.is_empty()
            || self.pages.len() > limits.nodes
            || self.edges.len() > limits.edges
            || self.total_bytes > limits.payload_bytes
        {
            return Err(limit("metadata installation group exceeds its budget"));
        }
        if !self.pages.contains_key(&self.root)
            || self
                .pages
                .values()
                .any(|size| *size < HEADER_LEN as u64 || *size > PAGE_MAX_BYTES as u64)
            || self
                .pages
                .values()
                .try_fold(0u64, |total, size| total.checked_add(*size))
                != Some(self.total_bytes)
        {
            return Err(integrity(
                "metadata installation root or page size is invalid",
            ));
        }
        let mut incoming: BTreeMap<_, usize> = self.pages.keys().map(|id| (*id, 0)).collect();
        let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for (parent, child) in &self.edges {
            if parent == child || !self.pages.contains_key(parent) {
                return Err(integrity("invalid metadata installation edge"));
            }
            *incoming
                .get_mut(child)
                .ok_or_else(|| integrity("metadata installation child is missing"))? += 1;
            children.entry(*parent).or_default().push(*child);
        }
        let mut ready: Vec<_> = incoming
            .iter()
            .filter_map(|(id, count)| (*count == 0).then_some(*id))
            .collect();
        let mut visited = 0;
        while let Some(parent) = ready.pop() {
            visited += 1;
            for child in children.get(&parent).into_iter().flatten() {
                let count = incoming
                    .get_mut(child)
                    .ok_or_else(|| integrity("missing installation child"))?;
                *count -= 1;
                if *count == 0 {
                    ready.push(*child);
                }
            }
        }
        if visited != self.pages.len() {
            return Err(integrity("cyclic metadata installation plan"));
        }
        let mut reachable = BTreeSet::new();
        let mut pending = vec![self.root];
        while let Some(id) = pending.pop() {
            if reachable.insert(id) {
                pending.extend(children.get(&id).into_iter().flatten());
            }
        }
        if reachable.len() != self.pages.len() {
            return Err(integrity("unreachable metadata installation pages"));
        }
        Ok(())
    }
}

struct PlanReader<'a>(&'a [u8]);

impl<'a> PlanReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], SnapshotError> {
        if count > self.0.len() {
            return Err(integrity("truncated metadata installation plan"));
        }
        let (bytes, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(bytes)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], SnapshotError> {
        self.take(N)?
            .try_into()
            .map_err(|_| integrity("invalid installation field"))
    }
    fn u16(&mut self) -> Result<u16, SnapshotError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn count(&mut self, maximum: usize) -> Result<usize, SnapshotError> {
        let count = self.u32()? as usize;
        if count > maximum {
            return Err(limit("metadata installation count exceeds its budget"));
        }
        Ok(count)
    }
    fn string(&mut self, maximum: usize) -> Result<String, SnapshotError> {
        let count = self.count(maximum)?;
        String::from_utf8(self.take(count)?.to_vec())
            .map_err(|_| integrity("non-UTF8 installation identity"))
    }
}

fn parse_node_id(value: &str) -> Result<MetadataPageId, SnapshotError> {
    let value = value
        .strip_prefix("page:sha256:")
        .ok_or_else(|| integrity("non-page installation node"))?;
    hex::decode(value)
        .map_err(|_| integrity("invalid installation page ID"))?
        .try_into()
        .map_err(|_| integrity("invalid installation page ID length"))
}

fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}
fn limit(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::LimitExceeded, message)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mst2_codec::metapage::{Page, page_id};

    use super::*;
    use crate::ceres::snapshot::{
        pages::PreparedNativeMetadataRetention, retention_dag::MetadataDagBuilder,
    };

    fn plan() -> MetadataInstallPlan {
        let bytes = Page::build(&[]).unwrap();
        let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
        builder.add_directory(&bytes, &[]).unwrap();
        PreparedNativeMetadataRetention::test_installation(
            Arc::new(builder.finish(page_id(&bytes)).unwrap()),
            "/",
        )
        .install_plan()
        .unwrap()
    }

    #[test]
    fn native_installation_plan_round_trip_binds_every_source_and_profile_field() {
        let plan = plan();
        let bytes = plan.encode().unwrap();
        let digest = plan.digest().unwrap();
        assert_eq!(MetadataInstallPlan::decode(&bytes, &digest).unwrap(), plan);
        let mut changed = plan.clone();
        changed.identity.tagged_root_tree_oid = format!("sha1:{}", "b".repeat(40));
        assert_ne!(changed.digest().unwrap(), digest);
        changed = plan.clone();
        changed.identity.scope = "/other".into();
        assert_ne!(changed.digest().unwrap(), digest);
        changed = plan;
        changed.identity.projection_revision += 1;
        assert_eq!(
            changed.encode().unwrap_err().code,
            SnapshotErrorCode::IntegrityError
        );
    }

    #[test]
    fn native_installation_stored_plan_rejects_bad_digest_truncation_trailing_bytes_and_profile() {
        let plan = plan();
        let bytes = plan.encode().unwrap();
        let digest = plan.digest().unwrap();
        let mut bad_digest = digest;
        bad_digest[0] ^= 1;
        assert!(MetadataInstallPlan::decode(&bytes, &bad_digest).is_err());
        for length in [0, DOMAIN.len(), bytes.len() - 1] {
            let truncated = &bytes[..length];
            let digest = Sha256::digest(truncated).into();
            assert!(MetadataInstallPlan::decode(truncated, &digest).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(MetadataInstallPlan::decode(&trailing, &Sha256::digest(&trailing).into()).is_err());
        let mut wrong_domain = bytes;
        wrong_domain[0] ^= 1;
        assert!(
            MetadataInstallPlan::decode(&wrong_domain, &Sha256::digest(&wrong_domain).into())
                .is_err()
        );
    }

    #[test]
    fn native_installation_plan_rejects_oversized_stored_bytes_before_allocating_fields() {
        let bytes = vec![0; MAX_PLAN_BYTES + 1];
        assert_eq!(
            MetadataInstallPlan::decode(&bytes, &[0; 32])
                .unwrap_err()
                .code,
            SnapshotErrorCode::LimitExceeded
        );
    }

    #[test]
    fn native_installation_posix_scopes_preserve_utf8_backslash_control_and_path_boundaries() {
        let original = plan();
        let at_byte_limit = format!("/{}", vec!["a".repeat(255); 16].join("/"));
        assert_eq!(at_byte_limit.len(), 4096);
        let at_component_limit = format!("/{}", vec!["a"; 256].join("/"));
        for scope in [
            "/a\\b".to_owned(),
            "/目录/é".to_owned(),
            "/line\ncontrol\t".to_owned(),
            format!("/{}", "a".repeat(255)),
            at_byte_limit.clone(),
            at_component_limit,
        ] {
            let mut value = original.clone();
            value.identity.scope = scope.clone();
            let encoded = value.encode().unwrap();
            assert_eq!(
                MetadataInstallPlan::decode(&encoded, &value.digest().unwrap())
                    .unwrap()
                    .identity
                    .scope,
                scope
            );
        }
        for scope in [
            format!("/{}", "a".repeat(256)),
            format!("/{}", vec!["a"; 257].join("/")),
            format!("{at_byte_limit}/a"),
            "/a\0b".to_owned(),
            "/a/..".to_owned(),
        ] {
            let mut value = original.clone();
            value.identity.scope = scope;
            assert_eq!(
                value.encode().unwrap_err().code,
                SnapshotErrorCode::IntegrityError
            );
        }
    }
}
