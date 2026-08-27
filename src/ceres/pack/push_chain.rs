//! Push-chain state model for the MonoRepo receive-pack path.
//!
//! Push semantics (base/tip/attribution) derive solely from
//! `RefCommand.old_id/new_id` plus the unpacked tip commit's parent chain —
//! never from pack arrival order (GC-MC-13, plan-20260827).

use git_internal::internal::object::commit::Commit;

use crate::{
    callisto::sea_orm_active_enums::RefTypeEnum,
    ceres::protocol::import_refs::{CommandType, RefCommand},
    common::{errors::MegaError, utils::ZERO_ID},
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
    /// multi-commit packs, so this holds exactly one commit; MC-03 turns
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

    use super::{PushChain, PushChainResolution, attribution_commit_id, primary_branch_command};
    use crate::{ceres::protocol::import_refs::RefCommand, common::utils::ZERO_ID};

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
}
