//! Quarantine unpack for one traces ingest. Objects land under the staging
//! prefix until closure and OID checks pass.

use std::collections::{HashMap, HashSet};

use crate::{
    ceres::traces::admission::{self, IngestAdmission},
    common::errors::{MegaError, TracesError},
    config::TracesProtocolLimits,
    jupiter::storage::{
        object_storage::MegaObjectStorageWrapper,
        trace_object_storage::{self, get_raw},
    },
};

/// One inflated loose object. Delta entries are not a quarantine object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LooseObject {
    pub object_type: String,
    pub declared_oid: String,
    pub payload: Vec<u8>,
}

/// Running unpack counters. They are the bytes already seen in this attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnpackBudget {
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
}

fn budget_exceeded() -> MegaError {
    MegaError::Traces(TracesError::ResponseBudget)
}

fn rejected() -> MegaError {
    MegaError::Traces(TracesError::InvalidParameter)
}

fn admit(budget: UnpackBudget) -> Result<(), MegaError> {
    if budget.compressed_bytes > TracesProtocolLimits::MAX_PACK_BYTES
        || budget.expanded_bytes > TracesProtocolLimits::MAX_DECODED_BYTES
    {
        return Err(budget_exceeded());
    }
    Ok(())
}

/// Verify OIDs and write the objects to ingest quarantine.
///
/// A budget overrun or a bad OID writes nothing further and does not publish.
pub async fn unpack_into_quarantine(
    store: &MegaObjectStorageWrapper,
    ingest_id: &str,
    hash_kind: &str,
    budget: UnpackBudget,
    objects: &[LooseObject],
) -> Result<Vec<String>, MegaError> {
    unpack_bounded(
        store,
        ingest_id,
        hash_kind,
        budget,
        &mut IngestAdmission::start(),
        objects,
    )
    .await
}

/// [`unpack_into_quarantine`] with the complexity already counted for this worker.
pub async fn unpack_bounded(
    store: &MegaObjectStorageWrapper,
    ingest_id: &str,
    hash_kind: &str,
    mut budget: UnpackBudget,
    admission: &mut IngestAdmission,
    objects: &[LooseObject],
) -> Result<Vec<String>, MegaError> {
    admit(budget)?;
    admission::admit_ingest(admission, 0)?;
    screen_complexity(hash_kind, admission, objects)?;
    let mut staged = Vec::with_capacity(objects.len());
    for object in objects {
        admission.refresh_elapsed();
        admission::admit_ingest(admission, object.payload.len() as u64)?;
        if object.payload.len() as u64 > TracesProtocolLimits::MAX_OBJECT_BYTES {
            return Err(budget_exceeded());
        }
        if is_delta(&object.object_type) {
            return Err(rejected());
        }
        let next = budget
            .expanded_bytes
            .checked_add(object.payload.len() as u64)
            .ok_or_else(budget_exceeded)?;
        budget.expanded_bytes = next;
        admit(budget)?;
        let oid = trace_object_storage::verify_declared_oid(
            hash_kind,
            &object.object_type,
            &object.payload,
            &object.declared_oid,
        )?;
        trace_object_storage::put_staging(store, ingest_id, hash_kind, &oid, &object.payload)
            .await?;
        staged.push(oid);
    }
    admission.record_wall_clock();
    admission::admit_ingest(admission, 0)?;
    Ok(staged)
}

fn screen_complexity(
    hash_kind: &str,
    admission: &mut IngestAdmission,
    objects: &[LooseObject],
) -> Result<(), MegaError> {
    if admission.is_cancelled() {
        admission.cancel();
        return Err(MegaError::Traces(TracesError::QueryTimeout));
    }
    let batch_objects = u64::try_from(objects.len()).map_err(|_| budget_exceeded())?;
    let batch_commits = objects
        .iter()
        .filter(|object| object.object_type == "commit")
        .count();
    let batch_commits = u64::try_from(batch_commits).map_err(|_| budget_exceeded())?;
    let next_objects = admission
        .objects
        .checked_add(batch_objects)
        .ok_or_else(budget_exceeded)?;
    let next_commits = admission
        .new_commits
        .checked_add(batch_commits)
        .ok_or_else(budget_exceeded)?;
    if next_objects > TracesProtocolLimits::MAX_OBJECTS
        || next_commits > TracesProtocolLimits::MAX_NEW_COMMITS
    {
        return Err(budget_exceeded());
    }
    let mut largest = 0u64;
    for object in objects {
        largest = largest.max(object.payload.len() as u64);
    }
    if largest > TracesProtocolLimits::MAX_OBJECT_BYTES {
        return Err(budget_exceeded());
    }
    let mut preview = admission.clone();
    preview.objects = next_objects;
    preview.new_commits = next_commits;
    admission::admit_ingest(&preview, largest)?;
    let tree_depth = tree_depth(hash_kind, objects)?;
    let delta_depth = delta_depth(hash_kind, objects)?;
    preview.delta_depth = admission.delta_depth.max(delta_depth);
    preview.tree_depth = admission.tree_depth.max(tree_depth);
    admission::admit_ingest(&preview, largest)?;
    admission.objects = preview.objects;
    admission.new_commits = preview.new_commits;
    admission.delta_depth = preview.delta_depth;
    admission.tree_depth = preview.tree_depth;
    Ok(())
}

