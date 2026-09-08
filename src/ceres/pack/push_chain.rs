//! Push-chain state model for the Monorepo receive-pack path.
//!
//! Push semantics (base/tip/attribution) derive solely from
//! `RefCommand.old_id/new_id` plus the unpacked tip commit's parent chain —
//! never from pack arrival order (GC-MC-13, plan-20260827).
//!
//! [`PushChain::validate`] is the chain validator (plan-20260827 MC-03): the
//! rejection surface for merge commits, broken topology, cycles, tip
//! mismatch, and the increment/cumulative length bounds. MC-06 wires it into
//! the receive-pack path: `Monorepo::finalize_receive_pack` validates the
//! primary branch command's chain before any ref/CL mutation, alongside the
//! ADR-MC-04 multi-branch rejection.

use std::collections::HashSet;

use git_internal::internal::object::commit::Commit;

use crate::{
    callisto::{mega_cl, sea_orm_active_enums::RefTypeEnum},
    ceres::protocol::import_refs::{CommandType, RefCommand},
    common::{errors::MegaError, utils::ZERO_ID},
    jupiter::{storage::mono_storage::MonoStorage, utils::converter::FromMegaModel},
};

/// Push semantics state for one receive-pack.
///
/// Built after unpack from the branch `RefCommand` and the stored tip commit;
/// replaces the former `current_commit` slot that captured the first commit in
/// pack arrival order.
#[derive(Debug, Clone)]
pub struct PushChain {
    /// Chain base: `RefCommand.old_id`, or — for a new-branch push
    /// (`old_id == ZERO_ID`) — the fork point: the first commit on the tip's
    /// first-parent chain that this push did not newly introduce (Codex R1
    /// P1-1; a pack may redundantly carry server-known ancestors, they do not
    /// extend the chain). Feeds the CL `from_hash`.
    pub base: String,
    /// Chain tip: the commit `RefCommand.new_id` points to. Feeds the CL
    /// `to_hash` and is the single source of CL ref `ref_commit_hash` /
    /// `ref_tree_hash`, file-path indexing, and object `commit_id`
    /// attribution (ADR-MC-06).
    pub tip: Commit,
    /// The chain surface the validator checks, tip first. For a push that
    /// introduces the tip this is the newly introduced increment `(fork, tip]`
    /// (redundantly carried known ancestors are excluded, Codex R1 P1-1); for
    /// a no-new-content push (a verbatim retry or a hand-crafted pack of known
    /// commits) it is the full content chain from the pack — re-validating it
    /// is what keeps rejections sticky across retries (Codex R2 P1-1/P1-2).
    /// `PushChain::validate` checks this segment in memory (Codex R1 P1-3: no
    /// duplicate DB reads of the pack segment); the CL commit listing (MC-04)
    /// still rebuilds the full `(from_hash, to_hash]` range from storage, and
    /// post-finalize commit binding (Codex R1 P1-2) covers only the newly
    /// introduced members.
    pub ordered_commits: Vec<Commit>,
}

/// Result of resolving a branch command against post-unpack storage.
#[derive(Debug)]
pub enum PushChainResolution {
    /// The pack introduced a new commit; the chain carries push semantics.
    /// Boxed to keep the no-op variant cheap (clippy::large_enum_variant).
    Chain(Box<PushChain>),
    /// ADR-MC-05: the pack contained no new commit and `new_id` is already
    /// known server-side — an idempotent no-op. CL ref and CL stay untouched;
    /// `notice` is the `remote:` hint text for report-status.
    Noop { notice: String },
}

