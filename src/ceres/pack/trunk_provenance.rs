//! Trunk synthetic-commit provenance (plan TP-16 / trunk-push.md 3.1–3.2).
//!
//! `Commit::from_tree_id` is the review-morph CL merge constructor and must
//! not be used for trunk squash / ancestor roll-up / descendant continuation.

use chrono::{FixedOffset, TimeZone};
use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::{
        commit::Commit,
        signature::{Signature, SignatureType},
    },
};

use crate::common::{
    errors::MegaError,
    utils::{commit_body_subject, split_commit_message},
};

/// Server-sign a trunk synthetic commit without rewriting identity.
pub type TrunkMegaSign = std::sync::Arc<dyn Fn(&Commit) -> Result<Commit, MegaError> + Send + Sync>;

/// Author-date span (seconds) above which `Mono-Author-Date-Range` is added.
pub const AUTHOR_DATE_RANGE_THRESHOLD_SECS: usize = 86_400;

/// Inputs shared by every synthetic commit of one B3 push round.
#[derive(Clone, Debug)]
pub struct TrunkProvenance {
    pub author: Signature,
    pub committer_name: String,
    pub committer_email: String,
    pub n: u32,
    pub path_p: String,
    pub squash_commit_id: String,
    pub range: Option<(String, String)>,
    pub land_ts: usize,
    pub n1_message: String,
}

impl TrunkProvenance {
    pub fn from_tip(
        tip: &Commit,
        n: u32,
        path_p: &str,
        squash_commit_id: String,
        range: Option<(String, String)>,
        land_ts: usize,
    ) -> Self {
        Self {
            author: author_from_tip(tip),
            committer_name: tip.committer.name.clone(),
            committer_email: tip.committer.email.clone(),
            n,
            path_p: path_p.to_owned(),
            squash_commit_id,
            range,
            land_ts,
            // git-internal keeps extra headers (e.g. the client's `gpgsig`) in
            // `message`; a synthesized layer commit carries only the body, and
            // the server signature is embedded separately (ADR-FU-01).
            n1_message: split_commit_message(&tip.message).body.to_owned(),
        }
    }

    pub fn committer(&self, prev: Option<&Signature>) -> Signature {
        committer_at_land(
            &self.committer_name,
            &self.committer_email,
            prev,
            self.land_ts,
        )
    }

    pub fn squash_message(&self, topo_asc: &[Commit]) -> String {
        squash_message(
            &self.path_p,
            topo_asc,
            self.range.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
            &self.author,
        )
    }

    pub fn layer_message(&self, ref_path: &str) -> String {
        if self.n <= 1 {
            self.n1_message.clone()
        } else {
            compact_message(
                &self.path_p,
                ref_path,
                self.n,
                &self.squash_commit_id,
                self.range.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
            )
        }
    }
}

pub fn author_from_tip(tip: &Commit) -> Signature {
    Signature {
        signature_type: SignatureType::Author,
        name: tip.author.name.clone(),
        email: tip.author.email.clone(),
        timestamp: tip.author.timestamp,
        timezone: tip.author.timezone.clone(),
    }
}

pub fn committer_at_land(
    name: &str,
    email: &str,
    prev: Option<&Signature>,
    land_ts: usize,
) -> Signature {
    let (timestamp, timezone) = match prev {
        Some(p) if p.timestamp >= land_ts => (p.timestamp, p.timezone.clone()),
        _ => (land_ts, "+0000".to_owned()),
    };
    Signature {
        signature_type: SignatureType::Committer,
        name: name.to_owned(),
        email: email.to_owned(),
        timestamp,
        timezone,
    }
}

/// Legacy constructor that derives the commit OID from the thread-local
/// [`HashKind`]. Kept for callers that already run under the configured kind;
/// B3 trunk landing must use [`synthesize_with_kind`] instead.
pub fn synthesize(
    author: Signature,
    committer: Signature,
    tree: ObjectHash,
    parents: Vec<ObjectHash>,
    message: &str,
) -> Commit {
    Commit::new(author, committer, tree, parents, message)
}