fn tree_depth(hash_kind: &str, objects: &[LooseObject]) -> Result<u64, MegaError> {
    if !objects.iter().any(|object| object.object_type == "tree") {
        return Ok(0);
    }
    let hash_len = hash_width(hash_kind)?;
    let mut index = HashMap::new();
    let mut starts = Vec::new();
    for (idx, object) in objects.iter().enumerate() {
        if object.object_type != "tree" {
            continue;
        }
        let Some(raw) = decode_oid(&object.declared_oid, hash_len) else {
            continue;
        };
        index.insert(raw, idx);
        starts.push(idx);
    }
    if starts.is_empty() {
        return Ok(0);
    }
    bounded_depth(&starts, TracesProtocolLimits::MAX_TREE_DEPTH, |idx| {
        in_batch_tree_children(&objects[idx].payload, hash_len, &index)
    })
}

fn delta_depth(hash_kind: &str, objects: &[LooseObject]) -> Result<u64, MegaError> {
    if !objects.iter().any(|object| object.object_type == "commit") {
        return Ok(0);
    }
    let oid_hex_len = hash_width(hash_kind)? * 2;
    let mut index = HashMap::new();
    let mut starts = Vec::new();
    for (idx, object) in objects.iter().enumerate() {
        if object.object_type != "commit" {
            continue;
        }
        index.insert(object.declared_oid.to_ascii_lowercase(), idx);
        starts.push(idx);
    }
    bounded_depth(&starts, TracesProtocolLimits::MAX_DELTA_DEPTH, |idx| {
        in_batch_commit_parents(&objects[idx].payload, oid_hex_len, &index)
    })
}

enum ChainDepth {
    Exact(u64),
    Over,
}

/// Longest in-batch chain. The walk stops once `frames_left` is spent, and it
/// asks for one node's in-batch links at a time instead of retaining every
/// payload record.
fn bounded_depth(
    starts: &[usize],
    limit: u64,
    mut links: impl FnMut(usize) -> Result<Vec<usize>, MegaError>,
) -> Result<u64, MegaError> {
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    let mut deepest = 0u64;
    for &start in starts {
        visiting.clear();
        match depth_at(
            start,
            limit.saturating_add(1),
            &mut links,
            &mut memo,
            &mut visiting,
        )? {
            ChainDepth::Over => return Ok(limit.saturating_add(1)),
            ChainDepth::Exact(height) if height > limit => return Ok(height),
            ChainDepth::Exact(height) => deepest = deepest.max(height),
        }
    }
    Ok(deepest)
}

fn depth_at(
    idx: usize,
    frames_left: u64,
    links: &mut impl FnMut(usize) -> Result<Vec<usize>, MegaError>,
    memo: &mut HashMap<usize, u64>,
    visiting: &mut HashSet<usize>,
) -> Result<ChainDepth, MegaError> {
    if frames_left == 0 {
        return Ok(ChainDepth::Over);
    }
    if let Some(height) = memo.get(&idx) {
        return Ok(ChainDepth::Exact(*height));
    }
    if !visiting.insert(idx) {
        return Err(budget_exceeded());
    }
    let mut height = 1u64;
    for child in links(idx)? {
        match depth_at(child, frames_left - 1, links, memo, visiting)? {
            ChainDepth::Over => {
                visiting.remove(&idx);
                return Ok(ChainDepth::Over);
            }
            ChainDepth::Exact(child_height) => {
                height = height.max(child_height.saturating_add(1));
            }
        }
    }
    visiting.remove(&idx);
    memo.insert(idx, height);
    Ok(ChainDepth::Exact(height))
}

fn in_batch_tree_children(
    payload: &[u8],
    hash_len: usize,
    index: &HashMap<Vec<u8>, usize>,
) -> Result<Vec<usize>, MegaError> {
    let mut seen = HashSet::new();
    let mut children = Vec::new();
    let mut rest = payload;
    while !rest.is_empty() {
        let Some(nul) = rest.iter().position(|byte| *byte == 0) else {
            return Err(rejected());
        };
        let hash_end = nul.checked_add(1 + hash_len).ok_or_else(rejected)?;
        if hash_end > rest.len() || nul == 0 || !rest[..nul].contains(&b' ') {
            return Err(rejected());
        }
        if let Some(&child) = index.get(&rest[nul + 1..hash_end])
            && seen.insert(child)
        {
            children.push(child);
        }
        rest = &rest[hash_end..];
    }
    Ok(children)
}

fn in_batch_commit_parents(
    payload: &[u8],
    oid_hex_len: usize,
    index: &HashMap<String, usize>,
) -> Result<Vec<usize>, MegaError> {
    let mut seen = HashSet::new();
    let mut parents = Vec::new();
    let header_end = payload
        .windows(2)
        .position(|window| window == b"\n\n")
        .unwrap_or(payload.len());
    for line in payload[..header_end].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, |byte| *byte == b' ');
        let label = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        if label != b"parent" {
            continue;
        }
        if value.len() != oid_hex_len || !value.iter().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(rejected());
        }
        let mut lowered = [0u8; 64];
        for (slot, byte) in lowered.iter_mut().zip(value) {
            *slot = byte.to_ascii_lowercase();
        }
        let key = std::str::from_utf8(&lowered[..value.len()]).map_err(|_| rejected())?;
        if let Some(&parent) = index.get(key)
            && seen.insert(parent)
        {
            parents.push(parent);
        }
    }
    Ok(parents)
}

fn hash_width(hash_kind: &str) -> Result<usize, MegaError> {
    match hash_kind {
        "sha1" => Ok(20),
        "sha256" => Ok(32),
        _ => Err(rejected()),
    }
}