impl PushChain {
    /// Resolve a non-delete branch command into a push chain (or an ADR-MC-05
    /// no-op).
    ///
    /// * `pack_commit_ids` — object ids of every commit the unpack delivered
    ///   (presence). All rejection rules are derived from this *content* set,
    ///   never from transient newness, so a rejected pack that is retried
    ///   verbatim is rejected identically (Codex R2 P1-1, sticky rejections).
    ///   Presence also drives the ADR-MC-05 no-op split and the "pack carries
    ///   the ref target" fail-closed check.
    /// * `new_commit_ids` — the subset of the pack's commits that were absent
    ///   from storage at unpack time (Codex R1 P1-1). Selects only *which*
    ///   prefix of the chain is this push's increment (for `ordered_commits`,
    ///   the fork point, and post-finalize binding) — never whether to reject.
    /// * `tip_commit` — post-unpack storage lookup of `cmd.new_id`.
    ///
    /// Error paths are explicit (GC-MC-14, no silent returns): an empty pack
    /// pointing at an unknown `new_id`, a pack whose commits do not include
    /// the ref update target, pack commits outside the tip's first-parent
    /// chain (junk side content), or a pack commit whose `new_id` never landed
    /// in storage, all fail the push with an actionable message.
    pub async fn resolve(
        cmd: &RefCommand,
        pack_commit_ids: &HashSet<String>,
        new_commit_ids: &HashSet<String>,
        tip_commit: Option<Commit>,
        storage: &MonoStorage,
        max_commits: usize,
    ) -> Result<PushChainResolution, MegaError> {
        debug_assert!(
            cmd.command_type != CommandType::Delete && cmd.new_id != ZERO_ID,
            "push chain is only resolved for non-delete branch commands"
        );
        match (!pack_commit_ids.is_empty(), tip_commit) {
            (false, Some(_)) => Ok(PushChainResolution::Noop {
                notice: format!(
                    "push contained no new commit objects and {} is already known; \
                     no change list was created or updated",
                    cmd.new_id
                ),
            }),
            (false, None) => Err(MegaError::Other(format!(
                "receive-pack got an empty pack for `{}` but new_id {} is not a known commit; \
                 push a pack that contains the commit",
                cmd.ref_name, cmd.new_id
            ))),
            (true, None) => Err(MegaError::Other(format!(
                "push commit {} for `{}` was not found in storage after unpack; \
                 the pack's commit must match the ref update",
                cmd.new_id, cmd.ref_name
            ))),
            (true, Some(tip)) => {
                // Fail closed (the MC01-R1 P2-1 invariant, generalized to
                // multi-commit packs): the pack must carry the ref update
                // target itself, not just any commit.
                let tip_id = tip.id.to_string();
                if !pack_commit_ids.contains(&tip_id) {
                    return Err(MegaError::Other(format!(
                        "the pack does not contain the ref update target {tip_id} for `{}`; \
                         push a pack whose commits include the ref update's new_id",
                        cmd.ref_name
                    )));
                }
                let tip_is_new = new_commit_ids.contains(&tip_id);

                // The merge-commit rule fires here, before the junk
                // classification (and again in the validator's in-memory
                // increment check): a merge's second-parent ancestry is
                // carried in the pack but lies off the first-parent path, so
                // without this priority the junk check would misidentify the
                // problem. A no-new-content push claims its whole content
                // chain; otherwise only newly introduced members are claimed
                // (known carried ancestors are ADR-MC-07-tolerated history).
                if tip.parent_commit_ids.len() > 1 {
                    return Err(MegaError::Other(format!(
                        "push chain contains merge commit {tip_id} ({} parents); \
                         monorepo CLs are linear — rebase the branch and push again",
                        tip.parent_commit_ids.len()
                    )));
                }

                // First-parent path walk over the pack's *content*: from the
                // tip downwards, while the next commit is carried by the pack.
                // Two budgets (MC-03's ≤250 PK reads for any real push): newly
                // introduced members count against the semantic chain limit;
                // when the push introduces the tip, already-known carried
                // ancestors get a separate hygiene budget (they are tolerated
                // — non-thin packs legitimately carry a few boundary objects —
                // but a pathological pack cannot make the walk unbounded).
                // When the tip is already known (a verbatim retry of a
                // rejected push re-carries the same, now-known commits), the
                // whole content chain is walked under the semantic budget so
                // the retry re-fails identically.
                let mut path: Vec<Commit> = vec![tip.clone()];
                let mut new_len = 1usize;
                let mut known_len = 0usize;
                let mut current = tip.clone();
                // `walk_stop` = the first first-parent not carried by the pack;
                // `None` if the walk reached a parentless (root) commit.
                let walk_stop: Option<String> = loop {
                    let Some(parent_id) =
                        current.parent_commit_ids.first().map(ToString::to_string)
                    else {
                        break None;
                    };
                    if !pack_commit_ids.contains(&parent_id) {
                        break Some(parent_id);
                    }
                    let parent_is_new = new_commit_ids.contains(&parent_id);
                    if tip_is_new {
                        if parent_is_new {
                            if new_len >= max_commits {
                                return Err(MegaError::Other(format!(
                                    "push introduces more than {max_commits} commits in \
                                     one chain; split the changes into smaller pushes or squash \
                                     and re-push"
                                )));
                            }
                            new_len += 1;
                        } else {
                            if known_len >= max_commits {
                                return Err(MegaError::Other(format!(
                                    "pack carries more than {max_commits} already-known \
                                     commits along the chain for `{}`; re-push with a thinner \
                                     pack (the redundant ancestors are not needed)",
                                    cmd.ref_name
                                )));
                            }
                            known_len += 1;
                        }
                    } else if path.len() >= max_commits {
                        return Err(MegaError::Other(format!(
                            "push introduces more than {max_commits} commits in one \
                             chain; split the changes into smaller pushes or squash and re-push"
                        )));
                    }
                    if path.len() >= pack_commit_ids.len() {
                        // Defensive bound: every walk step consumes one pack
                        // member; exceeding the set size means a first-parent
                        // cycle inside the pack.
                        return Err(MegaError::Other(format!(
                            "push chain contains a cycle below commit {tip_id}; \
                             re-create the commits with a linear history and re-push"
                        )));
                    }
                    let parent =
                        storage
                            .get_commit_by_hash(&parent_id)
                            .await?
                            .ok_or_else(|| {
                                MegaError::Other(format!(
                                    "push chain is broken: commit {parent_id} (parent of {}) \
                             was carried by the pack but is missing from storage; \
                             re-push including the full commit history",
                                    current.id
                                ))
                            })?;
                    let parent_commit = Commit::from_mega_model(parent);
                    // Claimed-segment merge rule (see the tip check above for
                    // why merge detection must precede junk classification).
                    let claimed = if tip_is_new { parent_is_new } else { true };
                    if claimed && parent_commit.parent_commit_ids.len() > 1 {
                        return Err(MegaError::Other(format!(
                            "push chain contains merge commit {parent_id} ({} parents); \
                             monorepo CLs are linear — rebase the branch and push again",
                            parent_commit.parent_commit_ids.len()
                        )));
                    }
                    current = parent_commit;
                    path.push(current.clone());
                };

                // Junk check (Codex R2 P1-1): every commit the pack carries
                // must lie on the tip's first-parent path. Off-chain commits —
                // whether newly introduced or already known — make the pack
                // inconsistent with the ref update and are rejected on every
                // attempt, retry included.
                let path_ids: HashSet<String> = path.iter().map(|c| c.id.to_string()).collect();
                let mut junk = pack_commit_ids
                    .iter()
                    .filter(|id| !path_ids.contains(id.as_str()));
                if let Some(first) = junk.next() {
                    let count = 1 + junk.count();
                    return Err(MegaError::Other(format!(
                        "the pack contains {count} commit(s) outside the ref update's chain \
                         for `{}` (e.g. {first}); push only the commits that lead to the ref \
                         update target",
                        cmd.ref_name
                    )));
                }

                let (ordered_commits, fork) = if tip_is_new {
                    // Normal push: the increment is the leading newly
                    // introduced prefix of the path; the fork point is the
                    // first already-known path member (a redundantly carried
                    // ancestor) or, when the whole path is new, the parent
                    // just past the path end.
                    let split = path
                        .iter()
                        .position(|c| !new_commit_ids.contains(&c.id.to_string()));
                    match split {
                        Some(i) => (path[..i].to_vec(), Some(path[i].id.to_string())),
                        None => (path.clone(), walk_stop),
                    }
                } else {
                    // No-new content push (a verbatim retry, or a hand-crafted
                    // pack of known commits): the full content chain is the
                    // surface the validator must re-check, so rejections stay
                    // sticky (Codex R2 P1-1). It is deliberately *not*
                    // collapsed to `[tip]`: a collapsed increment would let a
                    // retried chain with a mid-chain violation pass. When the
                    // walk ends at the parentless root, the root itself is the
                    // fork baseline.
                    let fork = match walk_stop {
                        Some(parent) => Some(parent),
                        None => path.last().map(|c| c.id.to_string()),
                    };
                    (path.clone(), fork)
                };
                let base = if cmd.old_id != ZERO_ID {
                    // The ref contract pins the base; the validator's
                    // termination check requires the walk to have reached it.
                    cmd.old_id.clone()
                } else {
                    // New-branch push: `base` is the fork point. A missing stop
                    // means a brand-new parentless chain — the historical
                    // orphan rejection.
                    let fork = fork.ok_or_else(|| {
                        MegaError::Other(
                            "Can not init directory under monorepo directory!".to_string(),
                        )
                    })?;
                    // The fork point anchors the CL `from_hash` and its tree
                    // drives the aggregate diff — it must be server-known.
                    // (`old_id`-bearing updates get the same guarantee from
                    // the validator: the base must terminate the increment
                    // walk, and the cumulative history walk reads it.)
                    if storage.get_commit_by_hash(&fork).await?.is_none() {
                        return Err(MegaError::Other(format!(
                            "push chain base {fork} for `{}` is not a known commit; \
                             fetch the repository and rebase onto a known head before pushing",
                            cmd.ref_name
                        )));
                    }
                    fork
                };
                Ok(PushChainResolution::Chain(Box::new(PushChain {
                    base,
                    tip,
                    ordered_commits,
                })))
            }
        }
    }