/// Fallible explicit-kind synthetic commit constructor used by every B3 trunk
/// landing path.
///
/// The commit OID is derived from `kind` alone and never from the thread-local
/// [`HashKind`], and `tree` plus every parent must already belong to `kind`.
/// A cross-kind reference therefore fails closed with [`MegaError::Other`]
/// rather than silently producing an OID under the wrong algorithm.
pub fn synthesize_with_kind(
    kind: HashKind,
    author: Signature,
    committer: Signature,
    tree: ObjectHash,
    parents: Vec<ObjectHash>,
    message: &str,
) -> Result<Commit, MegaError> {
    Commit::new_with_kind(kind, author, committer, tree, parents, message).map_err(|error| {
        MegaError::Other(format!(
            "trunk synthesize under {kind:?} rejected a cross-kind reference: {error}"
        ))
    })
}

pub fn squash_message(
    path: &str,
    topo_asc: &[Commit],
    range: Option<(&str, &str)>,
    tip_author: &Signature,
) -> String {
    let n = topo_asc.len();
    let mut body = String::new();
    body.push_str(&format!("Squash {n} commits at {path}\n\n"));
    body.push_str(&format!(
        "This commit was created by mega2. The push carried {n} commits, which\n\
were squashed into this single commit. The original commits are listed\n\
below in topological order; their objects remain retrievable from the\n\
object store via Mono-Squash-Range (omitted for creation pushes without\n\
a baseline — traversal starts from the persisted tip recorded in the\n\
push queue row).\n\n"
    ));
    for c in topo_asc {
        let subject = commit_subject(&c.message);
        body.push_str(&format!(
            "  {}  {}  {} <{}>\n      {}\n",
            c.id,
            format_sig_datetime(&c.author),
            c.author.name,
            c.author.email,
            subject
        ));
    }
    body.push('\n');
    body.push_str(&format!("Mono-Path: {path}\n"));
    body.push_str(&format!("Mono-Ref-Path: {path}\n"));
    if let Some((old, new)) = range {
        body.push_str(&format!("Mono-Squash-Range: {old}..{new}\n"));
    }
    body.push_str(&format!("Mono-Squash-Count: {n}\n"));
    if let Some(span) = author_date_range_trailer(topo_asc) {
        body.push_str(&format!("Mono-Author-Date-Range: {span}\n"));
    }
    for line in co_authored_by_lines(tip_author, topo_asc) {
        body.push_str(&line);
        body.push('\n');
    }
    body
}

pub fn compact_message(
    path_p: &str,
    ref_path: &str,
    n: u32,
    squash_id: &str,
    range: Option<(&str, &str)>,
) -> String {
    let mut body = String::new();
    body.push_str(&format!("Land {n} commits at {path_p}\n\n"));
    body.push_str("Squashed on the pushed path; see Mono-Squash-Commit for the full listing.\n\n");
    body.push_str(&format!("Mono-Path: {path_p}\n"));
    body.push_str(&format!("Mono-Ref-Path: {ref_path}\n"));
    body.push_str(&format!("Mono-Squash-Commit: {squash_id}\n"));
    if let Some((old, new)) = range {
        body.push_str(&format!("Mono-Squash-Range: {old}..{new}\n"));
    }
    body.push_str(&format!("Mono-Squash-Count: {n}\n"));
    body
}

pub fn co_authored_by_lines(tip_author: &Signature, topo_asc: &[Commit]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    seen.insert(identity_key(tip_author));
    let mut lines = Vec::new();
    for c in topo_asc {
        let key = identity_key(&c.author);
        if seen.insert(key) {
            lines.push(format!(
                "Co-authored-by: {} <{}>",
                c.author.name, c.author.email
            ));
        }
    }
    lines
}

fn identity_key(sig: &Signature) -> String {
    format!("{} <{}>", sig.name, sig.email)
}

fn commit_subject(message: &str) -> String {
    commit_body_subject(split_commit_message(message).body).to_owned()
}