fn decode_oid(hex: &str, hash_len: usize) -> Option<Vec<u8>> {
    if hex.len() != hash_len * 2 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut raw = Vec::with_capacity(hash_len);
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_nibble(bytes[index])?;
        let lo = hex_nibble(bytes[index + 1])?;
        raw.push((hi << 4) | lo);
        index += 2;
    }
    Some(raw)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_delta(object_type: &str) -> bool {
    matches!(
        object_type,
        "ref-delta" | "ofs-delta" | "hash-delta" | "offset-delta"
    )
}

/// Every OID that will become visible must already have bytes on this stream.
pub async fn require_durable_oids(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    oids: &[String],
) -> Result<(), MegaError> {
    for oid in oids {
        if get_raw(store, stream_id, hash_kind, oid).await?.is_none() {
            return Err(rejected());
        }
    }
    Ok(())
}

/// Every referenced OID must already be in this unpack or this stream.
/// Another stream, and the `git` namespace, are not bases.
pub async fn require_closure(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    objects: &[LooseObject],
) -> Result<(), MegaError> {
    let mut have = HashSet::new();
    for object in objects {
        have.insert(trace_object_storage::verify_declared_oid(
            hash_kind,
            &object.object_type,
            &object.payload,
            &object.declared_oid,
        )?);
    }
    for object in objects {
        for need in referenced_oids(hash_kind, &object.object_type, &object.payload)? {
            if have.contains(&need) {
                continue;
            }
            if get_raw(store, stream_id, hash_kind, &need).await?.is_some() {
                have.insert(need);
                continue;
            }
            return Err(rejected());
        }
    }
    enforce_capture_document_sizes(store, stream_id, hash_kind, objects).await
}

#[derive(Clone)]
struct TreeLink {
    is_tree: bool,
    name: Vec<u8>,
    oid: String,
}

struct RoleWalk {
    trees: HashMap<String, Option<std::sync::Arc<Vec<TreeLink>>>>,
    blob_lens: HashMap<String, Option<u64>>,
    checked_trees: HashSet<(u8, String)>,
    checked_blobs: HashSet<String>,
}

/// After closure, every new commit's `checkpoint/<prefix>/<rest>/` metadata
/// and manifest blobs must sit inside their raw-byte limits. A same-named
/// file anywhere else is not one of those roles.
async fn enforce_capture_document_sizes(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    objects: &[LooseObject],
) -> Result<(), MegaError> {
    let hash_len = hash_width(hash_kind)?;
    let mut by_oid = HashMap::new();
    for object in objects {
        by_oid.insert(object.declared_oid.to_ascii_lowercase(), object);
    }
    let mut walk = RoleWalk {
        trees: HashMap::new(),
        blob_lens: HashMap::new(),
        checked_trees: HashSet::new(),
        checked_blobs: HashSet::new(),
    };
    for object in objects {
        if object.object_type != "commit" {
            continue;
        }
        let Some(tree_oid) = commit_tree_oid(&object.payload, hash_len * 2)? else {
            continue;
        };
        check_commit_tree(store, stream_id, hash_kind, &by_oid, &mut walk, &tree_oid).await?;
    }
    Ok(())
}

async fn check_commit_tree(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    by_oid: &HashMap<String, &LooseObject>,
    walk: &mut RoleWalk,
    tree_oid: &str,
) -> Result<(), MegaError> {
    if !walk.checked_trees.insert((0, tree_oid.to_owned())) {
        return Ok(());
    }
    let Some(root) =
        tree_links_cached(store, stream_id, hash_kind, by_oid, walk, tree_oid, false).await?
    else {
        return Ok(());
    };
    let checkpoint_oids: Vec<String> = root
        .iter()
        .filter(|link| link.is_tree && link.name == b"checkpoint")
        .map(|link| link.oid.clone())
        .collect();
    for checkpoint_oid in checkpoint_oids {
        if !walk.checked_trees.insert((1, checkpoint_oid.clone())) {
            continue;
        }
        let Some(checkpoint_links) = tree_links_cached(
            store,
            stream_id,
            hash_kind,
            by_oid,
            walk,
            &checkpoint_oid,
            true,
        )
        .await?
        else {
            continue;
        };
        let prefix_oids: Vec<String> = checkpoint_links
            .iter()
            .filter(|link| link.is_tree)
            .map(|link| link.oid.clone())
            .collect();
        for prefix_oid in prefix_oids {
            if !walk.checked_trees.insert((2, prefix_oid.clone())) {
                continue;
            }
            let Some(prefix_links) =
                tree_links_cached(store, stream_id, hash_kind, by_oid, walk, &prefix_oid, true)
                    .await?
            else {
                continue;
            };
            let rest_oids: Vec<String> = prefix_links
                .iter()
                .filter(|link| link.is_tree)
                .map(|link| link.oid.clone())
                .collect();
            for rest_oid in rest_oids {
                if !walk.checked_trees.insert((3, rest_oid.clone())) {
                    continue;
                }
                let Some(leaf) =
                    tree_links_cached(store, stream_id, hash_kind, by_oid, walk, &rest_oid, true)
                        .await?
                else {
                    continue;
                };
                let roles: Vec<(String, String)> = leaf
                    .iter()
                    .filter(|item| !item.is_tree)
                    .filter_map(|item| {
                        let role = match item.name.as_slice() {
                            b"metadata.json" => "metadata",
                            b"manifest.json" => "manifest",
                            _ => return None,
                        };
                        Some((role.to_owned(), item.oid.clone()))
                    })
                    .collect();
                for (role, oid) in roles {
                    if !walk.checked_blobs.insert(oid.clone()) {
                        continue;
                    }
                    let Some(raw_bytes) =
                        blob_len_cached(store, stream_id, hash_kind, by_oid, walk, &oid).await?
                    else {
                        continue;
                    };
                    admission::admit_raw_capture_role(&role, raw_bytes)?;
                }
            }
        }
    }
    Ok(())
}