    /// Walk first-parent from a server-known tip to `cmd.old_id` (GAP-14).
    ///
    /// Used when the pack is empty or carries only already-known objects so
    /// [`Self::resolve`] would return [`PushChainResolution::Noop`]. `n` is
    /// the resulting increment length (`ordered_commits.len()`), or `0` when
    /// `old_id == new_id`. Server-knownness does not change `n`.
    pub async fn from_known_tip(
        cmd: &RefCommand,
        tip: Commit,
        storage: &MonoStorage,
        max_commits: usize,
    ) -> Result<Self, MegaError> {
        if cmd.new_id != tip.id.to_string() {
            return Err(MegaError::Other(format!(
                "known-tip chain expected new_id {} but tip is {}",
                cmd.new_id, tip.id
            )));
        }
        if cmd.old_id == cmd.new_id {
            return Ok(PushChain {
                base: cmd.old_id.clone(),
                tip: tip.clone(),
                ordered_commits: vec![tip],
            });
        }

        let mut path: Vec<Commit> = vec![tip.clone()];
        let mut current = tip.clone();
        loop {
            if path.len() > max_commits {
                return Err(MegaError::Other(format!(
                    "push introduces more than {max_commits} commits in one chain; \
                     split the changes into smaller pushes or squash and re-push"
                )));
            }
            let Some(parent_id) = current.parent_commit_ids.first().map(ToString::to_string) else {
                if cmd.old_id == ZERO_ID {
                    break;
                }
                return Err(MegaError::Other(format!(
                    "push chain from {} never reached old_id {}; fetch and rebase onto the advertised tip",
                    cmd.new_id, cmd.old_id
                )));
            };
            if parent_id == cmd.old_id {
                break;
            }
            let parent = storage
                .get_commit_by_hash(&parent_id)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(format!(
                        "push chain is broken: commit {parent_id} (parent of {}) is missing from storage",
                        current.id
                    ))
                })?;
            current = Commit::from_mega_model(parent);
            path.push(current.clone());
        }

        let base = if cmd.old_id != ZERO_ID {
            cmd.old_id.clone()
        } else {
            path.last()
                .and_then(|c| c.parent_commit_ids.first().map(ToString::to_string))
                .unwrap_or_else(|| ZERO_ID.to_string())
        };
        Ok(PushChain {
            base,
            tip,
            ordered_commits: path,
        })
    }

    /// Validate the resolved chain against commit storage (plan-20260827
    /// MC-03). Every rejection path is fail-closed with an actionable
    /// message. Wired into the receive-pack path by MC-06:
    /// `Monorepo::finalize_receive_pack` runs it on the primary branch
    /// command's chain before any ref/CL mutation.
    ///
    /// Two segments (ADR-MC-07), each checked exactly once (Codex R1 P1-3):
    /// the push increment `(base, tip]` is already in memory from resolve's
    /// bounded walk — merge commits, cycles, topology continuity and the
    /// length bound are checked on `ordered_commits` without re-reading the
    /// DB; the pre-existing CL history `(from_hash, base]` — present only when
    /// the push updates an open CL — is walked from `base` in the DB,
    /// count-only (it was validated when it was pushed, so a legacy anomaly
    /// there must not reject a legal increment). History still fails closed
    /// when counting is impossible: a missing object, unparseable
    /// `parents_id`, a parent cycle (would otherwise loop forever), or a chain
    /// that never reaches `from_hash`. The CL's cumulative range
    /// `(from_hash → tip]` stays bounded by `max_commits` (review passes
    /// [`crate::ceres::merge_checker::MAX_CL_CHAIN_COMMITS`]; trunk passes
    /// `[monorepo].max_push_commits`): exactly that many commits pass, one more
    /// is rejected.
    pub async fn validate(
        &self,
        cmd: &RefCommand,
        storage: &MonoStorage,
        open_cl: Option<&mega_cl::Model>,
        max_commits: usize,
    ) -> Result<(), MegaError> {
        let tip_id = self.tip.id.to_string();
        if tip_id != cmd.new_id {
            return Err(MegaError::Other(format!(
                "push chain tip {tip_id} does not match the ref update new_id {} for `{}`; \
                 the pack's tip commit must be the ref update target",
                cmd.new_id, cmd.ref_name
            )));
        }

        // Increment segment `(base, tip]`, in memory (Codex R1 P1-3): resolve
        // already read every one of these commits from storage during its
        // bounded walk, so only the checks live here.
        let increment_len = self.ordered_commits.len();
        let mut visited = HashSet::new();
        for (idx, commit) in self.ordered_commits.iter().enumerate() {
            let commit_id = commit.id.to_string();
            if idx == 0 && commit_id != tip_id {
                return Err(MegaError::Other(format!(
                    "push chain is broken: the increment starts at {commit_id}, not the tip \
                     {tip_id}; re-push including the full commit history"
                )));
            }
            if !visited.insert(commit_id.clone()) {
                return Err(MegaError::Other(format!(
                    "push chain contains a cycle at commit {commit_id}; \
                     re-create the commits with a linear history and re-push"
                )));
            }
            if commit.parent_commit_ids.len() > 1 {
                return Err(MegaError::Other(format!(
                    "push chain contains merge commit {commit_id} ({} parents); \
                     monorepo CLs are linear — rebase the branch and push again",
                    commit.parent_commit_ids.len()
                )));
            }
            let first_parent = commit.parent_commit_ids.first().map(ToString::to_string);
            match self.ordered_commits.get(idx + 1) {
                // Adjacent commits must link by first parent.
                Some(next) if first_parent != Some(next.id.to_string()) => {
                    return Err(MegaError::Other(format!(
                        "push chain is broken: commit {commit_id} is not linked to the next \
                         chain commit by its first parent; re-push including the full commit \
                         history"
                    )));
                }
                // The oldest increment commit must be parented on the base —
                // otherwise the claimed base is not on the tip's chain.
                None if first_parent.as_deref() != Some(self.base.as_str()) => {
                    return Err(MegaError::Other(format!(
                        "push chain is broken: base {} is not on the first-parent chain of \
                         tip {tip_id}; rebase onto the current base and re-push",
                        self.base
                    )));
                }
                _ => {}
            }
        }
        if increment_len > max_commits {
            return Err(MegaError::Other(format!(
                "push introduces more than {max_commits} commits in one \
                 chain; split the changes into smaller pushes or squash and re-push"
            )));
        }

        // History segment `(from_hash, base]` (ADR-MC-07): only when the push
        // updates an open CL. Count-only DB walk from `base`; the cumulative
        // total is increment + history.
        if let Some(cl) = open_cl {
            let mut history_len = 0usize;
            let mut visited = HashSet::new();
            let mut current = self.base.clone();
            while current != cl.from_hash {
                if !visited.insert(current.clone()) {
                    return Err(MegaError::Other(format!(
                        "push chain contains a cycle at commit {current}; \
                         re-create the commits with a linear history and re-push"
                    )));
                }
                let model = storage.get_commit_by_hash(&current).await?.ok_or_else(|| {
                    MegaError::Other(format!(
                        "push chain is broken: commit {current} is missing from storage; \
                         re-push including the full commit history"
                    ))
                })?;
                let parents: Vec<String> = serde_json::from_value(model.parents_id.clone())
                    .map_err(|e| {
                        MegaError::Other(format!(
                            "push chain is broken: corrupt parents_id for commit {current}: {e}"
                        ))
                    })?;
                history_len += 1;
                if increment_len + history_len > max_commits {
                    // The increment itself fit; the overflow comes from the
                    // pre-existing CL history — a cumulative violation.
                    return Err(MegaError::Other(format!(
                        "updating CL {} would exceed the {max_commits}-commit \
                         cumulative limit ({} → {tip_id}); \
                         merge the current CL first or open a new CL",
                        cl.link, cl.from_hash
                    )));
                }
                current = match parents.first() {
                    Some(parent) => parent.clone(),
                    None => {
                        return Err(MegaError::Other(format!(
                            "push chain is broken: commit {current} has no parent, but the chain \
                             never reached base {}; rebase the pushed commits onto the \
                             current base and re-push",
                            cl.from_hash
                        )));
                    }
                };
            }
        }
        Ok(())
    }
}

/// The semantics-defining command of a push: the first non-delete branch
/// command. Delete commands never build a chain; a receive-pack carrying more
/// than one non-delete branch command is rejected outright at finalize
/// (ADR-MC-04, MC-06), so at most one chain is ever built per push.
pub fn primary_branch_command(commands: &[RefCommand]) -> Option<RefCommand> {
    commands
        .iter()
        .find(|c| {
            c.ref_type == RefTypeEnum::Branch
                && c.command_type != CommandType::Delete
                && c.new_id != ZERO_ID
        })
        .cloned()
}

