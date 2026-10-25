//! Grants publication eligibility only after every requested commit preserves
//! the checkpoint bindings visible at its predecessor.

use super::{
    admission::IngestContinuation,
    publication::{CheckpointMap, PreparedPublication},
};
use crate::{
    common::errors::{MegaError, TracesError},
    jupiter::storage::trace_stream_storage::{ExpectedOldTip, PublishRef},
};

pub(crate) struct CheckpointValidatedPublication {
    facts: PreparedPublication,
}

impl CheckpointValidatedPublication {
    pub(crate) fn update(&self) -> &PublishRef {
        self.facts.update()
    }

    pub(crate) fn final_checkpoints(&self) -> Result<&CheckpointMap, MegaError> {
        self.facts.final_checkpoints()
    }

    pub(crate) fn control(&self) -> &IngestContinuation {
        self.facts.control()
    }

    pub(crate) async fn incoming_bytes(&self, oid: &str) -> Result<Option<Vec<u8>>, MegaError> {
        self.facts.incoming_bytes(oid).await
    }
}

fn invalid() -> MegaError {
    MegaError::Traces(TracesError::ObjectIntegrity)
}

#[cfg(test)]
thread_local! {
    static CHECKED_BINDINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) async fn validate_checkpoint_history(
    facts: PreparedPublication,
) -> Result<CheckpointValidatedPublication, MegaError> {
    facts.control().checkpoint().await?;
    let mut previous = facts.baseline_checkpoints()?;
    match (&facts.update().expected_old_tip, previous) {
        (ExpectedOldTip::Absent, None) | (ExpectedOldTip::At(_), Some(_)) => {}
        _ => return Err(invalid()),
    }
    for commit in facts.commits() {
        facts.control().checkpoint().await?;
        let current = facts.checkpoints(commit.tree()).ok_or_else(invalid)?;
        if let Some(previous) = previous
            && !std::ptr::eq(previous, current)
        {
            for binding in previous.iter()? {
                facts.control().checkpoint().await?;
                let binding = binding?;
                if current.get(&binding.checkpoint_key)?.as_ref() != Some(&binding) {
                    return Err(invalid());
                }
                #[cfg(test)]
                CHECKED_BINDINGS.with(|count| count.set(count.get() + 1));
            }
        }
        previous = Some(current);
    }
    facts.control().check()?;
    Ok(CheckpointValidatedPublication { facts })
}

#[cfg(test)]
mod tests {
    use git_internal::hash::HashKind;
    use sea_orm::DatabaseConnection;

    use super::*;
    use crate::{
        ceres::traces::{
            admission::{IngestAdmission, decode_wire_pack},
            publication::{
                self, PublicationProposal,
                tests::{Fixture, local_store},
            },
        },
        jupiter::{
            migration::apply_migrations, service::trace_object_service::TraceObjectService,
            storage::object_storage::MegaObjectStorageWrapper, tests::test_db_connection,
        },
    };

    async fn database() -> (tempfile::TempDir, DatabaseConnection) {
        let directory = tempfile::tempdir().unwrap();
        let db = test_db_connection(directory.path()).await;
        apply_migrations(&db, true).await.unwrap();
        (directory, db)
    }

    fn proposal(tip: &str, old: ExpectedOldTip) -> PublicationProposal {
        PublicationProposal {
            stream_id: uuid::Uuid::new_v4().to_string(),
            expected_old_tip: old,
            new_tip: tip.to_owned(),
            ref_names: vec!["refs/libra/traces".into()],
            delete: false,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
        }
    }

    async fn prepare(
        fixture: &Fixture,
        db: &DatabaseConnection,
        store: &MegaObjectStorageWrapper,
        tip: &str,
        old: ExpectedOldTip,
    ) -> Result<PreparedPublication, MegaError> {
        publication::prepare(
            db,
            &TraceObjectService::new(store.clone()),
            fixture.decoded(None).await,
            proposal(tip, old),
        )
        .await
    }

    fn checkpoint_root(fixture: &mut Fixture, leaves: &[(&str, &str)]) -> String {
        let entries: Vec<_> = leaves
            .iter()
            .map(|(name, oid)| ("40000", name.as_bytes(), *oid))
            .collect();
        let prefix = fixture.tree(&entries);
        let checkpoints = fixture.tree(&[("40000", b"ab", &prefix)]);
        fixture.tree(&[("40000", b"checkpoint", &checkpoints)])
    }