async fn tree_links_cached(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    by_oid: &HashMap<String, &LooseObject>,
    walk: &mut RoleWalk,
    oid: &str,
    strict: bool,
) -> Result<Option<std::sync::Arc<Vec<TreeLink>>>, MegaError> {
    if let Some(cached) = walk.trees.get(oid) {
        return Ok(cached.clone());
    }
    let hash_len = hash_width(hash_kind)?;
    let Some(links) =
        load_tree_links(store, stream_id, hash_kind, hash_len, by_oid, oid, strict).await?
    else {
        return Ok(None);
    };
    let links = std::sync::Arc::new(links);
    walk.trees.insert(oid.to_owned(), Some(links.clone()));
    Ok(Some(links))
}

async fn load_tree_links(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    hash_len: usize,
    by_oid: &HashMap<String, &LooseObject>,
    oid: &str,
    strict: bool,
) -> Result<Option<Vec<TreeLink>>, MegaError> {
    if let Some(object) = by_oid.get(oid) {
        if object.object_type != "tree" {
            return if strict { Err(rejected()) } else { Ok(None) };
        }
        return Ok(Some(tree_links(&object.payload, hash_len)?));
    }
    let Some(bytes) = get_raw(store, stream_id, hash_kind, oid).await? else {
        return Ok(None);
    };
    if trace_object_storage::verify_declared_oid(hash_kind, "tree", &bytes, oid).is_err() {
        return if strict { Err(rejected()) } else { Ok(None) };
    }
    Ok(Some(tree_links(&bytes, hash_len)?))
}

async fn blob_len_cached(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    by_oid: &HashMap<String, &LooseObject>,
    walk: &mut RoleWalk,
    oid: &str,
) -> Result<Option<u64>, MegaError> {
    if let Some(cached) = walk.blob_lens.get(oid) {
        return Ok(*cached);
    }
    let len = if let Some(object) = by_oid.get(oid) {
        if object.object_type != "blob" {
            return Err(rejected());
        }
        Some(object.payload.len() as u64)
    } else if let Some(bytes) = get_raw(store, stream_id, hash_kind, oid).await? {
        if trace_object_storage::verify_declared_oid(hash_kind, "blob", &bytes, oid).is_err() {
            return Err(rejected());
        }
        Some(bytes.len() as u64)
    } else {
        None
    };
    walk.blob_lens.insert(oid.to_owned(), len);
    Ok(len)
}

fn commit_tree_oid(payload: &[u8], oid_hex_len: usize) -> Result<Option<String>, MegaError> {
    let header_end = payload
        .windows(2)
        .position(|window| window == b"\n\n")
        .unwrap_or(payload.len());
    for line in payload[..header_end].split(|byte| *byte == b'\n') {
        let mut parts = line.splitn(2, |byte| *byte == b' ');
        let label = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        if label != b"tree" {
            continue;
        }
        if value.len() != oid_hex_len || !value.iter().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(rejected());
        }
        return Ok(Some(String::from_utf8_lossy(value).to_ascii_lowercase()));
    }
    Ok(None)
}

fn tree_links(payload: &[u8], hash_len: usize) -> Result<Vec<TreeLink>, MegaError> {
    let mut links = Vec::new();
    let mut rest = payload;
    while !rest.is_empty() {
        let Some(nul) = rest.iter().position(|byte| *byte == 0) else {
            return Err(rejected());
        };
        let hash_end = nul.checked_add(1 + hash_len).ok_or_else(rejected)?;
        if hash_end > rest.len() || nul == 0 || !rest[..nul].contains(&b' ') {
            return Err(rejected());
        }
        let header = &rest[..nul];
        let space = header
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(rejected)?;
        let mode = &header[..space];
        let name = header[space + 1..].to_vec();
        links.push(TreeLink {
            is_tree: mode == b"40000" || mode == b"040000",
            name,
            oid: encode_hex(&rest[nul + 1..hash_end]),
        });
        rest = &rest[hash_end..];
    }
    Ok(links)
}

fn referenced_oids(
    hash_kind: &str,
    object_type: &str,
    payload: &[u8],
) -> Result<Vec<String>, MegaError> {
    let oid_hex_len = match hash_kind {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Err(rejected()),
    };
    let hash_len = oid_hex_len / 2;
    match object_type {
        "blob" => Ok(Vec::new()),
        "commit" => header_oids(payload, &["tree", "parent"], oid_hex_len),
        "tag" => header_oids(payload, &["object"], oid_hex_len),
        "tree" => tree_oids(payload, hash_len),
        _ => Err(rejected()),
    }
}

fn header_oids(
    payload: &[u8],
    labels: &[&str],
    oid_hex_len: usize,
) -> Result<Vec<String>, MegaError> {
    let header_end = payload
        .windows(2)
        .position(|window| window == b"\n\n")
        .unwrap_or(payload.len());
    let mut refs = Vec::new();
    for line in payload[..header_end].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, |byte| *byte == b' ');
        let label = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        if !labels.iter().any(|candidate| candidate.as_bytes() == label) {
            continue;
        }
        if value.len() != oid_hex_len || !value.iter().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(rejected());
        }
        refs.push(String::from_utf8_lossy(value).to_ascii_lowercase());
    }
    Ok(refs)
}