/// ADR-MC-06 object `commit_id` attribution for one receive-pack: the chain
/// tip (`new_id` of the primary branch command), derived from the ref commands
/// alone — independent of pack arrival order (MC01-R2 P1-2). Empty string when
/// the push carries no branch command (tag-only) — the historical no-commit
/// value.
///
/// Safety: a Monorepo branch push with objects always carries its commit in
/// the pack, and `PushChain::resolve` fail-closes when the ref update target
/// is not among the pack's commits; delete-only pushes skip unpack entirely
/// and empty packs have no entries, so neither reaches `save_entry`. A
/// pathological "trees but no commit" pack is attributed to `new_id`, but
/// `batch_save_model` is insert-only (on-conflict do-nothing), so
/// pre-existing rows keep their attribution.
pub fn attribution_commit_id(commands: &[RefCommand]) -> String {
    primary_branch_command(commands)
        .map(|cmd| cmd.new_id)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use git_internal::{
        hash::ObjectHash,
        internal::object::{
            commit::Commit,
            signature::{Signature, SignatureType},
        },
    };
    use sea_orm::{EntityTrait, Set};
    use tempfile::TempDir;

    use super::{PushChain, PushChainResolution, attribution_commit_id, primary_branch_command};
    use crate::{
        callisto::{mega_cl, mega_commit, sea_orm_active_enums::MergeStatusEnum},
        ceres::{merge_checker::MAX_CL_CHAIN_COMMITS, protocol::import_refs::RefCommand},
        common::utils::ZERO_ID,
        jupiter::{
            storage::{Storage, base_storage::StorageConnector},
            tests::test_storage,
            utils::converter::FromMegaModel,
        },
    };

    fn test_signature(signature_type: SignatureType) -> Signature {
        Signature::new(
            signature_type,
            "Monoengine Test".to_string(),
            "monoengine-test@example.invalid".to_string(),
        )
    }

    fn test_commit(parent_commit_ids: Vec<ObjectHash>) -> Commit {
        let tree_id = ObjectHash::from_str("27dd8d4cf39f3868c6eee38b601bc9e9939304f5").unwrap();
        Commit::new(
            test_signature(SignatureType::Author),
            test_signature(SignatureType::Committer),
            tree_id,
            parent_commit_ids,
            "test commit",
        )
    }

    fn branch_command(old_id: String, new_id: String) -> RefCommand {
        RefCommand::new(old_id, new_id, "refs/heads/main".to_string())
    }

    fn id_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn id_range(start: u64, len: u64) -> HashSet<String> {
        (start + 1..=start + len).map(sha).collect()
    }

    #[tokio::test]
    async fn resolve_builds_single_commit_chain_for_existing_ref() {
        let (_temp, storage) = setup_storage().await;
        let parent = ObjectHash::from_str("119bc457cb05b52dfb0d6b14f66d9a8a52d09e25").unwrap();
        let tip = test_commit(vec![parent]);
        let old_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let cmd = branch_command(old_id.clone(), tip.id.to_string());
        let pack = id_set(&[&tip.id.to_string()]);

        let resolution = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(tip.clone()),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap();

        let PushChainResolution::Chain(chain) = resolution else {
            panic!("expected a push chain");
        };
        assert_eq!(chain.base, old_id);
        assert_eq!(chain.tip.id, tip.id);
        assert_eq!(chain.ordered_commits.len(), 1);
        assert_eq!(chain.ordered_commits[0].id, tip.id);
    }

    #[tokio::test]
    async fn resolve_uses_tip_first_parent_as_base_for_new_branch_push() {
        let (_temp, storage) = setup_storage().await;
        let parent = "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25";
        // The fork point must be a known commit (it anchors the CL from_hash).
        insert_commits(&storage, vec![commit_row(1, parent, &[])]).await;
        let tip = test_commit(vec![ObjectHash::from_str(parent).unwrap()]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.id.to_string());
        let pack = id_set(&[&tip.id.to_string()]);

        let resolution = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(tip.clone()),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap();

        let PushChainResolution::Chain(chain) = resolution else {
            panic!("expected a push chain");
        };
        assert_eq!(chain.base, parent);
        assert_eq!(chain.tip.id, tip.id);
    }

    // MC-06: a multi-commit new-branch push resolves `base` to the fork point
    // (the first tip first-parent the push did not newly introduce), not the
    // tip's own parent, and `ordered_commits` carries the full increment, tip
    // first.
    #[tokio::test]
    async fn resolve_new_branch_multicommit_walks_to_fork_point() {
        let (_temp, storage) = setup_storage().await;
        let fork = sha(100);
        insert_commits(&storage, vec![commit_row(100, &fork, &[])]).await;
        let tip = insert_linear_chain(&storage, 100, 3).await;
        let pack = id_set(&[&sha(101), &sha(102), &sha(103)]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());

        let resolution = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap();

        let PushChainResolution::Chain(chain) = resolution else {
            panic!("expected a push chain");
        };
        assert_eq!(chain.base, fork);
        let ordered: Vec<String> = chain
            .ordered_commits
            .iter()
            .map(|c| c.id.to_string())
            .collect();
        assert_eq!(ordered, vec![sha(103), sha(102), sha(101)]);
    }

    // Codex R1 P1-1: packs may redundantly carry server-known ancestors. The
    // fork-point walk follows only *newly introduced* commits, so the carried
    // fork sha(5000) stops the walk — base must be sha(5000), never its own
    // parent sha(4999).
    #[tokio::test]
    async fn resolve_new_branch_walk_stops_at_known_ancestor_carried_in_pack() {
        let (_temp, storage) = setup_storage().await;
        insert_commits(
            &storage,
            vec![
                commit_row(4999, &sha(4999), &[]),
                commit_row(5000, &sha(5000), &[sha(4999)]),
            ],
        )
        .await;
        let tip = insert_linear_chain(&storage, 5000, 2).await;
        // The pack redundantly carries the known fork sha(5000); only the two
        // chain commits are newly introduced.
        let pack = id_set(&[&sha(5000), &sha(5001), &sha(5002)]);
        let new_ids = id_set(&[&sha(5001), &sha(5002)]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());

        let resolution = PushChain::resolve(
            &cmd,
            &pack,
            &new_ids,
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap();

        let PushChainResolution::Chain(chain) = resolution else {
            panic!("expected a push chain");
        };
        assert_eq!(chain.base, sha(5000));
        let ordered: Vec<String> = chain
            .ordered_commits
            .iter()
            .map(|c| c.id.to_string())
            .collect();
        assert_eq!(ordered, vec![sha(5002), sha(5001)]);
    }

    // Codex R1 P1-1 (edge) / R2 P1-2: a push whose tip is already known (a
    // verbatim retry, or a branch cut from a commit known via another branch)
    // re-walks the full content chain so the validator re-checks it and
    // rejections stay sticky — `ordered_commits` is the whole walked path,
    // base = the parent just past the path end. (Nothing is newly introduced,
    // so post-finalize binding covers none of these commits.)
    #[tokio::test]
    async fn resolve_known_tip_with_no_new_commits_walks_presence_set() {
        let (_temp, storage) = setup_storage().await;
        insert_commits(&storage, vec![commit_row(6000, &sha(6000), &[])]).await;
        let tip = insert_linear_chain(&storage, 6000, 2).await;
        // The pack re-carries the (already known) chain; nothing is new.
        let pack = id_set(&[&sha(6001), &sha(6002)]);
        let new_ids = id_set(&[]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());

        let resolution = PushChain::resolve(
            &cmd,
            &pack,
            &new_ids,
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap();

        let PushChainResolution::Chain(chain) = resolution else {
            panic!("expected a push chain");
        };
        assert_eq!(chain.base, sha(6000));
        assert_eq!(chain.ordered_commits.len(), 2);
    }

    // Codex R2 P1-1 (sticky junk rejection): the junk rule is content-based —
    // derived from the pack's presence set against the tip's first-parent path,
    // never from transient newness. A rejected pack retried verbatim is
    // rejected identically: attempt 1 carries the junk commit as new, attempt 2
    // (the retry) finds it already stored — both must fail the same way.
    #[tokio::test]
    async fn resolve_junk_rejection_is_sticky_across_retries() {
        let (_temp, storage) = setup_storage().await;
        insert_commits(&storage, vec![commit_row(7000, &sha(7000), &[])]).await;
        let tip = insert_linear_chain(&storage, 7000, 1).await;
        let junk = commit_row(7999, &sha(7999), &[]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());
        let pack = id_set(&[&tip.commit_id, &junk.commit_id]);

        // Attempt 1: the junk commit is newly introduced (not yet stored).
        let err1 = PushChain::resolve(
            &cmd,
            &pack,
            &id_set(&[&junk.commit_id]),
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();
        let msg1 = err1.to_string();
        assert!(msg1.contains("outside the ref update's chain"), "{msg1}");
        assert!(msg1.contains(&junk.commit_id), "{msg1}");

        // The junk object landed in storage during attempt 1's unpack.
        insert_commits(&storage, vec![junk.clone()]).await;

        // Attempt 2: a verbatim retry — nothing is newly introduced any more.
        let err2 = PushChain::resolve(
            &cmd,
            &pack,
            &id_set(&[]),
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err1.to_string(),
            err2.to_string(),
            "a verbatim retry of a rejected pack must be rejected identically"
        );
    }

    // Codex R2 P1-1 (budget hygiene): already-known ancestors carried by the
    // pack are tolerated on the chain path, but their walk is separately
    // bounded — a pathological pack cannot turn redundant carries into
    // unbounded DB reads.
    #[tokio::test]
    async fn resolve_rejects_excess_redundant_known_ancestors() {
        let (_temp, storage) = setup_storage().await;
        // sha(8001..=8251): the known ancestor segment (251 rows).
        insert_linear_chain(&storage, 8000, MAX_CL_CHAIN_COMMITS as u64 + 1).await;
        // sha(8252..=8254): the genuinely new increment on top of it.
        let tip = insert_linear_chain(&storage, 8000 + MAX_CL_CHAIN_COMMITS as u64 + 1, 3).await;
        let pack = id_range(8000, MAX_CL_CHAIN_COMMITS as u64 + 1 + 3);
        let new_ids = id_range(8000 + MAX_CL_CHAIN_COMMITS as u64 + 1, 3);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());

        let err = PushChain::resolve(
            &cmd,
            &pack,
            &new_ids,
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("already-known commits"), "{err}");
    }

    // The merge-commit rejection must win over junk classification (a merge's
    // second-parent ancestry is carried in the pack but off the first-parent
    // path), and it must be sticky across a verbatim retry (the retried pack
    // is then all-known — Codex R2 P1-1).
    #[tokio::test]
    async fn resolve_merge_commit_message_wins_over_junk_and_is_sticky() {
        let (_temp, storage) = setup_storage().await;
        insert_commits(&storage, vec![commit_row(9000, &sha(9000), &[])]).await;
        // tip = merge of the mainline and side commits; the side commit rides
        // along in the pack but is off the tip's first-parent path.
        let mainline = commit_row(9001, &sha(9001), &[sha(9000)]);
        let side = commit_row(9099, &sha(9099), &[sha(9000)]);
        let merge = commit_row(9002, &sha(9002), &[sha(9001), sha(9099)]);
        let cmd = branch_command(ZERO_ID.to_string(), merge.commit_id.clone());
        let pack = id_set(&[&sha(9002), &sha(9001), &sha(9099)]);

        // Attempt 1: all three commits are new (nothing inserted yet).
        let err1 = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(Commit::from_mega_model(merge.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();
        let msg1 = err1.to_string();
        assert!(msg1.contains("merge commit"), "{msg1}");
        assert!(msg1.contains(&sha(9002)), "{msg1}");
        assert!(
            !msg1.contains("outside the ref update's chain"),
            "the merge message must win over junk classification: {msg1}"
        );

        // Attempt 2: verbatim retry — the objects are all known now.
        insert_commits(&storage, vec![mainline, side, merge.clone()]).await;
        let err2 = PushChain::resolve(
            &cmd,
            &pack,
            &id_set(&[]),
            Some(Commit::from_mega_model(merge.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();
        assert_eq!(msg1, err2.to_string(), "merge rejection must be sticky");
    }

    // A mid-chain merge (not the tip) is caught during the walk with the
    // merge-commit message naming the offending commit — before the side
    // ancestry could be misclassified as junk.
    #[tokio::test]
    async fn resolve_mid_chain_merge_is_named_before_junk_classification() {
        let (_temp, storage) = setup_storage().await;
        insert_commits(&storage, vec![commit_row(9500, &sha(9500), &[])]).await;
        let c1 = commit_row(9501, &sha(9501), &[sha(9500)]);
        let side = commit_row(9599, &sha(9599), &[sha(9500)]);
        let merge = commit_row(9502, &sha(9502), &[sha(9501), sha(9599)]);
        let tip = commit_row(9503, &sha(9503), &[sha(9502)]);
        insert_commits(
            &storage,
            vec![c1.clone(), side.clone(), merge.clone(), tip.clone()],
        )
        .await;
        let pack = id_set(&[&sha(9503), &sha(9502), &sha(9501), &sha(9599)]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());

        let err = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("merge commit"), "{msg}");
        assert!(msg.contains(&sha(9502)), "{msg}");
        assert!(
            !msg.contains("outside the ref update's chain"),
            "the merge message must win over junk classification: {msg}"
        );
    }

    // MC-06: the fork point left by the walk must be a known commit — otherwise
    // the CL would anchor on a hash whose tree the aggregate diff cannot read.
    #[tokio::test]
    async fn resolve_new_branch_rejects_unknown_fork_point() {
        let (_temp, storage) = setup_storage().await;
        // sha(200) itself is never inserted: the walk stops there and resolve
        // must fail closed.
        let tip = insert_linear_chain(&storage, 200, 2).await;
        let pack = id_set(&[&sha(201), &sha(202)]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());

        let err = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("not a known commit"), "{msg}");
        assert!(msg.contains(&sha(200)), "{msg}");
    }

    // MC-06: a pack whose commits do not include the ref update target fails
    // closed (the multi-commit generalization of the MC01-R1 P2-1 unpack-time
    // commit == new_id check).
    #[tokio::test]
    async fn resolve_rejects_pack_missing_ref_target() {
        let (_temp, storage) = setup_storage().await;
        let parent = ObjectHash::from_str("119bc457cb05b52dfb0d6b14f66d9a8a52d09e25").unwrap();
        let tip = test_commit(vec![parent]);
        // Distinct message so the decoy commit hashes differently from `tip`.
        let other = Commit::new(
            test_signature(SignatureType::Author),
            test_signature(SignatureType::Committer),
            tip.tree_id,
            vec![parent],
            "decoy commit, not the ref update target",
        );
        let cmd = branch_command(ZERO_ID.to_string(), tip.id.to_string());
        let pack = id_set(&[&other.id.to_string()]);

        let err = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(tip.clone()),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("does not contain the ref update target"),
            "{msg}"
        );
        assert!(msg.contains(&tip.id.to_string()), "{msg}");
    }

    // Defensive: a first-parent cycle wholly inside the pack must trip the
    // walk bound instead of looping forever.
    #[tokio::test]
    async fn resolve_new_branch_pack_cycle_trips_walk_bound() {
        let (_temp, storage) = setup_storage().await;
        let a = commit_row(301, &sha(301), &[sha(302)]);
        let b = commit_row(302, &sha(302), &[sha(301)]);
        insert_commits(&storage, vec![a.clone(), b]).await;
        let pack = id_set(&[&sha(301), &sha(302)]);
        let cmd = branch_command(ZERO_ID.to_string(), a.commit_id.clone());

        let err = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(Commit::from_mega_model(a)),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("cycle"), "{err}");
    }

    // MC06-R1 P1-1: the fork-point walk shares the validator's chain limit —
    // a 251-commit new-branch pack is rejected at the bound, not after walking
    // the whole chain. The fork point sha(4000) is deliberately never
    // inserted: without the bound, resolve would walk all 251 members and
    // surface the unknown-fork-point error ("not a known commit"); with it,
    // the length rejection fires first and sha(4001) is never even read.
    #[tokio::test]
    async fn resolve_new_branch_walk_is_bounded_at_chain_limit() {
        let (_temp, storage) = setup_storage().await;
        let tip = insert_linear_chain(&storage, 4000, MAX_CL_CHAIN_COMMITS as u64 + 1).await;
        let pack = id_range(4000, MAX_CL_CHAIN_COMMITS as u64 + 1);
        let cmd = branch_command(ZERO_ID.to_string(), tip.commit_id.clone());

        let err = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(Commit::from_mega_model(tip.clone())),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains(&format!("more than {MAX_CL_CHAIN_COMMITS} commits")),
            "{msg}"
        );
        assert!(!msg.contains("not a known commit"), "{msg}");
    }

    #[tokio::test]
    async fn resolve_rejects_orphan_tip_for_new_branch_push() {
        let (_temp, storage) = setup_storage().await;
        let tip = test_commit(Vec::new());
        let cmd = branch_command(ZERO_ID.to_string(), tip.id.to_string());
        let pack = id_set(&[&tip.id.to_string()]);

        let err = PushChain::resolve(
            &cmd,
            &pack,
            &pack.clone(),
            Some(tip),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("Can not init directory under monorepo directory")
        );
    }

    #[tokio::test]
    async fn resolve_empty_pack_with_known_new_id_is_noop() {
        let (_temp, storage) = setup_storage().await;
        let tip = test_commit(Vec::new());
        let cmd = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            tip.id.to_string(),
        );

        let resolution = PushChain::resolve(
            &cmd,
            &id_set(&[]),
            &id_set(&[]),
            Some(tip.clone()),
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap();

        let PushChainResolution::Noop { notice } = resolution else {
            panic!("expected an ADR-MC-05 no-op");
        };
        assert!(notice.contains(&tip.id.to_string()));
    }

    #[tokio::test]
    async fn resolve_empty_pack_with_unknown_new_id_fails() {
        let (_temp, storage) = setup_storage().await;
        let new_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let cmd = branch_command(ZERO_ID.to_string(), new_id.clone());

        let err = PushChain::resolve(
            &cmd,
            &id_set(&[]),
            &id_set(&[]),
            None,
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("empty pack"));
        assert!(err.to_string().contains(&new_id));
    }

    #[tokio::test]
    async fn resolve_pack_commit_missing_from_storage_fails() {
        let (_temp, storage) = setup_storage().await;
        let new_id = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let cmd = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            new_id.clone(),
        );

        let err = PushChain::resolve(
            &cmd,
            &id_set(&[&new_id]),
            &id_set(&[&new_id]),
            None,
            &storage.mono_storage(),
            MAX_CL_CHAIN_COMMITS,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains(&new_id));
    }

    #[test]
    fn primary_branch_command_skips_delete_commands() {
        let delete = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            ZERO_ID.to_string(),
        );
        let update = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            "27dd8d4cf39f3868c6eee38b601bc9e9939304f5".to_string(),
        );

        let selected = primary_branch_command(&[delete, update.clone()]);

        assert_eq!(selected, Some(update));
    }

    #[test]
    fn primary_branch_command_returns_none_for_delete_only_push() {
        let delete = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            ZERO_ID.to_string(),
        );

        assert_eq!(primary_branch_command(&[delete]), None);
        assert_eq!(primary_branch_command(&[]), None);
    }

    #[test]
    fn attribution_commit_id_is_tip_independent_of_pack_order() {
        // The commit entry may arrive after tree/blob entries in the pack
        // stream; attribution must still be the chain tip — the ref commands
        // are its only input (MC01-R2 P1-2).
        let update = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            "27dd8d4cf39f3868c6eee38b601bc9e9939304f5".to_string(),
        );
        assert_eq!(
            attribution_commit_id(&[update]),
            "27dd8d4cf39f3868c6eee38b601bc9e9939304f5"
        );

        // Tag-only pushes attribute nothing, as before.
        let tag = RefCommand::new(
            ZERO_ID.to_string(),
            "27dd8d4cf39f3868c6eee38b601bc9e9939304f5".to_string(),
            "refs/tags/v1".to_string(),
        );
        assert_eq!(attribution_commit_id(&[tag]), "");
    }

    const AUTHOR: &str = "author Test User <test@example.com> 1750000000 +0000";
    const COMMITTER: &str = "committer Test User <test@example.com> 1750000000 +0000";

    /// Fabricated 40-hex object id (same scheme as the gpg_signature_checker
    /// tests).
    fn sha(n: u64) -> String {
        format!("{n:040x}")
    }

    fn commit_row(id: i64, commit_sha: &str, parents: &[String]) -> mega_commit::Model {
        mega_commit::Model {
            id,
            commit_id: commit_sha.to_string(),
            tree: sha(900_000 + id as u64),
            parents_id: serde_json::json!(parents),
            author: Some(AUTHOR.to_string()),
            committer: Some(COMMITTER.to_string()),
            content: Some(format!("chain member {commit_sha}")),
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    async fn insert_commits(storage: &Storage, rows: Vec<mega_commit::Model>) {
        let models: Vec<mega_commit::ActiveModel> = rows
            .into_iter()
            .map(|m| mega_commit::ActiveModel {
                id: Set(m.id),
                commit_id: Set(m.commit_id),
                tree: Set(m.tree),
                parents_id: Set(m.parents_id),
                author: Set(m.author),
                committer: Set(m.committer),
                content: Set(m.content),
                created_at: Set(m.created_at),
                pack_id: Set(m.pack_id),
                pack_offset: Set(m.pack_offset),
            })
            .collect();
        mega_commit::Entity::insert_many(models)
            .exec(storage.mono_storage().get_connection())
            .await
            .expect("insert commits");
    }

    /// Insert commits `sha(start + 1) ..= sha(start + len)` as a linear
    /// first-parent chain rooted at `sha(start)` and return the tip row. The
    /// base row itself is not inserted — validation stops at the base.
    async fn insert_linear_chain(storage: &Storage, start: u64, len: u64) -> mega_commit::Model {
        let rows: Vec<_> = (1..=len)
            .map(|i| commit_row((start + i) as i64, &sha(start + i), &[sha(start + i - 1)]))
            .collect();
        let tip = rows.last().cloned().expect("non-empty chain");
        insert_commits(storage, rows).await;
        tip
    }

    /// The chain as `PushChain::resolve` builds it after its bounded walk:
    /// `base`, the tip, and the full increment `(base, tip]` in memory, tip
    /// first. `increment` holds the increment's commit rows, tip first.
    fn chain_from_increment(base: &str, increment: &[mega_commit::Model]) -> PushChain {
        let ordered: Vec<Commit> = increment
            .iter()
            .map(|row| Commit::from_mega_model(row.clone()))
            .collect();
        let tip = ordered.first().expect("non-empty increment").clone();
        PushChain {
            base: base.to_string(),
            tip,
            ordered_commits: ordered,
        }
    }

    /// Rows for a linear first-parent increment `sha(start+1)..=sha(start+len)`
    /// rooted at `sha(start)`, returned tip-first (NOT inserted — the
    /// increment segment is validated from memory, Codex R1 P1-3).
    fn increment_rows(start: u64, len: u64) -> Vec<mega_commit::Model> {
        let mut rows: Vec<_> = (1..=len)
            .map(|i| commit_row((start + i) as i64, &sha(start + i), &[sha(start + i - 1)]))
            .collect();
        rows.reverse();
        rows
    }

    fn open_cl(from_hash: &str, to_hash: &str) -> mega_cl::Model {
        mega_cl::Model {
            id: 1,
            link: "CLCHAIN01".to_string(),
            title: "chain validator test".to_string(),
            merge_date: None,
            status: MergeStatusEnum::Open,
            path: "/".to_string(),
            from_hash: from_hash.to_string(),
            to_hash: to_hash.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            username: "tester".to_string(),
            base_branch: "main".to_string(),
            revision: 0,
        }
    }

    async fn setup_storage() -> (TempDir, Storage) {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        (temp, storage)
    }

    // AC-7: existing single-commit pushes pass the validator unchanged — both
    // the fresh-CL form (no open CL) and the update form (open CL whose
    // frozen `from_hash` equals the push base).
    #[tokio::test]
    async fn validate_accepts_single_commit_push() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(100);
        let tip = commit_row(101, &sha(101), std::slice::from_ref(&base));
        let cmd = branch_command(base.clone(), tip.commit_id.clone());
        let chain = chain_from_increment(&base, std::slice::from_ref(&tip));

        chain
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect("single-commit push without an open CL must pass");

        let cl = open_cl(&base, &tip.commit_id);
        chain
            .validate(
                &cmd,
                &storage.mono_storage(),
                Some(&cl),
                MAX_CL_CHAIN_COMMITS,
            )
            .await
            .expect("single-commit push with an open CL must pass");
    }

    // AC-1: a chain containing a merge commit (≥2 parents) is rejected.
    #[tokio::test]
    async fn validate_rejects_merge_commit() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(200);
        let merge = commit_row(201, &sha(201), &[base.clone(), sha(299)]);
        let tip = commit_row(202, &sha(202), &[sha(201)]);
        let cmd = branch_command(base.clone(), tip.commit_id.clone());

        let err = chain_from_increment(&base, &[tip, merge])
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect_err("a merge commit in the chain must be rejected");

        let msg = err.to_string();
        assert!(msg.contains("merge commit"), "{msg}");
        assert!(msg.contains(&sha(201)), "{msg}");
    }

    // AC-2: broken topology is rejected fail-closed — an increment that does
    // not terminate at the base (parent outside the chain, a root that ends
    // the chain early) or a base outside the chain entirely hit the same
    // error family. (Objects missing from storage are resolve's job: its walk
    // reads every increment commit and fails closed there.)
    #[tokio::test]
    async fn validate_rejects_broken_topology() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(300);

        // The increment's only commit is parented outside the claimed base.
        let tip = commit_row(301, &sha(301), &[sha(399)]);
        let cmd = branch_command(base.clone(), tip.commit_id.clone());
        let err = chain_from_increment(&base, std::slice::from_ref(&tip))
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect_err("an increment not reaching its base must be rejected");
        assert!(err.to_string().contains("push chain is broken"), "{err}");

        // Discontinuity: the chain roots out (no parent) before reaching base.
        let root = commit_row(310, &sha(310), &[]);
        let tip2 = commit_row(311, &sha(311), &[sha(310)]);
        let cmd2 = branch_command(base.clone(), tip2.commit_id.clone());
        let err2 = chain_from_increment(&base, &[tip2, root])
            .validate(&cmd2, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect_err("a chain that never reaches its base must be rejected");
        assert!(err2.to_string().contains("push chain is broken"), "{err2}");

        // With an open CL: the increment terminates at the CL's `from_hash`
        // ancestry, not at the claimed push base — the base is not on the
        // tip's chain.
        let increment = increment_rows(350, 2);
        let cl = open_cl(&sha(350), &increment[0].commit_id);
        let cmd3 = branch_command(sha(999), increment[0].commit_id.clone());
        let err3 = chain_from_increment(&sha(999), &increment)
            .validate(
                &cmd3,
                &storage.mono_storage(),
                Some(&cl),
                MAX_CL_CHAIN_COMMITS,
            )
            .await
            .expect_err("a base outside the chain must be rejected");
        assert!(err3.to_string().contains("push chain is broken"), "{err3}");
    }

    // AC-3: a parent cycle (corrupt object graph) is rejected. resolve's walk
    // bound keeps real packs cycle-free; a hand-built chain that revisits a
    // commit must still fail closed here.
    #[tokio::test]
    async fn validate_rejects_cycle() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(400);
        // tip → a → b → a → … — never reaches base.
        let a = commit_row(401, &sha(401), &[sha(402)]);
        let b = commit_row(402, &sha(402), &[sha(401)]);
        let tip = commit_row(403, &sha(403), &[sha(401)]);
        let cmd = branch_command(base.clone(), tip.commit_id.clone());

        let err = chain_from_increment(&base, &[tip, a.clone(), b, a])
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect_err("a parent cycle must be rejected");

        assert!(err.to_string().contains("cycle"), "{err}");
    }

    // AC-4: tip != cmd.new_id is rejected before any storage read.
    #[tokio::test]
    async fn validate_rejects_tip_mismatch() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(500);
        let tip = commit_row(501, &sha(501), std::slice::from_ref(&base));
        let cmd = branch_command(base.clone(), sha(599));

        let err = chain_from_increment(&base, std::slice::from_ref(&tip))
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect_err("a chain tip that is not the ref update target must be rejected");

        let msg = err.to_string();
        assert!(msg.contains("does not match"), "{msg}");
        assert!(msg.contains(&sha(599)), "{msg}");
    }

    // AC-5/AC-6: exactly MAX_CL_CHAIN_COMMITS commits pass; one more is
    // rejected. The increment is validated from memory, so no commit rows are
    // inserted at all (Codex R1 P1-3).
    #[tokio::test]
    async fn validate_chain_length_boundary() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(1000);

        // AC-5: (base → sha(1250)] has exactly MAX_CL_CHAIN_COMMITS commits.
        let at_limit = increment_rows(1000, MAX_CL_CHAIN_COMMITS as u64);
        let cmd = branch_command(base.clone(), at_limit[0].commit_id.clone());
        chain_from_increment(&base, &at_limit)
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect("a chain at the limit must pass");

        // AC-6: one more commit crosses the limit.
        let over_limit = increment_rows(1000, MAX_CL_CHAIN_COMMITS as u64 + 1);
        let cmd = branch_command(base.clone(), over_limit[0].commit_id.clone());
        let err = chain_from_increment(&base, &over_limit)
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect_err("a chain over the limit must be rejected");
        assert!(
            err.to_string().contains(&format!("{MAX_CL_CHAIN_COMMITS}")),
            "{err}"
        );
    }

    // Codex R1 P1-3: the increment segment is checked entirely in memory —
    // validate must not re-read the pack segment from the DB. Proof by
    // absence: a 250-commit increment validates against an *empty*
    // `mega_commit` table; with an open CL, only the history segment's rows
    // exist in storage.
    #[tokio::test]
    async fn validate_increment_segment_needs_no_db_reads() {
        let (_temp, storage) = setup_storage().await;

        // No open CL: zero rows in mega_commit at all.
        let base = sha(8000);
        let at_limit = increment_rows(8000, MAX_CL_CHAIN_COMMITS as u64);
        let cmd = branch_command(base.clone(), at_limit[0].commit_id.clone());
        chain_from_increment(&base, &at_limit)
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect("a 250-commit increment must validate with an empty mega_commit table");

        // Open CL: the increment rows stay absent; only history rows exist.
        let from = sha(8500);
        let base = sha(8600);
        insert_linear_chain(&storage, 8500, 100).await;
        let increment = increment_rows(8600, 100);
        let cl = open_cl(&from, &base);
        let cmd = branch_command(base.clone(), increment[0].commit_id.clone());
        chain_from_increment(&base, &increment)
            .validate(
                &cmd,
                &storage.mono_storage(),
                Some(&cl),
                MAX_CL_CHAIN_COMMITS,
            )
            .await
            .expect("a 100+100 cumulative chain must pass reading only the history segment");
    }

    // AC-8 (ADR-MC-07): updating an open CL validates the cumulative range
    // `cl.from_hash → new tip`. The first 200-commit push passes; a second
    // push of 50 lands exactly at the 250 cumulative limit and passes; a
    // second push of 100 is a legal increment on its own but crosses the
    // cumulative limit and is rejected (the 200+100 scenario).
    #[tokio::test]
    async fn validate_cumulative_limit_for_open_cl_update() {
        let (_temp, storage) = setup_storage().await;
        let from = sha(2000);

        // First push: 200 commits (sha(2000) → sha(2200)]; no open CL yet.
        let first = increment_rows(2000, 200);
        let cmd1 = branch_command(from.clone(), first[0].commit_id.clone());
        chain_from_increment(&from, &first)
            .validate(&cmd1, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect("the first 200-commit push must pass");

        // The CL now exists with the frozen from_hash and the first tip. Its
        // history lives in storage; the second push's increments stay
        // memory-only (the increment segment is never re-read, Codex R1 P1-3).
        let cl = open_cl(&from, &first[0].commit_id);
        insert_linear_chain(&storage, 2000, 200).await;

        // Second push of 50: cumulative range is exactly 250 — passes.
        let ok_increment = increment_rows(2200, 50);
        let cmd_ok = branch_command(
            first[0].commit_id.clone(),
            ok_increment[0].commit_id.clone(),
        );
        chain_from_increment(&first[0].commit_id, &ok_increment)
            .validate(
                &cmd_ok,
                &storage.mono_storage(),
                Some(&cl),
                MAX_CL_CHAIN_COMMITS,
            )
            .await
            .expect("a cumulative range at the limit must pass");

        // Second push of 100: the 100-commit increment is legal on its own,
        // but the cumulative range (sha(2000) → sha(2300)] is 300 — rejected.
        let over_increment = increment_rows(2200, 100);
        let cmd2 = branch_command(
            first[0].commit_id.clone(),
            over_increment[0].commit_id.clone(),
        );
        let err = chain_from_increment(&first[0].commit_id, &over_increment)
            .validate(
                &cmd2,
                &storage.mono_storage(),
                Some(&cl),
                MAX_CL_CHAIN_COMMITS,
            )
            .await
            .expect_err("a cumulative range over the limit must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("cumulative"), "{msg}");
        assert!(msg.contains(&cl.link), "{msg}");
        assert!(
            msg.contains("merge the current CL first or open a new CL"),
            "{msg}"
        );
    }

    // Codex R1 P1 (ADR-MC-07): the historical segment `(from_hash, base]` of
    // an open CL is count-only — a merge commit in it must not reject a clean
    // increment; an overlong history is still rejected by the cumulative
    // count.
    #[tokio::test]
    async fn validate_cl_history_is_count_only() {
        let (_temp, storage) = setup_storage().await;

        // Within limit: from=sha(700), history sha(701..=703) with the middle
        // commit a merge (parents [sha(701), sha(799)], the second parent
        // never walked), base=sha(703), clean 1-commit increment sha(704).
        let from = sha(700);
        let base = sha(703);
        let cl = open_cl(&from, &base);
        insert_commits(
            &storage,
            vec![
                commit_row(701, &sha(701), std::slice::from_ref(&from)),
                commit_row(702, &sha(702), &[sha(701), sha(799)]),
                commit_row(703, &base, &[sha(702)]),
            ],
        )
        .await;
        let tip = commit_row(704, &sha(704), std::slice::from_ref(&base));
        let cmd = branch_command(base.clone(), tip.commit_id.clone());
        chain_from_increment(&base, std::slice::from_ref(&tip))
            .validate(
                &cmd,
                &storage.mono_storage(),
                Some(&cl),
                MAX_CL_CHAIN_COMMITS,
            )
            .await
            .expect("a merge commit in CL history must not reject a clean increment");

        // Over limit: 249-commit history (one member a merge) plus a legal
        // 2-commit increment → cumulative 251 → rejected on the cumulative
        // branch, not on the historical merge.
        let from = sha(3000);
        let base = sha(3000 + 249);
        let cl = open_cl(&from, &base);
        let rows: Vec<_> = (1..=249u64)
            .map(|i| {
                let parents = if i == 100 {
                    vec![sha(3000 + i - 1), sha(3999)]
                } else {
                    vec![sha(3000 + i - 1)]
                };
                commit_row((3000 + i) as i64, &sha(3000 + i), &parents)
            })
            .collect();
        insert_commits(&storage, rows).await;
        let increment = increment_rows(3000 + 249, 2);
        let cmd = branch_command(base.clone(), increment[0].commit_id.clone());
        let err = chain_from_increment(&base, &increment)
            .validate(
                &cmd,
                &storage.mono_storage(),
                Some(&cl),
                MAX_CL_CHAIN_COMMITS,
            )
            .await
            .expect_err("an overlong CL history must be rejected by the cumulative count");
        let msg = err.to_string();
        assert!(msg.contains("cumulative"), "{msg}");
        assert!(
            msg.contains("merge the current CL first or open a new CL"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn validate_respects_caller_max_commits() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(9000);
        let three = increment_rows(9000, 3);
        let cmd = branch_command(base.clone(), three[0].commit_id.clone());
        chain_from_increment(&base, &three)
            .validate(&cmd, &storage.mono_storage(), None, 2)
            .await
            .expect_err("trunk max_push_commits=2 must reject a 3-commit increment");
        chain_from_increment(&base, &three)
            .validate(&cmd, &storage.mono_storage(), None, MAX_CL_CHAIN_COMMITS)
            .await
            .expect("review MAX_CL_CHAIN_COMMITS=250 still accepts a 3-commit increment");
    }

    /// TP-03 Verification: operation_id fingerprint semantics (trunk-push §1.11).
    #[test]
    fn tp03_operation_id_fingerprints_match_section_1_11() {
        use crate::jupiter::service::push_queue_service::{
            attach_operation_id, merge_operation_id, push_operation_id,
        };

        // Same tip, different baselines → distinct push ops (A→C ≠ B→C).
        assert_ne!(
            push_operation_id(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "cccccccccccccccccccccccccccccccccccccccc"
            ),
            push_operation_id(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "cccccccccccccccccccccccccccccccccccccccc"
            ),
        );
        assert!(
            push_operation_id("a", "c").contains('→'),
            "push fingerprint uses U+2192 arrow"
        );
        // Different CLs sharing to_hash must not collide (merge = cl.link).
        assert_ne!(merge_operation_id("CL-1"), merge_operation_id("CL-2"));
        assert_eq!(merge_operation_id("CL-1"), "CL-1");
        // Attach is content-addressed and stable.
        let a = attach_operation_id("repo-1", "refs/heads/main:aaa");
        let b = attach_operation_id("repo-1", "refs/heads/main:aaa");
        let c = attach_operation_id("repo-1", "refs/heads/main:bbb");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }
}
