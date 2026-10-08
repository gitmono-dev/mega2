//! Bounded native metadata delta and certified reused-root installation identity.
//! Reused roots are opaque boundaries; their database proofs grant no authority here.

use std::collections::{BTreeMap, BTreeSet};

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    error::{SnapshotError, SnapshotErrorCode},
    metadata_install::{MAX_PLAN_BYTES, MetadataInstallIdentity},
    projection_observation::NATIVE_PROJECTION_REVISION,
    retention_dag::{MetadataDagLimits, MetadataPageId},
};

const DOMAIN: &[u8] = b"mega.mst2.rooted-install.v1\0";
const ENCODING_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootedReuseRoot {
    pub(crate) generation: i64,
    pub(crate) attestation_id: Uuid,
    pub(crate) attestation_digest: [u8; 32],
    pub(crate) certificate_digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootedMetadataInstallPlan {
    pub(crate) identity: MetadataInstallIdentity,
    pub(crate) root: MetadataPageId,
    pub(crate) delta: BTreeMap<MetadataPageId, u64>,
    pub(crate) edges: BTreeSet<(MetadataPageId, MetadataPageId)>,
    pub(crate) reused: BTreeMap<MetadataPageId, RootedReuseRoot>,
    pub(crate) source_roots: BTreeMap<String, MetadataPageId>,
}

impl RootedMetadataInstallPlan {
    pub(crate) fn new(
        identity: MetadataInstallIdentity,
        root: MetadataPageId,
        delta: BTreeMap<MetadataPageId, u64>,
        edges: BTreeSet<(MetadataPageId, MetadataPageId)>,
        reused: BTreeMap<MetadataPageId, RootedReuseRoot>,
        source_roots: BTreeMap<String, MetadataPageId>,
    ) -> Result<Self, SnapshotError> {
        let plan = Self {
            identity,
            root,
            delta,
            edges,
            reused,
            source_roots,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub(crate) fn digest(&self) -> Result<[u8; 32], SnapshotError> {
        Ok(Sha256::digest(self.encode()?).into())
    }

    pub(crate) fn delta_bytes(&self) -> Result<u64, SnapshotError> {
        self.delta
            .values()
            .try_fold(0u64, |total, size| total.checked_add(*size))
            .ok_or_else(|| limit("rooted metadata delta byte count overflow"))
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, SnapshotError> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(self.encoded_len());
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&ENCODING_VERSION.to_be_bytes());
        for value in [
            &self.identity.source_domain,
            &self.identity.tagged_root_tree_oid,
            &self.identity.scope,
        ] {
            write_string(&mut bytes, value);
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
        bytes.extend_from_slice(&(self.delta.len() as u32).to_be_bytes());
        for (page, size) in &self.delta {
            bytes.extend_from_slice(page);
            bytes.extend_from_slice(&size.to_be_bytes());
        }
        bytes.extend_from_slice(&(self.edges.len() as u32).to_be_bytes());
        for (parent, child) in &self.edges {
            bytes.extend_from_slice(parent);
            bytes.extend_from_slice(child);
        }
        bytes.extend_from_slice(&(self.reused.len() as u32).to_be_bytes());
        for (page, reused) in &self.reused {
            bytes.extend_from_slice(page);
            bytes.extend_from_slice(&reused.generation.to_be_bytes());
            bytes.extend_from_slice(reused.attestation_id.as_bytes());
            bytes.extend_from_slice(&reused.attestation_digest);
            bytes.extend_from_slice(&reused.certificate_digest);
        }
        bytes.extend_from_slice(&(self.source_roots.len() as u32).to_be_bytes());
        for (tree_oid, page) in &self.source_roots {
            write_string(&mut bytes, tree_oid);
            bytes.extend_from_slice(page);
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8], expected_digest: &[u8; 32]) -> Result<Self, SnapshotError> {
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(limit("stored rooted metadata plan exceeds its byte budget"));
        }
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if &digest != expected_digest {
            return Err(integrity("stored rooted metadata plan digest mismatch"));
        }
        let mut reader = PlanReader(bytes);
        if reader.take(DOMAIN.len())? != DOMAIN || reader.u16()? != ENCODING_VERSION {
            return Err(integrity("unsupported rooted metadata plan encoding"));
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
        let limits = MetadataDagLimits::default();
        let count = reader.count(limits.nodes)?;
        let mut delta = BTreeMap::new();
        let mut last = None;
        for _ in 0..count {
            let page = reader.array()?;
            let size = reader.u64()?;
            if last.is_some_and(|previous| previous >= page) {
                return Err(integrity("rooted metadata delta is not uniquely ordered"));
            }
            last = Some(page);
            delta.insert(page, size);
        }
        let count = reader.count(limits.edges)?;
        let mut edges = BTreeSet::new();
        let mut last = None;
        for _ in 0..count {
            let edge = (reader.array()?, reader.array()?);
            if last.is_some_and(|previous| previous >= edge) {
                return Err(integrity("rooted metadata edges are not uniquely ordered"));
            }
            last = Some(edge);
            edges.insert(edge);
        }
        let count = reader.count(limits.nodes - delta.len())?;
        let mut reused = BTreeMap::new();
        let mut last = None;
        for _ in 0..count {
            let page = reader.array()?;
            let proof = RootedReuseRoot {
                generation: i64::from_be_bytes(reader.array()?),
                attestation_id: Uuid::from_bytes(reader.array()?),
                attestation_digest: reader.array()?,
                certificate_digest: reader.array()?,
            };
            if last.is_some_and(|previous| previous >= page) {
                return Err(integrity(
                    "rooted metadata reused roots are not uniquely ordered",
                ));
            }
            last = Some(page);
            reused.insert(page, proof);
        }
        let count = reader.count(limits.nodes)?;
        let mut source_roots: BTreeMap<String, MetadataPageId> = BTreeMap::new();
        for _ in 0..count {
            let tree_oid = reader.string(128)?;
            let page = reader.array()?;
            if source_roots
                .last_key_value()
                .is_some_and(|(previous, _)| previous >= &tree_oid)
            {
                return Err(integrity(
                    "rooted metadata source roots are not uniquely ordered",
                ));
            }
            source_roots.insert(tree_oid, page);
        }
        if !reader.0.is_empty() {
            return Err(integrity("rooted metadata plan has trailing bytes"));
        }
        Self::new(identity, root, delta, edges, reused, source_roots)
    }

    pub(crate) fn validate(&self) -> Result<(), SnapshotError> {
        self.validate_and_order().map(|_| ())
    }

    /// Child-first delta order. Reused roots have no descendants in this plan.
    pub(crate) fn child_first_delta(&self) -> Result<Vec<MetadataPageId>, SnapshotError> {
        self.validate_and_order()
    }

    fn validate_and_order(&self) -> Result<Vec<MetadataPageId>, SnapshotError> {
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
            || identity.projection_revision != NATIVE_PROJECTION_REVISION
        {
            return Err(integrity("unsupported rooted native metadata profile"));
        }
        let hash_kind = tagged_hash_kind(&identity.tagged_root_tree_oid)?;
        super::view::validate_scope_relative_path(&identity.scope)
            .map_err(|_| integrity("noncanonical rooted metadata scope"))?;
        let limits = MetadataDagLimits::default();
        if self.delta.len() > limits.nodes
            || self.reused.len() > limits.nodes - self.delta.len()
            || self.delta.is_empty() && self.reused.is_empty()
            || self.edges.len() > limits.edges
            || self.source_roots.is_empty()
            || self.source_roots.len() > limits.nodes
        {
            return Err(limit("rooted metadata plan exceeds its group budget"));
        }
        if self.delta_bytes()? > limits.payload_bytes {
            return Err(limit("rooted metadata delta exceeds its payload budget"));
        }
        if !self.contains(&self.root)
            || self.delta.iter().any(|(page, size)| {
                self.reused.contains_key(page)
                    || !(HEADER_LEN as u64..=PAGE_MAX_BYTES as u64).contains(size)
            })
            || self.reused.values().any(|proof| proof.generation <= 0)
            || self.delta.is_empty()
                && (!self.reused.contains_key(&self.root) || !self.edges.is_empty())
        {
            return Err(integrity(
                "rooted metadata root, delta or reuse boundary is invalid",
            ));
        }
        for (tree_oid, page) in &self.source_roots {
            if tagged_hash_kind(tree_oid)? != hash_kind || !self.contains(page) {
                return Err(integrity(
                    "rooted metadata source binding crossed its profile or boundary",
                ));
            }
        }
        if !self.source_roots.values().any(|page| page == &self.root) {
            return Err(integrity(
                "rooted metadata root lacks a source tree binding",
            ));
        }
        if self.encoded_len() > MAX_PLAN_BYTES {
            return Err(limit("rooted metadata plan exceeds its byte budget"));
        }

        let mut remaining: BTreeMap<_, usize> = self.delta.keys().map(|page| (*page, 0)).collect();
        let mut parents: BTreeMap<_, Vec<_>> = BTreeMap::new();
        let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for &(parent, child) in &self.edges {
            if parent == child || !self.delta.contains_key(&parent) || !self.contains(&child) {
                return Err(integrity(
                    "rooted metadata edge crossed its delta or reused boundary",
                ));
            }
            children.entry(parent).or_default().push(child);
            if self.delta.contains_key(&child) {
                *remaining
                    .get_mut(&parent)
                    .ok_or_else(|| integrity("rooted metadata delta parent is missing"))? += 1;
                parents.entry(child).or_default().push(parent);
            }
        }
        let mut ready: BTreeSet<_> = remaining
            .iter()
            .filter_map(|(page, count)| (*count == 0).then_some(*page))
            .collect();
        let mut ordered = Vec::with_capacity(self.delta.len());
        while let Some(page) = ready.pop_first() {
            ordered.push(page);
            for parent in parents.get(&page).into_iter().flatten() {
                let count = remaining
                    .get_mut(parent)
                    .ok_or_else(|| integrity("rooted metadata delta ancestor is missing"))?;
                *count = count
                    .checked_sub(1)
                    .ok_or_else(|| integrity("rooted metadata delta edge accounting underflow"))?;
                if *count == 0 {
                    ready.insert(*parent);
                }
            }
        }
        if ordered.len() != self.delta.len() {
            return Err(integrity("rooted metadata delta contains a cycle"));
        }
        let mut reachable = BTreeSet::new();
        let mut pending = vec![self.root];
        while let Some(page) = pending.pop() {
            if reachable.insert(page) {
                pending.extend(children.get(&page).into_iter().flatten());
            }
        }
        if reachable.len() != self.delta.len() + self.reused.len() {
            return Err(integrity(
                "rooted metadata plan contains unreachable boundaries",
            ));
        }
        Ok(ordered)
    }

    fn contains(&self, page: &MetadataPageId) -> bool {
        self.delta.contains_key(page) || self.reused.contains_key(page)
    }

    fn encoded_len(&self) -> usize {
        DOMAIN.len()
            + 2
            + 3 * 4
            + self.identity.source_domain.len()
            + self.identity.tagged_root_tree_oid.len()
            + self.identity.scope.len()
            + 5 * 2
            + 4
            + 2
            + 32
            + 4 * 4
            + self.delta.len() * 40
            + self.edges.len() * 64
            + self.reused.len() * 120
            + self
                .source_roots
                .keys()
                .map(|tree_oid| 4 + tree_oid.len() + 32)
                .sum::<usize>()
    }
}

fn tagged_hash_kind(oid: &str) -> Result<&str, SnapshotError> {
    let (kind, hex) = oid
        .split_once(':')
        .ok_or_else(|| integrity("untagged rooted metadata source tree"))?;
    let length = match kind {
        "sha1" => 40,
        "sha256" | "blake3" => 64,
        _ => return Err(integrity("unsupported rooted metadata source hash kind")),
    };
    if hex.len() != length
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(integrity(
            "noncanonical rooted metadata source tree identity",
        ));
    }
    Ok(kind)
}

fn write_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

struct PlanReader<'a>(&'a [u8]);

impl<'a> PlanReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], SnapshotError> {
        if count > self.0.len() {
            return Err(integrity("truncated rooted metadata plan"));
        }
        let (bytes, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], SnapshotError> {
        self.take(N)?
            .try_into()
            .map_err(|_| integrity("invalid rooted metadata plan field"))
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
            return Err(limit("rooted metadata plan count exceeds its budget"));
        }
        Ok(count)
    }

    fn string(&mut self, maximum: usize) -> Result<String, SnapshotError> {
        let count = self.count(maximum)?;
        String::from_utf8(self.take(count)?.to_vec())
            .map_err(|_| integrity("non-UTF8 rooted metadata plan identity"))
    }
}