fn tree_oids(payload: &[u8], hash_len: usize) -> Result<Vec<String>, MegaError> {
    let mut refs = Vec::new();
    let mut rest = payload;
    while !rest.is_empty() {
        let Some(nul) = rest.iter().position(|byte| *byte == 0) else {
            return Err(rejected());
        };
        let hash_end = nul.checked_add(1 + hash_len).ok_or_else(rejected)?;
        if hash_end > rest.len() || nul == 0 || !rest[..nul].contains(&b' ') {
            return Err(rejected());
        }
        refs.push(encode_hex(&rest[nul + 1..hash_end]));
        rest = &rest[hash_end..];
    }
    Ok(refs)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use git_internal::{
        hash::{HashKind, ObjectHash},
        internal::object::types::ObjectType,
    };

    use super::*;
    use crate::jupiter::storage::{
        object_storage::mock_object_storage,
        trace_object_storage::{get_raw, get_staging, put_raw},
    };

    const STREAM_A: &str = "11111111-1111-4111-8111-111111111111";
    const STREAM_B: &str = "22222222-2222-4222-8222-222222222222";
    const INGEST: &str = "33333333-3333-4333-8333-333333333333";

    fn oid(kind: HashKind, object_type: ObjectType, payload: &[u8]) -> String {
        ObjectHash::from_type_and_data_for_kind(kind, object_type, payload)
            .expect("oid")
            .to_string()
    }

    fn loose(object_type: &str, declared_oid: &str, payload: &[u8]) -> LooseObject {
        LooseObject {
            object_type: object_type.to_owned(),
            declared_oid: declared_oid.to_owned(),
            payload: payload.to_vec(),
        }
    }

    #[tokio::test]
    async fn quarantine_recovery_stages_verified_objects_and_stops_on_budget() {
        let store = mock_object_storage();
        let payload = b"hello-traces";
        let good = oid(HashKind::Sha1, ObjectType::Blob, payload);
        let staged = unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: TracesProtocolLimits::MAX_PACK_BYTES,
                expanded_bytes: 0,
            },
            &[loose("blob", &good, payload)],
        )
        .await
        .expect("stage");
        assert_eq!(staged, vec![good.clone()]);
        assert_eq!(
            get_staging(&store, INGEST, "sha1", &good)
                .await
                .expect("staging read"),
            Some(payload.to_vec())
        );
        assert_eq!(
            get_raw(&store, STREAM_A, "sha1", &good)
                .await
                .expect("durable read"),
            None
        );
        let key = trace_object_storage::staging_object_key(INGEST, "sha1", &good).expect("key");
        assert!(key.key.starts_with(&format!("v1/staging/{INGEST}/")));
        assert!(!key.key.contains(&format!("v1/{STREAM_A}/")));

        let over_pack = unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: TracesProtocolLimits::MAX_PACK_BYTES + 1,
                expanded_bytes: 0,
            },
            &[loose("blob", &good, payload)],
        )
        .await;
        assert!(over_pack.is_err(), "compressed bytes stop at 512MiB");
        let over_expanded = unpack_into_quarantine(
            &store,
            "44444444-4444-4444-8444-444444444444",
            "sha1",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: TracesProtocolLimits::MAX_DECODED_BYTES,
            },
            &[loose("blob", &good, payload)],
        )
        .await;
        assert!(over_expanded.is_err(), "expanded bytes stop at 1GiB");
        assert_eq!(
            get_staging(
                &store,
                "44444444-4444-4444-8444-444444444444",
                "sha1",
                &good
            )
            .await
            .expect("over-budget staging"),
            None
        );

        let bad = unpack_into_quarantine(
            &store,
            "55555555-5555-4555-8555-555555555555",
            "sha1",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: 0,
            },
            &[loose("blob", &"ab".repeat(20), payload)],
        )
        .await;
        assert!(bad.is_err(), "declared sha1 must match the payload");

        let sha256_payload = b"sha256-body";
        let sha256 = oid(HashKind::Sha256, ObjectType::Blob, sha256_payload);
        unpack_into_quarantine(
            &store,
            INGEST,
            "sha256",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: 0,
            },
            &[loose("blob", &sha256, sha256_payload)],
        )
        .await
        .expect("sha256 stage");
        assert_eq!(
            get_staging(&store, INGEST, "sha256", &sha256)
                .await
                .expect("sha256 staging"),
            Some(sha256_payload.to_vec())
        );

        let delta = unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: 1,
                expanded_bytes: 0,
            },
            &[loose("ref-delta", &good, payload)],
        )
        .await;
        assert!(
            delta.is_err(),
            "a thin delta is not resolved from another scope"
        );
    }

    #[tokio::test]
    async fn quarantine_recovery_rejects_a_missing_or_cross_scope_base() {
        let store = mock_object_storage();
        let blob = b"base-blob";
        let blob_oid = oid(HashKind::Sha1, ObjectType::Blob, blob);
        let commit = format!("tree {blob_oid}\n\nmessage\n");
        let commit_oid = oid(HashKind::Sha1, ObjectType::Commit, commit.as_bytes());
        let objects = [loose("commit", &commit_oid, commit.as_bytes())];
        unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            UnpackBudget {
                compressed_bytes: commit.len() as u64,
                expanded_bytes: 0,
            },
            &objects,
        )
        .await
        .expect("stage commit");
        assert!(
            require_closure(&store, STREAM_A, "sha1", &objects)
                .await
                .is_err(),
            "the tree is not in this quarantine"
        );

        put_raw(&store, STREAM_B, "sha1", &blob_oid, blob)
            .await
            .expect("other stream");
        assert!(
            require_closure(&store, STREAM_A, "sha1", &objects)
                .await
                .is_err(),
            "another stream is not a base"
        );
        assert_eq!(
            get_raw(&store, STREAM_B, "sha1", &blob_oid)
                .await
                .expect("other read"),
            Some(blob.to_vec())
        );

        let closed = [
            loose("blob", &blob_oid, blob),
            loose("commit", &commit_oid, commit.as_bytes()),
        ];
        require_closure(&store, STREAM_A, "sha1", &closed)
            .await
            .expect("same quarantine closes the tree");
        put_raw(&store, STREAM_A, "sha1", &blob_oid, blob)
            .await
            .expect("same stream");
        require_closure(&store, STREAM_A, "sha1", &objects)
            .await
            .expect("this stream's archived blob closes the tree");
    }

    fn tree_pointing(child_oid: &str) -> Vec<u8> {
        let raw = hex_decode(child_oid);
        let mut bytes = b"40000 d".to_vec();
        bytes.push(0);
        bytes.extend(raw);
        bytes
    }

    fn hex_decode(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex"))
            .collect()
    }

    fn tree_chain(levels: usize) -> Vec<LooseObject> {
        let mut objects = Vec::with_capacity(levels);
        let mut child: Option<String> = None;
        for _ in 0..levels {
            let payload = match &child {
                None => Vec::new(),
                Some(oid) => tree_pointing(oid),
            };
            let declared = oid(HashKind::Sha1, ObjectType::Tree, &payload);
            objects.push(loose("tree", &declared, &payload));
            child = Some(declared);
        }
        objects
    }

    fn commit_chain(levels: usize) -> Vec<LooseObject> {
        let tree = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut objects = Vec::with_capacity(levels);
        let mut parent: Option<String> = None;
        for _ in 0..levels {
            let payload = match &parent {
                None => format!("tree {tree}\n\nmessage\n"),
                Some(oid) => format!("tree {tree}\nparent {oid}\n\nmessage\n"),
            };
            let declared = oid(HashKind::Sha1, ObjectType::Commit, payload.as_bytes());
            objects.push(loose("commit", &declared, payload.as_bytes()));
            parent = Some(declared);
        }
        objects
    }

    #[tokio::test]
    async fn ingest_resource_budget_stops_before_staging_or_publish() {
        use sea_orm::{
            ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set,
        };
        use uuid::Uuid;

        use crate::{
            callisto::{trace_object, trace_principal, trace_stream, trace_target},
            ceres::traces::admission::IngestAdmission,
            jupiter::{
                migration::apply_migrations,
                service::trace_publish_service::TracePublishService,
                storage::trace_stream_storage::{CommitParentList, PublishRef, TRACES_REF},
                tests::test_db_connection,
            },
        };

        let store = mock_object_storage();
        let payload = b"budget-blob";
        let good = oid(HashKind::Sha1, ObjectType::Blob, payload);
        let object = loose("blob", &good, payload);
        let budget = UnpackBudget {
            compressed_bytes: 1,
            expanded_bytes: 0,
        };

        let mut too_many = IngestAdmission::start();
        too_many.objects = TracesProtocolLimits::MAX_OBJECTS;
        assert!(
            unpack_bounded(
                &store,
                INGEST,
                "sha1",
                budget,
                &mut too_many,
                std::slice::from_ref(&object)
            )
            .await
            .is_err()
        );
        assert_eq!(
            get_staging(&store, INGEST, "sha1", &good)
                .await
                .expect("object budget"),
            None
        );

        let mut too_many_commits = IngestAdmission::start();
        too_many_commits.new_commits = TracesProtocolLimits::MAX_NEW_COMMITS;
        let one_commit = commit_chain(1);
        assert!(
            unpack_bounded(
                &store,
                INGEST,
                "sha1",
                budget,
                &mut too_many_commits,
                &one_commit
            )
            .await
            .is_err()
        );

        let at_tree_limit = tree_chain(TracesProtocolLimits::MAX_TREE_DEPTH as usize);
        let at_tree_tip = at_tree_limit
            .last()
            .expect("tree at the limit")
            .declared_oid
            .clone();
        unpack_into_quarantine(&store, INGEST, "sha1", budget, &at_tree_limit)
            .await
            .expect("tree depth limit is inclusive");
        assert!(
            get_staging(&store, INGEST, "sha1", &at_tree_tip)
                .await
                .expect("inclusive tree")
                .is_some(),
            "a tree at the depth limit is staged"
        );
        let deep_trees = tree_chain(TracesProtocolLimits::MAX_TREE_DEPTH as usize + 1);
        assert!(
            unpack_into_quarantine(&store, INGEST, "sha1", budget, &deep_trees)
                .await
                .is_err(),
            "tree depth"
        );
        let at_commit_limit = commit_chain(TracesProtocolLimits::MAX_DELTA_DEPTH as usize);
        unpack_into_quarantine(&store, INGEST, "sha1", budget, &at_commit_limit)
            .await
            .expect("delta depth limit is inclusive");
        let deep_commits = commit_chain(TracesProtocolLimits::MAX_DELTA_DEPTH as usize + 1);
        assert!(
            unpack_into_quarantine(&store, INGEST, "sha1", budget, &deep_commits)
                .await
                .is_err(),
            "delta depth"
        );

        let second_payload = b"budget-blob-2";
        let second = oid(HashKind::Sha1, ObjectType::Blob, second_payload);
        let mut during = IngestAdmission::start();
        during.record_elapsed(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS - 1);
        during.seconds_per_object = 1;
        let during_ingest = "66666666-6666-4666-8666-666666666666";
        let staged_first = unpack_bounded(
            &store,
            during_ingest,
            "sha1",
            budget,
            &mut during,
            &[object.clone(), loose("blob", &second, second_payload)],
        )
        .await;
        assert!(staged_first.is_err(), "deadline fires on the next object");
        assert_eq!(
            get_staging(&store, during_ingest, "sha1", &good)
                .await
                .expect("first object staged before the deadline"),
            Some(payload.to_vec())
        );
        assert_eq!(
            get_staging(&store, during_ingest, "sha1", &second)
                .await
                .expect("second object"),
            None
        );

        let mut late = IngestAdmission::start();
        late.record_elapsed(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS + 1);
        assert!(
            unpack_bounded(
                &store,
                INGEST,
                "sha1",
                budget,
                &mut late,
                std::slice::from_ref(&object)
            )
            .await
            .is_err(),
            "deadline"
        );

        let mut cancelled = IngestAdmission::start();
        cancelled.cancel();
        assert!(
            unpack_bounded(
                &store,
                INGEST,
                "sha1",
                budget,
                &mut cancelled,
                std::slice::from_ref(&object)
            )
            .await
            .is_err(),
            "cancel"
        );
        assert_eq!(cancelled.objects, 0);
        assert!(cancelled.is_cancelled());
        assert_eq!(
            get_staging(&store, INGEST, "sha1", &good)
                .await
                .expect("cancelled staging"),
            None
        );

        let huge = vec![0u8; TracesProtocolLimits::MAX_OBJECT_BYTES as usize + 1];
        let huge_object = loose("blob", &"ab".repeat(20), &huge);
        let huge_err = unpack_into_quarantine(
            &store,
            INGEST,
            "sha1",
            budget,
            std::slice::from_ref(&huge_object),
        )
        .await
        .expect_err("object size");
        assert!(
            matches!(huge_err, MegaError::Traces(TracesError::ResponseBudget)),
            "object size is a budget failure before parsing"
        );
        assert_eq!(
            get_staging(&store, INGEST, "sha1", &"ab".repeat(20))
                .await
                .expect("huge staging"),
            None
        );

        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/team/{target_id}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("target");
        let credential_id = format!("publish-{target_id}");
        crate::callisto::trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            namespace: Set("static_push_token".to_owned()),
            credential_id: Set(credential_id.clone()),
            created_at: Set(now),
            retired_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("binding");
        let stream_id = trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            target_id: Set(target_id.clone()),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("stream")
        .id;
        let path = format!("/team/{target_id}");
        let token = crate::config::PushTokenConfig {
            name: credential_id.clone(),
            token: "synthetic-secret".to_owned(),
            paths: Some(vec![path.clone()]),
        };
        let authority = crate::ceres::traces::auth::PublishAuthority {
            caller: crate::ceres::traces::auth::TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token: &token,
            },
            principal_id: &principal_id,
            target_id: &target_id,
            authorized_path: &path,
            expected_generation: 1,
            policy: None,
        };
        let service = TracePublishService::new(store);
        let absent = "dddddddddddddddddddddddddddddddddddddddd";
        assert!(
            service
                .publish_quarantine(
                    &db,
                    INGEST,
                    budget,
                    &[huge_object],
                    &PublishRef {
                        stream_id: stream_id.clone(),
                        ref_names: vec![TRACES_REF.to_owned()],
                        delete: false,
                        new_tip: absent.to_owned(),
                        commits: vec![CommitParentList {
                            oid: absent.to_owned(),
                            parents: Vec::new(),
                        }],
                        checkpoints: Vec::new(),
                        object_oids: vec![absent.to_owned()],
                        hash_kind: "sha1".to_owned(),
                        idempotency_key: "over-budget".to_owned(),
                        schema_id: "manifest-v1".to_owned(),
                    },
                    &authority,
                )
                .await
                .is_err()
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&stream_id))
                .count(&db)
                .await
                .expect("membership"),
            0
        );
        assert!(
            trace_stream::Entity::find_by_id(&stream_id)
                .one(&db)
                .await
                .expect("stream")
                .expect("row")
                .tip_oid
                .is_none()
        );
    }

    fn git_entry(mode: &str, name: &str, oid_hex: &str) -> Vec<u8> {
        let mut bytes = format!("{mode} {name}").into_bytes();
        bytes.push(0);
        bytes.extend(hex_decode(oid_hex));
        bytes
    }

    fn blob_loose(payload: &[u8]) -> LooseObject {
        let declared = oid(HashKind::Sha1, ObjectType::Blob, payload);
        loose("blob", &declared, payload)
    }

    fn tree_loose(payload: &[u8]) -> LooseObject {
        let declared = oid(HashKind::Sha1, ObjectType::Tree, payload);
        loose("tree", &declared, payload)
    }

    fn commit_loose(tree_oid: &str, parent: Option<&str>) -> LooseObject {
        let payload = match parent {
            None => format!("tree {tree_oid}\n\ncheckpoint\n"),
            Some(parent) => format!("tree {tree_oid}\nparent {parent}\n\ncheckpoint\n"),
        };
        let declared = oid(HashKind::Sha1, ObjectType::Commit, payload.as_bytes());
        loose("commit", &declared, payload.as_bytes())
    }

    fn checkpoint_objects(
        metadata: &[u8],
        manifest: &[u8],
        parent: Option<&str>,
        decoy: Option<&[u8]>,
    ) -> Vec<LooseObject> {
        let meta = blob_loose(metadata);
        let manifest_blob = blob_loose(manifest);
        let mut leaf_bytes = git_entry("100644", "metadata.json", &meta.declared_oid);
        leaf_bytes.extend(git_entry(
            "100644",
            "manifest.json",
            &manifest_blob.declared_oid,
        ));
        let leaf = tree_loose(&leaf_bytes);
        let prefix = tree_loose(&git_entry("40000", "cdef", &leaf.declared_oid));
        let checkpoint = tree_loose(&git_entry("40000", "ab", &prefix.declared_oid));
        let mut root_bytes = git_entry("40000", "checkpoint", &checkpoint.declared_oid);
        let mut objects = vec![meta, manifest_blob, leaf, prefix, checkpoint];
        if let Some(decoy_bytes) = decoy {
            let decoy_blob = blob_loose(decoy_bytes);
            root_bytes.extend(git_entry(
                "100644",
                "metadata.json",
                &decoy_blob.declared_oid,
            ));
            objects.push(decoy_blob);
        }
        let root = tree_loose(&root_bytes);
        let commit = commit_loose(&root.declared_oid, parent);
        objects.push(root);
        objects.push(commit);
        objects
    }

    #[tokio::test]
    async fn capture_document_publication_rejects_an_oversized_middle_commit() {
        use sea_orm::{
            ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set,
        };
        use uuid::Uuid;

        use crate::{
            callisto::{
                trace_object, trace_principal, trace_ref_update, trace_stream, trace_target,
            },
            ceres::traces::auth::PublishAuthority,
            jupiter::{
                migration::apply_migrations,
                service::trace_publish_service::TracePublishService,
                storage::trace_stream_storage::{CommitParentList, PublishRef, TRACES_REF},
                tests::test_db_connection,
            },
        };

        let store = mock_object_storage();
        let decoy = vec![7u8; TracesProtocolLimits::MAX_METADATA_BYTES as usize + 1];
        let allowed = checkpoint_objects(b"ok", b"{}", None, Some(&decoy));
        require_closure(&store, STREAM_A, "sha1", &allowed)
            .await
            .expect("a root metadata.json is not the checkpoint role");

        let over = vec![9u8; TracesProtocolLimits::MAX_METADATA_BYTES as usize + 1];
        let middle = checkpoint_objects(&over, b"{}", None, None);
        let middle_oid = middle.last().expect("middle commit").declared_oid.clone();
        let tip = checkpoint_objects(b"tip", b"{}", Some(&middle_oid), None);
        let tip_oid = tip.last().expect("tip commit").declared_oid.clone();
        let mut objects = middle;
        objects.extend(tip);
        assert!(
            require_closure(&store, STREAM_A, "sha1", &objects)
                .await
                .is_err(),
            "the middle commit's metadata is over the role limit"
        );

        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/team/{target_id}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("target");
        let credential_id = format!("publish-{target_id}");
        crate::callisto::trace_credential_binding::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            namespace: Set("static_push_token".to_owned()),
            credential_id: Set(credential_id.clone()),
            created_at: Set(now),
            retired_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("binding");
        let stream_id = trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id.clone()),
            target_id: Set(target_id.clone()),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("stream")
        .id;
        let journal_before = trace_ref_update::Entity::find()
            .filter(trace_ref_update::Column::StreamId.eq(&stream_id))
            .count(&db)
            .await
            .expect("journal before");
        let tip_before = trace_stream::Entity::find_by_id(&stream_id)
            .one(&db)
            .await
            .expect("stream before")
            .expect("row")
            .tip_oid;
        let path = format!("/team/{target_id}");
        let token = crate::config::PushTokenConfig {
            name: credential_id.clone(),
            token: "synthetic-secret".to_owned(),
            paths: Some(vec![path.clone()]),
        };
        let authority = PublishAuthority {
            caller: crate::ceres::traces::auth::TracesCaller::StaticPushToken {
                credential_id: &token.name,
                token: &token,
            },
            principal_id: &principal_id,
            target_id: &target_id,
            authorized_path: &path,
            expected_generation: 1,
            policy: None,
        };
        let commits = objects
            .iter()
            .filter(|object| object.object_type == "commit")
            .map(|object| CommitParentList {
                oid: object.declared_oid.clone(),
                parents: if object.declared_oid == middle_oid {
                    Vec::new()
                } else {
                    vec![middle_oid.clone()]
                },
            })
            .collect();
        let object_oids = objects
            .iter()
            .map(|object| object.declared_oid.clone())
            .collect();
        assert!(
            TracePublishService::new(store)
                .publish_quarantine(
                    &db,
                    "77777777-7777-4777-8777-777777777777",
                    UnpackBudget {
                        compressed_bytes: 1,
                        expanded_bytes: 0,
                    },
                    &objects,
                    &PublishRef {
                        stream_id: stream_id.clone(),
                        ref_names: vec![TRACES_REF.to_owned()],
                        delete: false,
                        new_tip: tip_oid,
                        commits,
                        checkpoints: Vec::new(),
                        object_oids,
                        hash_kind: "sha1".to_owned(),
                        idempotency_key: "capture-size".to_owned(),
                        schema_id: "manifest-v1".to_owned(),
                    },
                    &authority,
                )
                .await
                .is_err()
        );
        assert_eq!(
            trace_ref_update::Entity::find()
                .filter(trace_ref_update::Column::StreamId.eq(&stream_id))
                .count(&db)
                .await
                .expect("journal after"),
            journal_before
        );
        assert_eq!(
            trace_stream::Entity::find_by_id(&stream_id)
                .one(&db)
                .await
                .expect("stream")
                .expect("row")
                .tip_oid,
            tip_before
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::StreamId.eq(&stream_id))
                .count(&db)
                .await
                .expect("membership"),
            0
        );
    }
}
