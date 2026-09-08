//! Trunk synthetic-commit provenance (plan TP-16 / trunk-push.md 3.1–3.2).
//!
//! `Commit::from_tree_id` is the review-morph CL merge constructor and must
//! not be used for trunk squash / ancestor roll-up / descendant continuation.

use chrono::{FixedOffset, TimeZone};
use git_internal::{
    hash::ObjectHash,
    internal::object::{
        commit::Commit,
        signature::{Signature, SignatureType},
    },
};

use crate::common::errors::MegaError;

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
            n1_message: tip.message.clone(),
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

pub fn synthesize(
    author: Signature,
    committer: Signature,
    tree: ObjectHash,
    parents: Vec<ObjectHash>,
    message: &str,
) -> Commit {
    Commit::new(author, committer, tree, parents, message)
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
        "This commit was created by monoengine. The push carried {n} commits, which\n\
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
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned()
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
            assert!(msg.contains("This commit was created by monoengine"));
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

    #[test]
    fn n1_layer_message_is_verbatim() {
        let tip = commit("feat: keep me\n\nbody\n", alice(9), None);
        let plan = TrunkProvenance::from_tip(&tip, 1, "/p", tip.id.to_string(), None, 99);
        assert_eq!(plan.layer_message("/"), tip.message);
        assert!(!plan.layer_message("/").contains("Mono-Squash"));
    }
}