fn author_date_range_trailer(topo_asc: &[Commit]) -> Option<String> {
    let mut min_ts = usize::MAX;
    let mut max_ts = 0usize;
    let mut min_sig: Option<&Signature> = None;
    let mut max_sig: Option<&Signature> = None;
    for c in topo_asc {
        if c.author.timestamp < min_ts {
            min_ts = c.author.timestamp;
            min_sig = Some(&c.author);
        }
        if c.author.timestamp > max_ts {
            max_ts = c.author.timestamp;
            max_sig = Some(&c.author);
        }
    }
    if max_ts.saturating_sub(min_ts) < AUTHOR_DATE_RANGE_THRESHOLD_SECS {
        return None;
    }
    Some(format!(
        "{}..{}",
        format_sig_datetime(min_sig?),
        format_sig_datetime(max_sig?)
    ))
}

fn parse_tz_offset_secs(tz: &str) -> i32 {
    let trimmed = tz.trim();
    let (sign, rest) = if let Some(r) = trimmed.strip_prefix('-') {
        (-1, r)
    } else {
        (1, trimmed.trim_start_matches('+'))
    };
    let hours: i32 = rest.get(..2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mins: i32 = rest.get(2..4).and_then(|s| s.parse().ok()).unwrap_or(0);
    sign * (hours * 3600 + mins * 60)
}

fn format_sig_datetime(sig: &Signature) -> String {
    let offset = parse_tz_offset_secs(&sig.timezone);
    let tz = FixedOffset::east_opt(offset).unwrap_or(FixedOffset::east_opt(0).unwrap());
    match chrono::Utc.timestamp_opt(sig.timestamp as i64, 0) {
        chrono::LocalResult::Single(utc) => utc
            .with_timezone(&tz)
            .format("%Y-%m-%d %H:%M:%S %z")
            .to_string(),
        _ => format!("{} {}", sig.timestamp, sig.timezone),
    }
}

/// Walk `payload_ids` (tip-first) into commits; the last element is the tip.
pub fn load_topo_asc(tip_first: &[Commit]) -> Result<Vec<Commit>, MegaError> {
    if tip_first.is_empty() {
        return Err(MegaError::Other(
            "trunk provenance requires a non-empty commit chain".into(),
        ));
    }
    Ok(tip_first.iter().rev().cloned().collect())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use git_internal::internal::object::{ObjectTrait, types::ObjectType};

    use super::*;
    use crate::config::DEFAULT_MAX_PUSH_COMMITS;

    fn sig(ty: SignatureType, name: &str, email: &str, ts: usize) -> Signature {
        Signature {
            signature_type: ty,
            name: name.to_owned(),
            email: email.to_owned(),
            timestamp: ts,
            timezone: "+0800".to_owned(),
        }
    }

    fn commit(msg: &str, author: Signature, parent: Option<ObjectHash>) -> Commit {
        let tree = ObjectHash::from_str(&"1".repeat(40)).unwrap();
        let parents = parent.into_iter().collect();
        let committer = Signature {
            signature_type: SignatureType::Committer,
            name: author.name.clone(),
            email: author.email.clone(),
            timestamp: author.timestamp,
            timezone: author.timezone.clone(),
        };
        Commit::new(author, committer, tree, parents, msg)
    }

    fn alice(ts: usize) -> Signature {
        sig(SignatureType::Author, "Alice", "alice@example.com", ts)
    }

    fn bob(ts: usize) -> Signature {
        sig(SignatureType::Author, "Bob", "bob@example.com", ts)
    }

    #[test]
    fn squash_lists_all_n_in_topo_asc_without_truncation() {
        for n in [3usize, 50, DEFAULT_MAX_PUSH_COMMITS] {
            let mut chain = Vec::new();
            let mut parent = None;
            for i in 0..n {
                let c = commit(&format!("subject-{i}"), alice(1_700_000_000 + i), parent);
                parent = Some(c.id);
                chain.push(c);
            }
            let tip_first: Vec<_> = chain.iter().rev().cloned().collect();
            let topo = load_topo_asc(&tip_first).unwrap();
            assert_eq!(topo.len(), n);
            assert_eq!(topo.first().unwrap().id, chain[0].id);
            assert_eq!(topo.last().unwrap().id, chain[n - 1].id);
            let msg = squash_message(
                "/project/foo",
                &topo,
                Some((
                    "aa".repeat(20).as_str(),
                    topo.last().unwrap().id.to_string().as_str(),
                )),
                &alice(1),
            );
            assert!(
                msg.starts_with(&format!("Squash {n} commits at /project/foo\n")),
                "intro paragraph required"
            );
            assert!(msg.contains("This commit was created by mega2"));
            assert!(msg.contains(&format!("Mono-Squash-Count: {n}")));
            assert!(!msg.contains("Mono-Commits"));
            for c in &topo {
                assert!(msg.contains(&c.id.to_string()), "missing {} in N={n}", c.id);
                let subject = commit_subject(&c.message);
                assert!(msg.contains(&subject), "missing subject {subject}");
            }
            let listed = msg
                .lines()
                .filter(|l| l.starts_with("  ") && !l.starts_with("      "))
                .count();
            assert_eq!(listed, n);
        }
    }

    #[test]
    fn create_omits_range_and_compact_points_at_squash_id() {
        let a = commit("one", alice(10), None);
        let b = commit("two", bob(11), Some(a.id));
        let topo = vec![a.clone(), b.clone()];
        let squash = squash_message("/p", &topo, None, &b.author);
        assert!(
            !squash.contains("Mono-Squash-Range:"),
            "create must omit the Range trailer"
        );
        assert!(squash.contains("Mono-Squash-Count: 2"));
        let compact = compact_message("/p", "/", 2, &b.id.to_string(), None);
        assert!(compact.contains(&format!("Mono-Squash-Commit: {}", b.id)));
        assert!(!compact.contains("Mono-Squash-Range:"));
        assert!(compact.contains("Mono-Ref-Path: /"));
        assert!(compact.contains("Mono-Path: /p"));
        let desc = compact_message("/p", "/p/foo", 2, &b.id.to_string(), None);
        assert!(desc.contains("Mono-Ref-Path: /p/foo"));
        assert!(!desc.contains("Mono-Ref-Path: /p\n"));
    }

    #[test]
    fn co_authored_by_skips_tip_and_duplicates() {
        let a = commit("a", alice(1), None);
        let b = commit("b", bob(2), Some(a.id));
        let c = commit("c", bob(3), Some(b.id));
        let lines = co_authored_by_lines(&c.author, &[a, b, c.clone()]);
        assert_eq!(
            lines,
            vec!["Co-authored-by: Alice <alice@example.com>".to_string()]
        );
        assert!(
            !squash_message("/p", std::slice::from_ref(&c), None, &c.author)
                .contains("Mono-Commits")
        );
    }

    #[test]
    fn author_date_range_only_when_span_is_large() {
        let a = commit("a", alice(1_000), None);
        let b = commit(
            "b",
            alice(1_000 + AUTHOR_DATE_RANGE_THRESHOLD_SECS),
            Some(a.id),
        );
        let msg = squash_message("/p", &[a.clone(), b.clone()], None, &b.author);
        assert!(msg.contains("Mono-Author-Date-Range:"));
        let close = commit("b", alice(1_001), Some(a.id));
        let msg2 = squash_message("/p", &[a, close.clone()], None, &close.author);
        assert!(!msg2.contains("Mono-Author-Date-Range"));
    }

    #[test]
    fn committer_date_is_max_of_land_and_prev() {
        let prev = sig(SignatureType::Committer, "x", "x@e", 5_000);
        let later = committer_at_land("n", "e@e", Some(&prev), 100);
        assert_eq!(later.timestamp, 5_000);
        let earlier_prev = sig(SignatureType::Committer, "x", "x@e", 50);
        let land = committer_at_land("n", "e@e", Some(&earlier_prev), 200);
        assert_eq!(land.timestamp, 200);
        assert_eq!(land.timezone, "+0000");
    }

    const SIGNED_TIP_MESSAGE: &str = "gpgsig -----BEGIN PGP SIGNATURE-----\n \n wsBcBAABCAAQBQJ\n =abcd\n -----END PGP SIGNATURE-----\n\nfeat: signed subject\n\nbody\n";

    #[test]
    fn n1_layer_message_is_body() {
        // Unsigned tip as git-internal parses it: leading header/body newline.
        let tip = commit("\nfeat: keep me\n\nbody\n", alice(9), None);
        let plan = TrunkProvenance::from_tip(&tip, 1, "/p", tip.id.to_string(), None, 99);
        assert_eq!(plan.layer_message("/"), "feat: keep me\n\nbody\n");
        assert!(!plan.layer_message("/").contains("Mono-Squash"));

        // Signed tip: the client's signature header never reaches the layer body.
        let signed = commit(SIGNED_TIP_MESSAGE, alice(10), None);
        let plan = TrunkProvenance::from_tip(&signed, 1, "/p", signed.id.to_string(), None, 99);
        assert_eq!(plan.layer_message("/"), "feat: signed subject\n\nbody\n");
        assert!(!plan.layer_message("/").contains("BEGIN PGP SIGNATURE"));
    }

    #[test]
    fn squash_subject_skips_signature() {
        let a = commit(SIGNED_TIP_MESSAGE, alice(10), None);
        let b = commit("\nfix: second\n", bob(11), Some(a.id));
        let msg = squash_message("/p", &[a, b.clone()], None, &b.author);
        assert!(msg.contains("feat: signed subject"), "{msg}");
        assert!(msg.contains("fix: second"), "{msg}");
        assert!(!msg.contains("gpgsig"), "{msg}");
    }

    fn kind_tree(kind: HashKind) -> ObjectHash {
        ObjectHash::new_for_kind(kind, b"FIX16 tree")
    }

    fn kind_parent(kind: HashKind) -> ObjectHash {
        ObjectHash::new_for_kind(kind, b"FIX16 parent")
    }

    fn committer(ts: usize) -> Signature {
        sig(
            SignatureType::Committer,
            "trunk",
            "trunk@example.invalid",
            ts,
        )
    }

    #[test]
    fn native_push_hash_synthesize_explicit_kind_matches_recomputation() {
        for kind in [HashKind::Sha1, HashKind::Sha256, HashKind::Blake3] {
            let tree = kind_tree(kind);
            let parent = kind_parent(kind);
            let author = alice(1_700_000_000);
            let committer = committer(1_700_000_001);
            let message = "land /project at FIX16\n";

            let landed = synthesize_with_kind(
                kind,
                author.clone(),
                committer.clone(),
                tree,
                vec![parent],
                message,
            )
            .expect("explicit-kind synthesis must succeed for same-kind references");

            // AC-1: the OID is exactly the configured algorithm's digest of the
            // canonical commit bytes, never a thread-local fallback.
            let expected = Commit::new_with_kind(
                kind,
                author.clone(),
                committer.clone(),
                tree,
                vec![parent],
                message,
            )
            .expect("reference synthesis");
            assert_eq!(landed.id.kind(), kind);
            // Independent oracle: hash the canonical commit bytes directly
            // instead of delegating to the constructor under test.
            let canonical = landed.to_data().expect("canonical commit bytes");
            let independent =
                ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Commit, &canonical)
                    .expect("independent commit digest");
            assert_eq!(landed.id, independent, "FIX16_AC1_INDEPENDENT {kind:?}");
            assert_eq!(landed.id, expected.id, "FIX16_AC1 {kind:?}");
            assert_eq!(landed.id.to_string(), expected.id.to_string());
            let width = match kind {
                HashKind::Sha1 => 40,
                HashKind::Sha256 | HashKind::Blake3 => 64,
            };
            assert_eq!(
                landed.id.to_string().len(),
                width,
                "FIX16_OID_WIDTH {kind:?}"
            );

            // AC-3..AC-6: provenance fields pass through unchanged.
            assert_eq!(landed.author, author);
            assert_eq!(landed.committer, committer);
            assert_eq!(landed.parent_commit_ids, vec![parent]);
            assert_eq!(landed.message, message);
            assert_eq!(landed.tree_id, tree);
        }
    }

    #[test]
    fn native_push_hash_synthesize_rejects_cross_kind_tree_and_parent() {
        let author = alice(1_700_000_000);
        let committer = committer(1_700_000_001);

        for kind in [HashKind::Sha1, HashKind::Sha256, HashKind::Blake3] {
            // Exercise both a different-width pair (SHA-1 vs 32-byte) and the
            // same-width pair (SHA-256 vs BLAKE3), so rejection cannot rest on
            // length alone.
            for other in [HashKind::Sha256, HashKind::Blake3] {
                if other == kind || (kind == HashKind::Sha1 && other == HashKind::Blake3) {
                    continue;
                }

                // AC-2: a foreign-kind tree fails closed instead of minting an OID
                // under the wrong algorithm.
                let foreign_tree = kind_tree(other);
                assert!(
                    synthesize_with_kind(
                        kind,
                        author.clone(),
                        committer.clone(),
                        foreign_tree,
                        Vec::new(),
                        "FIX16 cross-kind tree\n",
                    )
                    .is_err(),
                    "FIX16_AC2_TREE {kind:?}"
                );

                let foreign_parent = kind_parent(other);
                assert!(
                    synthesize_with_kind(
                        kind,
                        author.clone(),
                        committer.clone(),
                        kind_tree(kind),
                        vec![foreign_parent],
                        "FIX16 cross-kind parent\n",
                    )
                    .is_err(),
                    "FIX16_AC2_PARENT {kind:?}"
                );
            }
        }
    }

    /// VER-5: the B3 trunk composition (provenance plan plus explicit-kind
    /// synthesis, as `execute_b3` performs it) must mint the configured
    /// algorithm's OID and never the thread-local one.
    #[test]
    fn native_push_hash_hash_red_trunk_composition_uses_configured_kind() {
        use git_internal::{
            hash::{get_hash_kind, set_hash_kind_for_test},
            internal::object::ObjectTrait,
        };

        let _tls = set_hash_kind_for_test(HashKind::Sha1);
        for kind in [HashKind::Sha256, HashKind::Blake3] {
            let tip = Commit::new_with_kind(
                kind,
                alice(1_700_000_000),
                committer(1_700_000_000),
                kind_tree(kind),
                vec![kind_parent(kind)],
                "feat: trunk tip\n",
            )
            .expect("typed tip");
            let plan = TrunkProvenance::from_tip(
                &tip,
                2,
                "/project/app",
                tip.id.to_string(),
                None,
                1_700_000_100,
            );

            let landed = synthesize_with_kind(
                kind,
                plan.author.clone(),
                plan.committer(None),
                kind_tree(kind),
                vec![kind_parent(kind)],
                &plan.layer_message("/project/app"),
            )
            .expect("trunk composition");

            assert_eq!(landed.id.kind(), kind, "FIX16_VER5_KIND {kind:?}");
            let canonical = landed.to_data().expect("canonical bytes");
            assert_eq!(
                landed.id,
                ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Commit, &canonical)
                    .expect("independent digest"),
                "FIX16_VER5_RECOMPUTE {kind:?}"
            );
            if let Ok(tls_id) = ObjectHash::from_type_and_data_for_kind(
                get_hash_kind(),
                ObjectType::Commit,
                &canonical,
            ) {
                assert_ne!(landed.id, tls_id, "FIX16_VER5_TLS {kind:?}");
            }
            assert_eq!(get_hash_kind(), HashKind::Sha1);
        }
    }

    #[test]
    fn native_push_hash_synthesize_ignores_thread_local_kind() {
        use git_internal::hash::{get_hash_kind, set_hash_kind_for_test};

        let _tls = set_hash_kind_for_test(HashKind::Sha1);
        assert_eq!(get_hash_kind(), HashKind::Sha1);

        for kind in [HashKind::Sha256, HashKind::Blake3] {
            let landed = synthesize_with_kind(
                kind,
                alice(1_700_000_000),
                committer(1_700_000_001),
                kind_tree(kind),
                vec![kind_parent(kind)],
                "FIX16 TLS independence\n",
            )
            .expect("explicit kind synthesis");
            assert_eq!(landed.id.kind(), kind, "FIX16_TLS {kind:?}");
            assert_eq!(
                get_hash_kind(),
                HashKind::Sha1,
                "FIX16 synthesis must not mutate the thread-local kind"
            );
        }
    }
}
