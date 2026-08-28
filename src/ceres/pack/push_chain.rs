//! Push-chain state model for the MonoRepo receive-pack path.
//!
//! Push semantics (base/tip/attribution) derive solely from
//! `RefCommand.old_id/new_id` plus the unpacked tip commit's parent chain —
//! never from pack arrival order (GC-MC-13, plan-20260827).
//!
//! [`PushChain::validate`] is the chain validator (plan-20260827 MC-03): the
//! rejection surface for merge commits, broken topology, cycles, tip
//! mismatch, and the increment/cumulative length bounds. It is deliberately
//! not wired into the receive-pack path yet — MC-06 turns it on together
//! with multi-commit push.

use std::collections::HashSet;

use git_internal::internal::object::commit::Commit;

use crate::{
    callisto::{mega_cl, sea_orm_active_enums::RefTypeEnum},
    ceres::{
        merge_checker::MAX_CL_CHAIN_COMMITS,
        protocol::import_refs::{CommandType, RefCommand},
    },
    common::{errors::MegaError, utils::ZERO_ID},
    jupiter::storage::mono_storage::MonoStorage,
};

/// Push semantics state for one receive-pack.
///
/// Built after unpack from the branch `RefCommand` and the stored tip commit;
/// replaces the former `current_commit` slot that captured the first commit in
/// pack arrival order.
#[derive(Debug, Clone)]
pub struct PushChain {
    /// Chain base: `RefCommand.old_id`, or the tip's first parent for a
    /// new-branch push (`old_id == ZERO_ID`). Feeds the CL `from_hash`.
    pub base: String,
    /// Chain tip: the commit `RefCommand.new_id` points to. Feeds the CL
    /// `to_hash` and is the single source of CL ref `ref_commit_hash` /
    /// `ref_tree_hash`, file-path indexing, and object `commit_id`
    /// attribution (ADR-MC-06).
    pub tip: Commit,
    /// Commits introduced by this push, tip first. `check_entry` still rejects
    /// multi-commit packs, so this holds exactly one commit; MC-06 turns
    /// construction into a full parent-chain walk when multi-commit push
    /// opens up.
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
    /// * `pack_saw_commit` — whether unpack delivered a commit object.
    /// * `tip_commit` — post-unpack storage lookup of `cmd.new_id`.
    ///
    /// Error paths are explicit (GC-MC-14, no silent returns): an empty pack
    /// pointing at an unknown `new_id`, or a pack commit whose `new_id` never
    /// landed in storage, both fail the push with an actionable message.
    pub fn resolve(
        cmd: &RefCommand,
        pack_saw_commit: bool,
        tip_commit: Option<Commit>,
    ) -> Result<PushChainResolution, MegaError> {
        debug_assert!(
            cmd.command_type != CommandType::Delete && cmd.new_id != ZERO_ID,
            "push chain is only resolved for non-delete branch commands"
        );
        match (pack_saw_commit, tip_commit) {
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
                let base = if cmd.old_id != ZERO_ID {
                    cmd.old_id.clone()
                } else {
                    tip.parent_commit_ids
                        .first()
                        .map(ToString::to_string)
                        .ok_or_else(|| {
                            MegaError::Other(
                                "Can not init directory under monorepo directory!".to_string(),
                            )
                        })?
                };
                let ordered_commits = vec![tip.clone()];
                Ok(PushChainResolution::Chain(Box::new(PushChain {
                    base,
                    tip,
                    ordered_commits,
                })))
            }
        }
    }

    /// Validate the resolved chain against commit storage (plan-20260827
    /// MC-03). Every rejection path is fail-closed with an actionable
    /// message. Not wired into the receive-pack path yet — MC-06 calls this
    /// when multi-commit push opens up; until then unit tests drive it
    /// directly.
    ///
    /// A single first-parent walk over the `mega_commit` table serves both
    /// length bounds (ADR-MC-07): the push increment ends at `base`; when the
    /// push updates an open CL the walk continues to the CL's frozen
    /// `from_hash`, bounding the CL's cumulative range `(from_hash → tip]`.
    /// At most `MAX_CL_CHAIN_COMMITS + 1` primary-key reads. The walk is
    /// DB-driven and does not consult `ordered_commits` (still only the tip
    /// until MC-06). Exactly `MAX_CL_CHAIN_COMMITS` commits pass; one more is
    /// rejected.
    ///
    /// The walk is two-segment (ADR-MC-07): the increment `(base, tip]` gets
    /// the full rejection surface (merge commits, broken topology, cycles,
    /// length); the pre-existing CL history `(from_hash, base]` is count-only
    /// — it was validated when it was pushed, so a legacy anomaly there must
    /// not reject a legal increment. History still fails closed when counting
    /// is impossible: a missing object, unparseable `parents_id`, a parent
    /// cycle (would otherwise loop forever), or a chain that never reaches
    /// `from_hash`.
    pub async fn validate(
        &self,
        cmd: &RefCommand,
        storage: &MonoStorage,
        open_cl: Option<&mega_cl::Model>,
    ) -> Result<(), MegaError> {
        let tip_id = self.tip.id.to_string();
        if tip_id != cmd.new_id {
            return Err(MegaError::Other(format!(
                "push chain tip {tip_id} does not match the ref update new_id {} for `{}`; \
                 the pack's tip commit must be the ref update target",
                cmd.new_id, cmd.ref_name
            )));
        }

        // ADR-MC-07: the walk boundary is the open CL's frozen `from_hash`
        // (cumulative range) when one is reused, otherwise the push base
        // (this push's increment only).
        let boundary = open_cl.map_or_else(|| self.base.as_str(), |cl| cl.from_hash.as_str());
        let mut visited = HashSet::new();
        let mut passed_base = false;
        let mut walked = 0usize;
        let mut current = tip_id.clone();
        loop {
            if current == self.base {
                // Crossing the push base: everything above is the increment.
                passed_base = true;
            }
            if current == boundary {
                break;
            }
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
            let parents: Vec<String> =
                serde_json::from_value(model.parents_id.clone()).map_err(|e| {
                    MegaError::Other(format!(
                        "push chain is broken: corrupt parents_id for commit {current}: {e}"
                    ))
                })?;
            // Merge commits are rejected only in this push's increment
            // (ADR-MC-07: the historical segment is count-only).
            if !passed_base && parents.len() > 1 {
                return Err(MegaError::Other(format!(
                    "push chain contains merge commit {current} ({} parents); \
                     monorepo CLs are linear — rebase the branch and push again",
                    parents.len()
                )));
            }
            walked += 1;
            if walked > MAX_CL_CHAIN_COMMITS {
                return Err(match (passed_base, open_cl) {
                    // The increment itself fit; the overflow comes from the
                    // pre-existing CL history — a cumulative violation.
                    (true, Some(cl)) => MegaError::Other(format!(
                        "updating CL {} would exceed the {MAX_CL_CHAIN_COMMITS}-commit \
                         cumulative limit ({} → {tip_id}); \
                         merge the current CL first or open a new CL",
                        cl.link, cl.from_hash
                    )),
                    _ => MegaError::Other(format!(
                        "push introduces more than {MAX_CL_CHAIN_COMMITS} commits in one \
                         chain; split the changes into smaller pushes or squash and re-push"
                    )),
                });
            }
            current = match parents.first() {
                Some(parent) => parent.clone(),
                None => {
                    return Err(MegaError::Other(format!(
                        "push chain is broken: commit {current} has no parent, but the chain \
                         never reached base {boundary}; rebase the pushed commits onto the \
                         current base and re-push"
                    )));
                }
            };
        }
        if !passed_base {
            return Err(MegaError::Other(format!(
                "push chain is broken: base {} is not on the first-parent chain of tip \
                 {tip_id}; rebase onto the current base and re-push",
                self.base
            )));
        }
        Ok(())
    }
}