    #[tokio::test]
    async fn checkpoint_history_chain_accepts_incremental_raw_order_both_hashes() {
        let (_directory, db) = database().await;
        let (_backend, store) = local_store().await;
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut fixture = Fixture::new(kind);
            let (first_tree, first_leaf) = fixture.capture(br#"{"schema_version":2}"#);
            let first = fixture.commit(&first_tree, None);
            let (_, second_leaf) = fixture.capture(b"opaque unknown schema\x00\xff");
            let next_tree =
                checkpoint_root(&mut fixture, &[("cd", &first_leaf), ("ef", &second_leaf)]);
            let second = fixture.commit(&next_tree, Some(&first));
            let third = fixture.commit(&next_tree, Some(&second));
            let orphan_tree = fixture.tree(&[]);
            let orphan = fixture.commit(&orphan_tree, Some(&first));
            fixture.objects.reverse();
            let facts = prepare(
                &fixture,
                &db,
                &store,
                &third,
                ExpectedOldTip::At(first.clone()),
            )
            .await
            .unwrap();
            assert_eq!(
                facts.commits().iter().map(|c| c.oid()).collect::<Vec<_>>(),
                [second.as_str(), third.as_str()]
            );
            assert_eq!(facts.commits()[0].parents(), [first.as_str()]);
            assert!(!facts.update().object_oids.contains(&orphan));
            assert!(std::ptr::eq(
                facts.checkpoints(facts.commits()[0].tree()).unwrap(),
                facts.checkpoints(facts.commits()[1].tree()).unwrap()
            ));
            let result = validate_checkpoint_history(facts).await.unwrap();
            assert_eq!(result.update().new_tip, third);
            assert_eq!(
                result
                    .final_checkpoints()
                    .unwrap()
                    .get("abcd")
                    .unwrap()
                    .unwrap()
                    .original_tree,
                first_leaf
            );
            assert_eq!(
                result
                    .final_checkpoints()
                    .unwrap()
                    .get("abef")
                    .unwrap()
                    .unwrap()
                    .original_tree,
                second_leaf
            );
            assert_eq!(result.update().schema_id, "unsupported");
            validate_checkpoint_history(
                prepare(&fixture, &db, &store, &third, ExpectedOldTip::Absent)
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        }
    }

    async fn rejects_mutation(existing: bool, deletion: bool) {
        let (_directory, db) = database().await;
        let (_backend, store) = local_store().await;
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut fixture = Fixture::new(kind);
            let empty = fixture.tree(&[]);
            let root = fixture.commit(&empty, None);
            let (tree, _) = fixture.capture(br#"{"schema_version":2}"#);
            let introduced = fixture.commit(&tree, Some(&root));
            let changed_tree = if deletion {
                empty
            } else {
                fixture
                    .capture(br#"{"schema_version":2,"mutation":true}"#)
                    .0
            };
            let changed = fixture.commit(&changed_tree, Some(&introduced));
            let restored = fixture.commit(&tree, Some(&changed));
            let old = if existing {
                introduced.clone()
            } else {
                root.clone()
            };
            for baseline in [ExpectedOldTip::At(old), ExpectedOldTip::Absent] {
                let commits = if matches!(baseline, ExpectedOldTip::Absent) {
                    4
                } else if existing {
                    2
                } else {
                    3
                };
                let facts = prepare(&fixture, &db, &store, &restored, baseline)
                    .await
                    .unwrap();
                assert_eq!(facts.commits().len(), commits);
                assert_eq!(
                    facts
                        .final_checkpoints()
                        .unwrap()
                        .get("abcd")
                        .unwrap()
                        .unwrap(),
                    facts
                        .checkpoints(&tree)
                        .unwrap()
                        .get("abcd")
                        .unwrap()
                        .unwrap()
                );
                let spool = facts.spool_path_for_test().to_owned();
                assert!(matches!(
                    validate_checkpoint_history(facts).await,
                    Err(MegaError::Traces(TracesError::ObjectIntegrity))
                ));
                assert!(!spool.exists());
            }
        }
    }

    #[tokio::test]
    async fn checkpoint_history_chain_rejects_existing_mutation_then_restore() {
        rejects_mutation(true, false).await;
    }

    #[tokio::test]
    async fn checkpoint_history_chain_rejects_existing_deletion_then_restore() {
        rejects_mutation(true, true).await;
    }

    #[tokio::test]
    async fn checkpoint_history_chain_rejects_new_binding_mutation_then_restore() {
        rejects_mutation(false, false).await;
    }

    #[tokio::test]
    async fn checkpoint_history_chain_rejects_new_binding_deletion_then_restore() {
        rejects_mutation(false, true).await;
    }

    #[tokio::test]
    async fn checkpoint_history_chain_rejects_unbound_and_wrong_baseline() {
        let (_directory, db) = database().await;
        let (_backend, store) = local_store().await;
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut fixture = Fixture::new(kind);
            let tree = fixture.tree(&[]);
            let tip = fixture.commit(&tree, None);
            let (other_tree, _) = fixture.capture(b"other branch");
            let other = fixture.commit(&other_tree, None);
            assert!(matches!(
                prepare(&fixture, &db, &store, &tip, ExpectedOldTip::At(other)).await,
                Err(MegaError::Traces(TracesError::ObjectIntegrity))
            ));
            let facts = prepare(&fixture, &db, &store, &tip, ExpectedOldTip::Unspecified)
                .await
                .unwrap();
            assert!(matches!(
                validate_checkpoint_history(facts).await,
                Err(MegaError::Traces(TracesError::ObjectIntegrity))
            ));
        }
    }

    #[tokio::test]
    async fn checkpoint_history_resources_paged_maps_cancel_during_validation_and_drop() {
        let (_directory, db) = database().await;
        let (_backend, store) = local_store().await;
        let mut fixture = Fixture::new(HashKind::Sha256);
        let (_, leaf) = fixture.capture(br#"{"schema_version":2}"#);
        let names: Vec<_> = (0..1025).map(|n| format!("{n:08x}")).collect();
        let leaves: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), leaf.as_str()))
            .collect();
        let tree = checkpoint_root(&mut fixture, &leaves);
        let first = fixture.commit(&tree, None);
        let mut additional = leaves.clone();
        additional.push(("ffffffff", &leaf));
        let next_tree = checkpoint_root(&mut fixture, &additional);
        let next = fixture.commit(&next_tree, Some(&first));
        let pack = fixture.pack(None);
        let mut admission = IngestAdmission::start();
        let decoded = decode_wire_pack(&mut admission, HashKind::Sha256, pack.as_slice(), None)
            .await
            .unwrap();
        let facts = publication::prepare(
            &db,
            &TraceObjectService::new(store.clone()),
            decoded,
            proposal(&next, ExpectedOldTip::At(first.clone())),
        )
        .await
        .unwrap();
        let spool = facts.spool_path_for_test().to_owned();
        assert!(
            std::fs::metadata(spool.join(format!("map-{tree}")))
                .unwrap()
                .len()
                > 64 * 1024
        );
        assert_eq!(
            facts
                .baseline_checkpoints()
                .unwrap()
                .unwrap()
                .iter()
                .unwrap()
                .count(),
            1025
        );
        CHECKED_BINDINGS.with(|count| count.set(0));
        let mut future = Box::pin(validate_checkpoint_history(facts));
        for _ in 0..300 {
            assert!(futures::poll!(&mut future).is_pending());
        }
        assert!(CHECKED_BINDINGS.with(|count| count.get()) > 0);
        admission.cancel();
        assert!(matches!(
            future.await,
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
        assert!(!spool.exists());

        let facts = prepare(
            &fixture,
            &db,
            &store,
            &next,
            ExpectedOldTip::At(first.clone()),
        )
        .await
        .unwrap();
        let spool = facts.spool_path_for_test().to_owned();
        let mut future = Box::pin(validate_checkpoint_history(facts));
        assert!(futures::poll!(&mut future).is_pending());
        drop(future);
        assert!(!spool.exists());
        let accepted = validate_checkpoint_history(
            prepare(&fixture, &db, &store, &next, ExpectedOldTip::At(first))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            accepted
                .final_checkpoints()
                .unwrap()
                .iter()
                .unwrap()
                .count(),
            1026
        );
    }
}