fn integrity(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::IntegrityError, message)
}

fn limit(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::LimitExceeded, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(value: u32) -> MetadataPageId {
        let mut id = [0; 32];
        id[28..].copy_from_slice(&value.to_be_bytes());
        id
    }

    fn identity() -> MetadataInstallIdentity {
        MetadataInstallIdentity {
            source_domain: "native-git".into(),
            tagged_root_tree_oid: format!("sha1:{}", "a".repeat(40)),
            scope: "/".into(),
            schema_version: mst2_codec::descriptor::SCHEMA_VERSION,
            metadata_codec: mst2_codec::descriptor::METADATA_CODEC,
            materialization_policy: mst2_codec::descriptor::MATERIALIZATION_POLICY_GIT_RAW_V1,
            fs_semantics: mst2_codec::descriptor::FS_SEMANTICS_LINUX_CODE_V1,
            access_projection: mst2_codec::descriptor::ACCESS_PROJECTION_EXACT_FULL,
            verification_revision: crate::jupiter::storage::mono_storage::MST2_VERIFICATION_VERSION,
            projection_revision: NATIVE_PROJECTION_REVISION,
        }
    }

    fn reuse(value: u128) -> RootedReuseRoot {
        RootedReuseRoot {
            generation: 7,
            attestation_id: Uuid::from_u128(value),
            attestation_digest: [11; 32],
            certificate_digest: [12; 32],
        }
    }

    fn plan() -> RootedMetadataInstallPlan {
        let identity = identity();
        RootedMetadataInstallPlan::new(
            identity.clone(),
            page(1),
            BTreeMap::from([(page(1), 20), (page(2), 57), (page(3), 16384)]),
            BTreeSet::from([
                (page(1), page(2)),
                (page(1), page(3)),
                (page(1), page(5)),
                (page(2), page(4)),
                (page(3), page(4)),
            ]),
            BTreeMap::from([(page(4), reuse(1)), (page(5), reuse(2))]),
            BTreeMap::from([
                (identity.tagged_root_tree_oid, page(1)),
                (format!("sha1:{}", "b".repeat(40)), page(4)),
            ]),
        )
        .unwrap()
    }

    fn decode(bytes: &[u8]) -> Result<RootedMetadataInstallPlan, SnapshotError> {
        RootedMetadataInstallPlan::decode(bytes, &Sha256::digest(bytes).into())
    }

    fn records(bytes: &[u8]) -> [Vec<std::ops::Range<usize>>; 4] {
        let mut reader = PlanReader(bytes);
        reader.take(DOMAIN.len() + 2).unwrap();
        for _ in 0..3 {
            reader.string(4096).unwrap();
        }
        reader.take(16 + 32).unwrap();
        let mut sections: [Vec<std::ops::Range<usize>>; 4] = std::array::from_fn(|_| Vec::new());
        for (section, width) in sections[..3].iter_mut().zip([40, 64, 120]) {
            let count = reader.u32().unwrap();
            for _ in 0..count {
                let start = bytes.len() - reader.0.len();
                reader.take(width).unwrap();
                section.push(start..start + width);
            }
        }
        let count = reader.u32().unwrap();
        for _ in 0..count {
            let start = bytes.len() - reader.0.len();
            reader.string(128).unwrap();
            reader.take(32).unwrap();
            sections[3].push(start..bytes.len() - reader.0.len());
        }
        assert!(reader.0.is_empty());
        sections
    }

    #[test]
    fn rooted_plan_roundtrip_and_shared_reuse_need_only_delta_order() {
        let plan = plan();
        let bytes = plan.encode().unwrap();
        assert_eq!(bytes.len(), plan.encoded_len());
        assert_eq!(bytes.len(), 1004);
        assert_eq!(
            hex::encode(plan.digest().unwrap()),
            "e74e98e8737cfcbdfb264d9f12e15ba2126e96b1e5f35cb0cfd89fb24dde808b"
        );
        assert_eq!(decode(&bytes).unwrap(), plan);
        assert_eq!(
            plan.child_first_delta().unwrap(),
            [page(2), page(3), page(1)]
        );
        assert_eq!(plan.delta_bytes().unwrap(), 20 + 57 + 16384);
        for range in &records(&bytes)[2] {
            assert_eq!(range.len(), 120);
        }
        let original = plan.digest().unwrap();
        for field in 0..4 {
            let mut changed = plan.clone();
            let proof = changed.reused.get_mut(&page(4)).unwrap();
            match field {
                0 => proof.generation += 1,
                1 => proof.attestation_id = Uuid::from_u128(9),
                2 => proof.attestation_digest[0] ^= 1,
                _ => proof.certificate_digest[0] ^= 1,
            }
            assert_ne!(changed.digest().unwrap(), original);
        }
        let mut changed = plan.clone();
        changed.identity.scope = "/目录/a\\b".into();
        assert_ne!(changed.digest().unwrap(), original);
        assert_eq!(decode(&changed.encode().unwrap()).unwrap(), changed);
        changed = plan;
        changed
            .source_roots
            .insert(format!("sha1:{}", "c".repeat(40)), page(3));
        assert_ne!(changed.digest().unwrap(), original);
    }

    #[test]
    fn rooted_plan_stored_order_duplicates_digest_domain_version_truncation_and_eof_reject() {
        let plan = plan();
        let bytes = plan.encode().unwrap();
        let mut wrong_digest = plan.digest().unwrap();
        wrong_digest[0] ^= 1;
        assert!(RootedMetadataInstallPlan::decode(&bytes, &wrong_digest).is_err());
        for section in records(&bytes) {
            let mut changed = bytes.clone();
            let a = &section[0];
            let b = &section[1];
            assert_eq!(a.len(), b.len());
            changed[a.clone()].copy_from_slice(&bytes[b.clone()]);
            changed[b.clone()].copy_from_slice(&bytes[a.clone()]);
            assert!(decode(&changed).is_err());
            changed[b.clone()].copy_from_slice(&bytes[b.clone()]);
            assert!(decode(&changed).is_err());
        }
        for length in [0, DOMAIN.len(), bytes.len() - 1] {
            assert!(decode(&bytes[..length]).is_err());
        }
        let mut changed = bytes.clone();
        changed.push(0);
        assert!(decode(&changed).is_err());
        changed = bytes.clone();
        changed[0] ^= 1;
        assert!(decode(&changed).is_err());
        changed = bytes;
        changed[DOMAIN.len() + 1] = 2;
        assert!(decode(&changed).is_err());
        let oversized = vec![0; MAX_PLAN_BYTES + 1];
        assert_eq!(
            decode(&oversized).unwrap_err().code,
            SnapshotErrorCode::LimitExceeded
        );
    }

    #[test]
    fn rooted_plan_decode_bounds_declared_counts_strings_and_reused_generations() {
        let bytes = plan().encode().unwrap();
        let sections = records(&bytes);
        for section in &sections {
            let mut changed = bytes.clone();
            let count_at = section[0].start - 4;
            changed[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
            assert_eq!(
                decode(&changed).unwrap_err().code,
                SnapshotErrorCode::LimitExceeded
            );
        }
        let mut changed = bytes.clone();
        let source_at = sections[3][0].start;
        changed[source_at..source_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            decode(&changed).unwrap_err().code,
            SnapshotErrorCode::LimitExceeded
        );
        changed = bytes.clone();
        changed[DOMAIN.len() + 2 + 4] = 0xff;
        assert_eq!(
            decode(&changed).unwrap_err().code,
            SnapshotErrorCode::IntegrityError
        );
        changed = bytes;
        let generation_at = sections[2][0].start + 32;
        changed[generation_at..generation_at + 8].copy_from_slice(&i64::MIN.to_be_bytes());
        assert_eq!(
            decode(&changed).unwrap_err().code,
            SnapshotErrorCode::IntegrityError
        );
    }

    #[test]
    fn rooted_plan_rejects_cycles_unknown_endpoints_reused_parents_and_unreachable_members() {
        let original = plan();
        for invalid in 0..7 {
            let mut changed = original.clone();
            match invalid {
                0 => {
                    changed.edges.insert((page(2), page(1)));
                }
                1 => {
                    changed.edges.insert((page(4), page(2)));
                }
                2 => {
                    changed.edges.insert((page(2), page(99)));
                }
                3 => {
                    changed.edges.insert((page(99), page(2)));
                }
                4 => {
                    changed.delta.insert(page(6), 20);
                }
                5 => {
                    changed.reused.insert(page(6), reuse(6));
                }
                _ => {
                    changed.reused.insert(page(2), reuse(2));
                }
            }
            assert_eq!(
                changed.encode().unwrap_err().code,
                SnapshotErrorCode::IntegrityError
            );
        }
    }

    #[test]
    fn rooted_plan_zero_delta_requires_its_single_reused_root_and_source_binding() {
        let identity = identity();
        let plan = RootedMetadataInstallPlan::new(
            identity.clone(),
            page(4),
            BTreeMap::new(),
            BTreeSet::new(),
            BTreeMap::from([(page(4), reuse(4))]),
            BTreeMap::from([(identity.tagged_root_tree_oid, page(4))]),
        )
        .unwrap();
        assert!(plan.child_first_delta().unwrap().is_empty());
        assert_eq!(plan.delta_bytes().unwrap(), 0);
        assert_eq!(plan.encode().unwrap().len(), 363);
        assert_eq!(
            hex::encode(plan.digest().unwrap()),
            "81b4cbdaaacd7efe8687477d05bd4db190b974f5aa99ce7e8101326857d0e7b1"
        );
        assert_eq!(decode(&plan.encode().unwrap()).unwrap(), plan);
        for invalid in 0..5 {
            let mut changed = plan.clone();
            match invalid {
                0 => {
                    changed.root = page(9);
                }
                1 => {
                    changed.edges.insert((page(4), page(4)));
                }
                2 => {
                    changed.reused.insert(page(5), reuse(5));
                }
                3 => {
                    changed.delta.insert(page(5), 20);
                }
                _ => {
                    changed.source_roots.clear();
                }
            }
            assert!(changed.encode().is_err());
        }
    }

    #[test]
    fn rooted_plan_rejects_source_profile_scope_size_and_generation_mismatches() {
        let original = plan();
        for invalid in 0..18 {
            let mut changed = original.clone();
            match invalid {
                0 => changed.identity.metadata_codec += 1,
                1 => changed.identity.projection_revision += 1,
                2 => changed.identity.verification_revision += 1,
                3 => changed.identity.scope = "/a/..".into(),
                4 => changed.identity.scope = format!("/{}", "a".repeat(256)),
                5 => changed.identity.tagged_root_tree_oid = format!("sha1:{}", "A".repeat(40)),
                6 => {
                    changed.delta.insert(page(2), HEADER_LEN as u64 - 1);
                }
                7 => {
                    changed.delta.insert(page(2), PAGE_MAX_BYTES as u64 + 1);
                }
                8 => {
                    changed.reused.get_mut(&page(4)).unwrap().generation = 0;
                }
                9 => {
                    changed
                        .source_roots
                        .insert(format!("sha256:{}", "a".repeat(64)), page(3));
                }
                10 => {
                    changed
                        .source_roots
                        .insert(format!("sha1:{}", "d".repeat(40)), page(99));
                }
                11 => {
                    changed
                        .source_roots
                        .retain(|_, page_id| *page_id != page(1));
                }
                12 => {
                    changed.source_roots.insert("sha1:bad".into(), page(3));
                }
                13 => changed.identity.source_domain = "other".into(),
                14 => changed.identity.schema_version += 1,
                15 => changed.identity.materialization_policy += 1,
                16 => changed.identity.fs_semantics += 1,
                _ => changed.identity.access_projection += 1,
            }
            assert!(changed.encode().is_err());
        }
        for kind in ["sha256", "blake3"] {
            let mut changed = original.clone();
            changed.identity.tagged_root_tree_oid = format!("{kind}:{}", "a".repeat(64));
            changed.source_roots =
                BTreeMap::from([(changed.identity.tagged_root_tree_oid.clone(), page(1))]);
            assert_eq!(decode(&changed.encode().unwrap()).unwrap(), changed);
        }
    }

    #[test]
    fn rooted_plan_admits_exact_group_payload_and_source_count_boundaries() {
        let identity = identity();
        let limits = MetadataDagLimits::default();
        let delta: BTreeMap<_, _> = (1..=limits.nodes as u32)
            .map(|value| (page(value), PAGE_MAX_BYTES as u64))
            .collect();
        let edges = (2..=limits.nodes as u32)
            .map(|value| (page(1), page(value)))
            .collect();
        let full_plan = RootedMetadataInstallPlan::new(
            identity.clone(),
            page(1),
            delta,
            edges,
            BTreeMap::new(),
            BTreeMap::from([(identity.tagged_root_tree_oid, page(1))]),
        )
        .unwrap();
        assert_eq!(full_plan.delta_bytes().unwrap(), limits.payload_bytes);
        assert_eq!(decode(&full_plan.encode().unwrap()).unwrap(), full_plan);
        let mut changed = full_plan.clone();
        changed
            .delta
            .insert(page(limits.nodes as u32 + 1), HEADER_LEN as u64);
        assert_eq!(
            changed.encode().unwrap_err().code,
            SnapshotErrorCode::LimitExceeded
        );
        changed = full_plan.clone();
        changed
            .reused
            .insert(page(limits.nodes as u32 + 1), reuse(9));
        assert_eq!(
            changed.encode().unwrap_err().code,
            SnapshotErrorCode::LimitExceeded
        );

        let mut source_plan = plan();
        source_plan.source_roots = (0..limits.nodes)
            .map(|value| (format!("sha1:{value:040x}"), page(1)))
            .collect();
        assert_eq!(decode(&source_plan.encode().unwrap()).unwrap(), source_plan);
        source_plan
            .source_roots
            .insert(format!("sha1:{:040x}", limits.nodes), page(1));
        assert_eq!(
            source_plan.encode().unwrap_err().code,
            SnapshotErrorCode::LimitExceeded
        );
    }

    #[test]
    fn rooted_plan_admits_exact_edge_boundary_then_rejects_one_more_edge() {
        let mut value = plan();
        value.delta = (1..=4096).map(|id| (page(id), HEADER_LEN as u64)).collect();
        value.reused.clear();
        value.source_roots.retain(|_, root| *root == page(1));
        value.edges = (2..=4096).map(|id| (page(1), page(id))).collect();
        'fill: for parent in 2..=4096 {
            for child in parent + 1..=4096 {
                if value.edges.len() == MetadataDagLimits::default().edges {
                    break 'fill;
                }
                value.edges.insert((page(parent), page(child)));
            }
        }
        assert_eq!(value.edges.len(), MetadataDagLimits::default().edges);
        assert_eq!(decode(&value.encode().unwrap()).unwrap(), value);
        assert!(value.edges.insert((page(4095), page(4096))));
        assert_eq!(
            value.encode().unwrap_err().code,
            SnapshotErrorCode::LimitExceeded
        );
    }
}