/// The semantics-defining command of a push: the first non-delete branch
/// command. Delete commands never build a chain; multi-branch pushes remain
/// as-is until MC-06 rejects them (ADR-MC-04).
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
/// Safety: a MonoRepo branch push with objects always carries its commit in
/// the pack, and `check_entry` already fail-closed on `commit != new_id`;
/// delete-only pushes skip unpack entirely and empty packs have no entries, so
/// neither reaches `save_entry`. A pathological "trees but no commit" pack is
/// attributed to `new_id`, but `batch_save_model` is insert-only (on-conflict
/// do-nothing), so pre-existing rows keep their attribution.
pub fn attribution_commit_id(commands: &[RefCommand]) -> String {
    primary_branch_command(commands)
        .map(|cmd| cmd.new_id)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

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

    #[test]
    fn resolve_builds_single_commit_chain_for_existing_ref() {
        let parent = ObjectHash::from_str("119bc457cb05b52dfb0d6b14f66d9a8a52d09e25").unwrap();
        let tip = test_commit(vec![parent]);
        let old_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let cmd = branch_command(old_id.clone(), tip.id.to_string());

        let resolution = PushChain::resolve(&cmd, true, Some(tip.clone())).unwrap();

        let PushChainResolution::Chain(chain) = resolution else {
            panic!("expected a push chain");
        };
        assert_eq!(chain.base, old_id);
        assert_eq!(chain.tip.id, tip.id);
        assert_eq!(chain.ordered_commits.len(), 1);
        assert_eq!(chain.ordered_commits[0].id, tip.id);
    }

    #[test]
    fn resolve_uses_tip_first_parent_as_base_for_new_branch_push() {
        let parent = ObjectHash::from_str("119bc457cb05b52dfb0d6b14f66d9a8a52d09e25").unwrap();
        let tip = test_commit(vec![parent]);
        let cmd = branch_command(ZERO_ID.to_string(), tip.id.to_string());

        let resolution = PushChain::resolve(&cmd, true, Some(tip.clone())).unwrap();

        let PushChainResolution::Chain(chain) = resolution else {
            panic!("expected a push chain");
        };
        assert_eq!(chain.base, parent.to_string());
        assert_eq!(chain.tip.id, tip.id);
    }

    #[test]
    fn resolve_rejects_orphan_tip_for_new_branch_push() {
        let tip = test_commit(Vec::new());
        let cmd = branch_command(ZERO_ID.to_string(), tip.id.to_string());

        let err = PushChain::resolve(&cmd, true, Some(tip)).unwrap_err();

        assert!(
            err.to_string()
                .contains("Can not init directory under monorepo directory")
        );
    }

    #[test]
    fn resolve_empty_pack_with_known_new_id_is_noop() {
        let tip = test_commit(Vec::new());
        let cmd = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            tip.id.to_string(),
        );

        let resolution = PushChain::resolve(&cmd, false, Some(tip.clone())).unwrap();

        let PushChainResolution::Noop { notice } = resolution else {
            panic!("expected an ADR-MC-05 no-op");
        };
        assert!(notice.contains(&tip.id.to_string()));
    }

    #[test]
    fn resolve_empty_pack_with_unknown_new_id_fails() {
        let new_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let cmd = branch_command(ZERO_ID.to_string(), new_id.clone());

        let err = PushChain::resolve(&cmd, false, None).unwrap_err();

        assert!(err.to_string().contains("empty pack"));
        assert!(err.to_string().contains(&new_id));
    }

    #[test]
    fn resolve_pack_commit_missing_from_storage_fails() {
        let new_id = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let cmd = branch_command(
            "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25".to_string(),
            new_id.clone(),
        );

        let err = PushChain::resolve(&cmd, true, None).unwrap_err();

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

    /// The chain as `PushChain::resolve` would build it: base from the ref
    /// command, tip from storage, `ordered_commits` still just the tip.
    fn chain_for(tip_row: &mega_commit::Model, base: &str) -> PushChain {
        let tip = Commit::from_mega_model(tip_row.clone());
        PushChain {
            base: base.to_string(),
            ordered_commits: vec![tip.clone()],
            tip,
        }
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
        let tip = insert_linear_chain(&storage, 100, 1).await;
        let cmd = branch_command(base.clone(), tip.commit_id.clone());
        let chain = chain_for(&tip, &base);

        chain
            .validate(&cmd, &storage.mono_storage(), None)
            .await
            .expect("single-commit push without an open CL must pass");

        let cl = open_cl(&base, &tip.commit_id);
        chain
            .validate(&cmd, &storage.mono_storage(), Some(&cl))
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
        insert_commits(&storage, vec![merge, tip.clone()]).await;
        let cmd = branch_command(base.clone(), tip.commit_id.clone());

        let err = chain_for(&tip, &base)
            .validate(&cmd, &storage.mono_storage(), None)
            .await
            .expect_err("a merge commit in the chain must be rejected");

        let msg = err.to_string();
        assert!(msg.contains("merge commit"), "{msg}");
        assert!(msg.contains(&sha(201)), "{msg}");
    }

    // AC-2: broken topology is rejected fail-closed — a parent object missing
    // from storage, a chain that roots out before reaching the base, and a
    // base that is not on the chain at all hit the same error family.
    #[tokio::test]
    async fn validate_rejects_broken_topology() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(300);

        // Missing object: the tip's parent is not in storage.
        let tip = commit_row(301, &sha(301), &[sha(399)]);
        insert_commits(&storage, vec![tip.clone()]).await;
        let cmd = branch_command(base.clone(), tip.commit_id.clone());
        let err = chain_for(&tip, &base)
            .validate(&cmd, &storage.mono_storage(), None)
            .await
            .expect_err("a missing parent object must be rejected");
        assert!(err.to_string().contains("push chain is broken"), "{err}");

        // Discontinuity: the chain roots out (no parent) before reaching base.
        let root = commit_row(310, &sha(310), &[]);
        let tip2 = commit_row(311, &sha(311), &[sha(310)]);
        insert_commits(&storage, vec![root, tip2.clone()]).await;
        let cmd2 = branch_command(base.clone(), tip2.commit_id.clone());
        let err2 = chain_for(&tip2, &base)
            .validate(&cmd2, &storage.mono_storage(), None)
            .await
            .expect_err("a chain that never reaches its base must be rejected");
        assert!(err2.to_string().contains("push chain is broken"), "{err2}");

        // With an open CL: the walk reaches the CL's `from_hash` without ever
        // passing the push base — the base is not an ancestor of the tip.
        let tip3 = insert_linear_chain(&storage, 350, 2).await;
        let cl = open_cl(&sha(350), &tip3.commit_id);
        let cmd3 = branch_command(sha(999), tip3.commit_id.clone());
        let err3 = chain_for(&tip3, &sha(999))
            .validate(&cmd3, &storage.mono_storage(), Some(&cl))
            .await
            .expect_err("a base outside the chain must be rejected");
        assert!(err3.to_string().contains("push chain is broken"), "{err3}");
    }

    // AC-3: a parent cycle (corrupt object graph) is rejected.
    #[tokio::test]
    async fn validate_rejects_cycle() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(400);
        // tip → a → b → a → … — never reaches base.
        let a = commit_row(401, &sha(401), &[sha(402)]);
        let b = commit_row(402, &sha(402), &[sha(401)]);
        let tip = commit_row(403, &sha(403), &[sha(401)]);
        insert_commits(&storage, vec![a, b, tip.clone()]).await;
        let cmd = branch_command(base.clone(), tip.commit_id.clone());

        let err = chain_for(&tip, &base)
            .validate(&cmd, &storage.mono_storage(), None)
            .await
            .expect_err("a parent cycle must be rejected");

        assert!(err.to_string().contains("cycle"), "{err}");
    }

    // AC-4: tip != cmd.new_id is rejected before any storage read.
    #[tokio::test]
    async fn validate_rejects_tip_mismatch() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(500);
        let tip = insert_linear_chain(&storage, 500, 1).await;
        let cmd = branch_command(base.clone(), sha(599));

        let err = chain_for(&tip, &base)
            .validate(&cmd, &storage.mono_storage(), None)
            .await
            .expect_err("a chain tip that is not the ref update target must be rejected");

        let msg = err.to_string();
        assert!(msg.contains("does not match"), "{msg}");
        assert!(msg.contains(&sha(599)), "{msg}");
    }

    // AC-5/AC-6: exactly MAX_CL_CHAIN_COMMITS commits pass; one more is
    // rejected.
    #[tokio::test]
    async fn validate_chain_length_boundary() {
        let (_temp, storage) = setup_storage().await;
        let base = sha(1000);
        let over_tip = insert_linear_chain(&storage, 1000, MAX_CL_CHAIN_COMMITS as u64 + 1).await;

        // AC-5: (base → sha(1250)] has exactly MAX_CL_CHAIN_COMMITS commits.
        let at_limit = 1000 + MAX_CL_CHAIN_COMMITS as u64;
        let at_limit_tip = commit_row(at_limit as i64, &sha(at_limit), &[sha(at_limit - 1)]);
        let cmd = branch_command(base.clone(), at_limit_tip.commit_id.clone());
        chain_for(&at_limit_tip, &base)
            .validate(&cmd, &storage.mono_storage(), None)
            .await
            .expect("a chain at the limit must pass");

        // AC-6: one more commit crosses the limit.
        let cmd = branch_command(base.clone(), over_tip.commit_id.clone());
        let err = chain_for(&over_tip, &base)
            .validate(&cmd, &storage.mono_storage(), None)
            .await
            .expect_err("a chain over the limit must be rejected");
        assert!(
            err.to_string().contains(&format!("{MAX_CL_CHAIN_COMMITS}")),
            "{err}"
        );
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
        let first_tip = insert_linear_chain(&storage, 2000, 200).await;
        let cmd1 = branch_command(from.clone(), first_tip.commit_id.clone());
        chain_for(&first_tip, &from)
            .validate(&cmd1, &storage.mono_storage(), None)
            .await
            .expect("the first 200-commit push must pass");

        // The CL now exists with the frozen from_hash and the first tip.
        // Second-push candidates hang off that tip; rows sha(2201..=2300) are
        // inserted once.
        let cl = open_cl(&from, &first_tip.commit_id);
        let over_tip = insert_linear_chain(&storage, 2200, 100).await;

        // Second push of 50: cumulative range is exactly 250 — passes.
        let ok_tip = commit_row(2250, &sha(2250), &[sha(2249)]);
        let cmd_ok = branch_command(first_tip.commit_id.clone(), ok_tip.commit_id.clone());
        chain_for(&ok_tip, &first_tip.commit_id)
            .validate(&cmd_ok, &storage.mono_storage(), Some(&cl))
            .await
            .expect("a cumulative range at the limit must pass");

        // Second push of 100: the 100-commit increment is legal on its own,
        // but the cumulative range (sha(2000) → sha(2300)] is 300 — rejected.
        let cmd2 = branch_command(first_tip.commit_id.clone(), over_tip.commit_id.clone());
        let err = chain_for(&over_tip, &first_tip.commit_id)
            .validate(&cmd2, &storage.mono_storage(), Some(&cl))
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
                commit_row(704, &sha(704), std::slice::from_ref(&base)),
            ],
        )
        .await;
        let tip = commit_row(704, &sha(704), std::slice::from_ref(&base));
        let cmd = branch_command(base.clone(), tip.commit_id.clone());
        chain_for(&tip, &base)
            .validate(&cmd, &storage.mono_storage(), Some(&cl))
            .await
            .expect("a merge commit in CL history must not reject a clean increment");

        // Over limit: 249-commit history (one member a merge) plus a legal
        // 2-commit increment → cumulative 251 → rejected on the cumulative
        // branch, not on the historical merge.
        let from = sha(3000);
        let base = sha(3000 + 249);
        let cl = open_cl(&from, &base);
        let mut rows: Vec<_> = (1..=249u64)
            .map(|i| {
                let parents = if i == 100 {
                    vec![sha(3000 + i - 1), sha(3999)]
                } else {
                    vec![sha(3000 + i - 1)]
                };
                commit_row((3000 + i) as i64, &sha(3000 + i), &parents)
            })
            .collect();
        rows.push(commit_row(3250, &sha(3250), std::slice::from_ref(&base)));
        rows.push(commit_row(3251, &sha(3251), &[sha(3250)]));
        insert_commits(&storage, rows).await;
        let tip = commit_row(3251, &sha(3251), &[sha(3250)]);
        let cmd = branch_command(base.clone(), tip.commit_id.clone());
        let err = chain_for(&tip, &base)
            .validate(&cmd, &storage.mono_storage(), Some(&cl))
            .await
            .expect_err("an overlong CL history must be rejected by the cumulative count");
        let msg = err.to_string();
        assert!(msg.contains("cumulative"), "{msg}");
        assert!(
            msg.contains("merge the current CL first or open a new CL"),
            "{msg}"
        );
    }
}
