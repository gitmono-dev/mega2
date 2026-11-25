use git_internal::internal::object::commit::Commit;

use crate::{
    callisto::{entity_ext::generate_link, mega_cl, mega_refs, sea_orm_active_enums::ConvTypeEnum},
    ceres::{
        api_service::ApiHandler,
        merge_checker::{CheckerRegistry, MAX_CL_CHAIN_COMMITS},
    },
    common::errors::MegaError,
    config::PushPolicy,
    jupiter::{
        service::webhook_service::WebhookEvent,
        storage::{Storage, mono_storage::MonoStorage},
        utils::converter::FromMegaModel,
    },
};

pub(crate) fn reject_trunk_cl_write(storage: &Storage) -> Result<(), MegaError> {
    if storage.config().monorepo.push_policy == PushPolicy::Trunk {
        return Err(MegaError::Other(
            "push_policy=trunk: CL writes are closed; land changes with git push".into(),
        ));
    }
    Ok(())
}

/// MC-04: rebuild a CL's complete commit chain for its current
/// `(from_hash, to_hash)` — a first-parent walk from `to_hash` down to
/// `from_hash` (the frozen baseline, not itself a member), tip first.
///
/// The listing must never be the push's increment: an update push keeps the
/// CL's frozen `from_hash` and only advances `to_hash`, so the chain is
/// always rebuilt over the CL's full cumulative range. Bounded by
/// `MAX_CL_CHAIN_COMMITS` (the receive side guarantees the cumulative range
/// fits, ADR-MC-07; this is the fail-closed backstop) and fail-closed on a
/// missing commit row, a parent cycle, or a walk that roots out before
/// reaching `from_hash`.
pub(crate) async fn collect_cl_chain(
    storage: &Storage,
    from_hash: &str,
    to_hash: &str,
) -> Result<Vec<Commit>, MegaError> {
    let mono_storage = storage.mono_storage();
    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut current = to_hash.to_string();
    while current != from_hash {
        if !visited.insert(current.clone()) {
            return Err(MegaError::Other(format!(
                "CL chain has a parent cycle at commit {current}; cannot rebuild the listing"
            )));
        }
        if chain.len() >= MAX_CL_CHAIN_COMMITS {
            return Err(MegaError::Other(format!(
                "CL chain exceeds the {MAX_CL_CHAIN_COMMITS}-commit limit \
                 ({from_hash}..{to_hash}); cannot rebuild the listing"
            )));
        }
        let model = mono_storage
            .get_commit_by_hash(&current)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!(
                    "CL chain commit {current} is missing from storage; cannot rebuild the \
                     listing (fail-closed)"
                ))
            })?;
        let commit = Commit::from_mega_model(model);
        current = match commit.parent_commit_ids.first() {
            Some(parent) => parent.to_string(),
            None => {
                return Err(MegaError::Other(format!(
                    "CL chain rooted out at commit {current} before reaching the frozen base \
                     {from_hash}; cannot rebuild the listing (fail-closed)"
                )));
            }
        };
        chain.push(commit);
    }
    Ok(chain)
}

pub(crate) trait ConversationMessageFormater {
    fn format(
        &self,
        cl: &mega_cl::Model,
        from_hash: &str,
        to_hash: &str,
        username: &str,
    ) -> String {
        let old_hash = &cl.to_hash[..6];
        let new_hash = &to_hash[..6];
        if cl.from_hash == from_hash {
            format!(
                "{} updated the change_list automatic from {} to {}",
                username, old_hash, new_hash
            )
        } else {
            format!(
                "{} detected upstream changes (base {} → {}). Use Update Branch to sync.",
                username, old_hash, new_hash
            )
        }
    }
}

pub(crate) trait CLRefUpdateVisitor {
    async fn visit(
        &self,
        cl: &mega_cl::Model,
        commit_hash: &str,
        tree_hash: &str,
        txn: &sea_orm::DatabaseTransaction,
    ) -> Result<mega_refs::Model, MegaError>;
}

pub(crate) trait CLRefUpdateAcceptor<VT: CLRefUpdateVisitor> {
    async fn accept(
        &self,
        visitor: &VT,
        cl: &mega_cl::Model,
        commit_hash: &str,
        tree_hash: &str,
        txn: &sea_orm::DatabaseTransaction,
    ) -> Result<(), MegaError> {
        visitor.visit(cl, commit_hash, tree_hash, txn).await?;
        Ok(())
    }
}

pub(crate) trait Checker {
    async fn check(
        &self,
        storage: Storage,
        username: &str,
        cl: &mega_cl::Model,
    ) -> Result<(), MegaError> {
        let check_reg = CheckerRegistry::new(storage.into(), username.to_string());
        check_reg.run_checks(cl.clone().into()).await?;
        Ok(())
    }
}

pub(crate) trait Director<T: ApiHandler + Clone> {
    async fn get_api_handler(&self) -> T;
}

fn cl_with_latest_to_hash(mut cl: mega_cl::Model, to_hash: &str) -> mega_cl::Model {
    cl.to_hash = to_hash.to_string();
    cl
}

fn fresh_or_fallback_cl(
    original: mega_cl::Model,
    fresh: Option<mega_cl::Model>,
    to_hash: &str,
) -> mega_cl::Model {
    match fresh {
        Some(cl) => cl,
        None => cl_with_latest_to_hash(original, to_hash),
    }
}

pub(crate) struct CodeEditService<FMT, VT, AC, CK, HD, DR>
where
    FMT: ConversationMessageFormater,
    VT: CLRefUpdateVisitor,
    AC: CLRefUpdateAcceptor<VT>,
    CK: Checker,
    HD: ApiHandler + Clone,
    DR: Director<HD>,
{
    pub repo_path: String,
    pub base_branch: String,
    pub from_hash: String,
    formator: FMT,
    clref_visitor: VT,
    clref_acceptor: AC,
    checker: CK,
    director: DR,
    // mark HD used
    _marker: std::marker::PhantomData<HD>,
}

pub struct DefaultVisitor<'a> {
    mono_storage: &'a MonoStorage,
    ref_name: &'a str,
}

impl CLRefUpdateVisitor for DefaultVisitor<'_> {
    async fn visit(
        &self,
        _: &mega_cl::Model,
        _: &str,
        _: &str,
        _: &sea_orm::DatabaseTransaction,
    ) -> Result<mega_refs::Model, MegaError> {
        let _ = self.ref_name;
        let _ = self.mono_storage;
        panic!("visitor not implemented!");
    }
}

pub struct DefualtDirector<T: ApiHandler + Clone> {
    pub handler: T,
}

impl<T: ApiHandler + Clone> crate::ceres::code_edit::model::Director<T> for DefualtDirector<T> {
    async fn get_api_handler(&self) -> T {
        self.handler.clone()
    }
}

impl<
    FMT: ConversationMessageFormater,
    VT: CLRefUpdateVisitor,
    AC: CLRefUpdateAcceptor<VT>,
    CK: Checker,
    HD: ApiHandler + Clone,
    DR: Director<HD>,
> CodeEditService<FMT, VT, AC, CK, HD, DR>
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo_path: &str,
        base_branch: &str,
        from_hash: &str,
        formator: FMT,
        clref_visitor: VT,
        clref_acceptor: AC,
        checker: CK,
        director: DR,
    ) -> Self {
        Self {
            repo_path: repo_path.to_string(),
            base_branch: base_branch.to_string(),
            from_hash: from_hash.to_string(),
            formator,
            clref_visitor,
            clref_acceptor,
            checker,
            director,
            _marker: std::marker::PhantomData,
        }
    }

    pub async fn update_existing_cl(
        &self,
        cl: mega_cl::Model,
        storage: &Storage,
        from_hash: &str,
        to_hash: &str,
        username: &str,
    ) -> Result<(), MegaError> {
        reject_trunk_cl_write(storage)?;
        let cl_stg = storage.cl_storage();
        let comment_stg = storage.conversation_storage();

        let to_same = cl.to_hash == to_hash;
        if to_same {
            tracing::info!("repeat commit with change_list: {}, do nothing", cl.id);
        } else {
            // MC-04: the to_hash advance, commit-listing rebuild, CL ref
            // upsert, CI check invalidation, and transactional CL webhook
            // outbox enqueue share one DB transaction — neither takes effect
            // without the other (CLB-15). The listing is the complete chain
            // over the frozen `(from_hash, to_hash]`, never the push's increment.
            let dst_commit = Commit::from_mega_model(
                storage
                    .mono_storage()
                    .get_commit_by_hash(to_hash)
                    .await?
                    .ok_or_else(|| MegaError::Other(format!("invalid to_hash: {to_hash}")))?,
            );
            let chain = collect_cl_chain(storage, &cl.from_hash, to_hash).await?;
            let txn = storage.begin_db_transaction().await?;
            cl_stg
                .update_cl_to_hash_in_txn(cl.clone(), to_hash, &txn)
                .await?;
            cl_stg
                .save_cl_commits_in_txn(&cl.link, &chain, &txn)
                .await?;
            let updated_cl = cl_stg.get_cl_in_txn(&cl.link, &txn).await?.ok_or_else(|| {
                MegaError::Other(format!("CL {} disappeared after hash update", cl.link))
            })?;
            self.clref_acceptor
                .accept(
                    &self.clref_visitor,
                    &updated_cl,
                    to_hash,
                    &dst_commit.tree_id.to_string(),
                    &txn,
                )
                .await?;
            cl_stg
                .invalidate_ci_check_in_txn(&updated_cl.link, &txn)
                .await?;
            cl_stg
                .enqueue_cl_webhook_outbox_in_txn(WebhookEvent::ClUpdated, &updated_cl, &txn)
                .await?;
            txn.commit().await.map_err(MegaError::Db)?;
            // Freeze cl base for Open cl: do NOT auto-update from_hash here.
            // Only update to_hash to reflect latest edits, and prompt user to run Update Branch.
            // The conversation entry narrates the committed advance, so it
            // is written only after the transaction commits.
            comment_stg
                .add_conversation(
                    &cl.link,
                    username,
                    Some(self.formator.format(&cl, from_hash, to_hash, username)),
                    ConvTypeEnum::Comment,
                )
                .await?;
        }
        Ok(())
    }

    pub async fn create_new_cl(
        &self,
        storage: &Storage,
        repo_path: &str,
        from_hash: &str,
        to_hash: &str,
        username: &str,
    ) -> Result<mega_cl::Model, MegaError> {
        reject_trunk_cl_write(storage)?;
        let dst_commit = Commit::from_mega_model(
            storage
                .mono_storage()
                .get_commit_by_hash(to_hash)
                .await?
                .expect("invalid to_hash"),
        );
        let cl_stg = storage.cl_storage();
        // MC-04 + CLB-15: CL creation (head_revision = 1), commit-listing
        // rebuild, CL ref persistence, and cl.created outbox enqueue share one
        // DB transaction so an outbox event can never be committed without its
        // CL ref.
        let chain = collect_cl_chain(storage, from_hash, to_hash).await?;
        let txn = storage.begin_db_transaction().await?;
        let cl_link = generate_link();
        let cl = cl_stg
            .new_cl_model_in_txn(
                repo_path,
                &cl_link,
                &dst_commit.format_message(),
                &self.base_branch,
                from_hash,
                to_hash,
                username,
                &txn,
            )
            .await?;
        cl_stg
            .save_cl_commits_in_txn(&cl_link, &chain, &txn)
            .await?;
        self.clref_acceptor
            .accept(
                &self.clref_visitor,
                &cl,
                to_hash,
                &dst_commit.tree_id.to_string(),
                &txn,
            )
            .await?;
        cl_stg
            .enqueue_cl_webhook_outbox_in_txn(WebhookEvent::ClCreated, &cl, &txn)
            .await?;
        txn.commit().await.map_err(MegaError::Db)?;

        storage
            .conversation_storage()
            .add_conversation(
                &cl.link,
                username,
                Some(self.formator.format(&cl, from_hash, to_hash, username)),
                ConvTypeEnum::Comment,
            )
            .await?;
        Ok(cl)
    }

    pub async fn update_or_create_cl(
        &self,
        storage: &Storage,
        from_hash: &str,
        to_hash: &str,
        username: &str,
    ) -> Result<mega_cl::Model, MegaError> {
        reject_trunk_cl_write(storage)?;
        let path_str = &self.repo_path;
        match storage
            .cl_storage()
            .get_open_cl_by_path(path_str, username)
            .await?
        {
            Some(cl) => {
                self.update_existing_cl(cl.clone(), storage, &cl.from_hash, to_hash, username)
                    .await?;
                let fresh = storage.cl_storage().get_cl(&cl.link).await?;
                if fresh.is_none() {
                    tracing::warn!(
                        cl_link = %cl.link,
                        "CL was updated but fresh model lookup returned None; fallback to in-memory to_hash update."
                    );
                }
                Ok(fresh_or_fallback_cl(cl, fresh, to_hash))
            }
            None => Ok(self
                .create_new_cl(storage, path_str, from_hash, to_hash, username)
                .await?),
        }
    }

    pub async fn trigger_check(
        &self,
        storage: Storage,
        username: &str,
        cl: &mega_cl::Model,
    ) -> Result<(), MegaError> {
        self.checker.check(storage, username, cl).await
    }
}

#[cfg(test)]
mod tests {
    use super::{cl_with_latest_to_hash, fresh_or_fallback_cl};
    use crate::callisto::sea_orm_active_enums::MergeStatusEnum;

    fn sample_cl(to_hash: &str) -> crate::callisto::mega_cl::Model {
        crate::callisto::mega_cl::Model {
            id: 1,
            link: "C1234567".to_string(),
            title: "test".to_string(),
            merge_date: None,
            status: MergeStatusEnum::Open,
            path: "/project/buck2_test".to_string(),
            from_hash: "1111111111111111111111111111111111111111".to_string(),
            to_hash: to_hash.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            username: "tester".to_string(),
            base_branch: "main".to_string(),
            revision: 0,
            head_revision: 1,
        }
    }

    #[test]
    fn test_fresh_or_fallback_cl_prefers_fresh_model() {
        let original = sample_cl("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let fresh = sample_cl("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let selected = fresh_or_fallback_cl(original, Some(fresh.clone()), "cccc");

        assert_eq!(selected.to_hash, fresh.to_hash);
    }

    #[test]
    fn test_fresh_or_fallback_cl_updates_to_hash_when_fresh_missing() {
        let original = sample_cl("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let selected =
            fresh_or_fallback_cl(original, None, "cccccccccccccccccccccccccccccccccccccccc");

        assert_eq!(selected.to_hash, "cccccccccccccccccccccccccccccccccccccccc");
    }

    #[test]
    fn test_cl_with_latest_to_hash_updates_only_to_hash_field() {
        let original = sample_cl("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let updated =
            cl_with_latest_to_hash(original.clone(), "dddddddddddddddddddddddddddddddddddddddd");

        assert_eq!(updated.to_hash, "dddddddddddddddddddddddddddddddddddddddd");
        assert_eq!(updated.link, original.link);
        assert_eq!(updated.path, original.path);
    }

    // --- MC-04: CL row write and commit-listing rebuild in one transaction ---

    use std::sync::Arc;

    use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
    use tempfile::TempDir;

    use crate::{
        callisto::{mega_commit, mega_tree},
        ceres::{
            api_service::{cache::GitObjectCache, mono_api_service::MonoApiService},
            code_edit::on_push::OnpushCodeEdit,
        },
        jupiter::{
            storage::{Storage, base_storage::StorageConnector},
            tests::test_storage,
        },
    };

    /// The well-known git empty-tree id — the seeded commits all share it, so
    /// the reviewer/diff machinery sees an empty change set.
    const MC04_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn mc04_sha(n: u64) -> String {
        format!("{n:040x}")
    }

    fn mc04_commit_row(n: u64, parents: &[u64]) -> mega_commit::Model {
        mega_commit::Model {
            id: crate::callisto::entity_ext::generate_id(),
            commit_id: mc04_sha(n),
            tree: MC04_TREE.to_string(),
            parents_id: serde_json::json!(parents.iter().map(|p| mc04_sha(*p)).collect::<Vec<_>>()),
            author: Some("author Test User <mc04@example.invalid> 1750000000 +0000".to_string()),
            committer: Some(
                "committer Test User <mc04@example.invalid> 1750000000 +0000".to_string(),
            ),
            content: Some(format!("mc04 message {}", n)),
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    async fn mc04_seed(storage: &Storage, rows: Vec<mega_commit::Model>) {
        let mono_storage = storage.mono_storage();
        let conn = mono_storage.get_connection();
        // The shared empty tree is seeded once; the update phase must not
        // duplicate it.
        if mega_tree::Entity::find()
            .filter(mega_tree::Column::TreeId.eq(MC04_TREE))
            .one(conn)
            .await
            .expect("probe empty tree")
            .is_none()
        {
            mega_tree::Entity::insert(
                mega_tree::Model {
                    id: crate::callisto::entity_ext::generate_id(),
                    tree_id: MC04_TREE.to_string(),
                    sub_trees: Vec::new(),
                    size: 0,
                    created_at: chrono::Utc::now().naive_utc(),
                    pack_id: String::new(),
                    pack_offset: 0,
                    commit_id: String::new(),
                }
                .into_active_model(),
            )
            .exec(conn)
            .await
            .expect("insert empty tree");
        }
        mega_commit::Entity::insert_many(
            rows.into_iter()
                .map(|m| m.into_active_model())
                .collect::<Vec<_>>(),
        )
        .exec(conn)
        .await
        .expect("insert commits");
    }

    fn mc04_editor(storage: &Storage) -> OnpushCodeEdit {
        // Never connected: the listing path under test does not touch the cache.
        let connection = ::redis::aio::ConnectionManager::new_lazy_with_config(
            ::redis::Client::open("redis://127.0.0.1:6379").expect("redis client"),
            ::redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy connection manager");
        let api = MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection,
                prefix: "mc04-test".to_string(),
            }),
        };
        OnpushCodeEdit::from("/", "main", &mc04_sha(0), &api, storage.mono_storage())
    }

    #[tokio::test]
    async fn update_or_create_cl_writes_cl_and_listing_atomically() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        mc04_seed(
            &storage,
            vec![
                mc04_commit_row(500, &[]),
                mc04_commit_row(501, &[500]),
                mc04_commit_row(502, &[501]),
            ],
        )
        .await;
        let editor = mc04_editor(&storage);

        let cl = editor
            .update_or_create_cl(&storage, &mc04_sha(500), &mc04_sha(502), "tester")
            .await
            .expect("create CL with listing");
        assert_eq!(cl.from_hash, mc04_sha(500));
        assert_eq!(cl.to_hash, mc04_sha(502));
        let listing = storage
            .cl_storage()
            .get_cl_commits(&cl.link)
            .await
            .expect("read listing");
        let shas: Vec<String> = listing.iter().map(|r| r.commit_sha.clone()).collect();
        assert_eq!(
            shas,
            vec![mc04_sha(501), mc04_sha(502)],
            "the create path lists the full chain, oldest first"
        );
        assert_eq!(listing[0].author_email, "mc04@example.invalid");

        // Update path: advance to 503 with the frozen from_hash — the listing
        // must rebuild over the whole cumulative chain, not the increment.
        mc04_seed(&storage, vec![mc04_commit_row(503, &[502])]).await;
        let cl2 = editor
            .update_or_create_cl(&storage, &mc04_sha(500), &mc04_sha(503), "tester")
            .await
            .expect("update CL");
        assert_eq!(cl2.link, cl.link, "the update reuses the open CL");
        assert_eq!(cl2.from_hash, mc04_sha(500), "from_hash stays frozen");
        assert_eq!(cl2.to_hash, mc04_sha(503));
        let listing2 = storage
            .cl_storage()
            .get_cl_commits(&cl.link)
            .await
            .expect("read listing after update");
        let shas2: Vec<String> = listing2.iter().map(|r| r.commit_sha.clone()).collect();
        assert_eq!(
            shas2,
            vec![mc04_sha(501), mc04_sha(502), mc04_sha(503)],
            "the update rebuilds the complete chain from the frozen from_hash"
        );
    }

    // collect_cl_chain fail-closed: a chain member missing from storage errors
    // instead of producing a truncated listing.
    #[tokio::test]
    async fn collect_cl_chain_missing_member_fails_closed() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        // sha(601) — the middle member — is deliberately not seeded.
        mc04_seed(&storage, vec![mc04_commit_row(602, &[601])]).await;

        let err = super::collect_cl_chain(&storage, &mc04_sha(600), &mc04_sha(602))
            .await
            .expect_err("a walk that never reaches the base must fail closed");
        assert!(err.to_string().contains("fail-closed"), "{err}");
        assert!(err.to_string().contains(&mc04_sha(601)), "{err}");
    }

    // MC-04 R3: a CL left at an empty range (from == to) by a no-change rebase
    // recovers normally on the next push — the update path advances to the new
    // tip and the listing is rebuilt to the new chain.
    #[tokio::test]
    async fn update_or_create_cl_recovers_after_empty_range_rebase() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        mc04_seed(
            &storage,
            vec![mc04_commit_row(700, &[]), mc04_commit_row(701, &[700])],
        )
        .await;
        let editor = mc04_editor(&storage);

        // Seed the empty-range state directly (from == to), as the no-change
        // rebase leaves it.
        storage
            .cl_storage()
            .new_cl_model(
                "/",
                "CLMC04ER",
                "empty range",
                "main",
                &mc04_sha(700),
                &mc04_sha(700),
                "tester",
            )
            .await
            .expect("seed empty-range CL");

        let cl = editor
            .update_or_create_cl(&storage, &mc04_sha(700), &mc04_sha(701), "tester")
            .await
            .expect("push onto an empty-range CL");
        assert_eq!(cl.from_hash, mc04_sha(700), "from stays frozen");
        assert_eq!(cl.to_hash, mc04_sha(701), "to advances to the new tip");
        let listing = storage
            .cl_storage()
            .get_cl_commits(&cl.link)
            .await
            .expect("listing readable after recovery push");
        let shas: Vec<String> = listing.iter().map(|r| r.commit_sha.clone()).collect();
        assert_eq!(shas, vec![mc04_sha(701)], "the listing is the new chain");
    }

    #[tokio::test]
    async fn update_or_create_cl_fails_closed_under_trunk() {
        let temp = TempDir::new().expect("temp dir");
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        let storage = crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
        let editor = mc04_editor(&storage);
        let err = editor
            .update_or_create_cl(&storage, &mc04_sha(500), &mc04_sha(502), "tester")
            .await
            .expect_err("trunk must not create a CL");
        assert!(err.to_string().contains("push_policy=trunk"), "{err}");
        assert!(
            storage
                .cl_storage()
                .get_open_cl_by_path("/", "tester")
                .await
                .unwrap()
                .is_none(),
            "trunk must not persist a mega_cl row"
        );
    }

    #[tokio::test]
    async fn cl_head_revision_outbox_create_update_and_branch_lifecycle() {
        use sea_orm::{ActiveModelTrait, QueryOrder};

        use crate::callisto::{
            check_result, cl_webhook_outbox, mega_refs, sea_orm_active_enums::CheckTypeEnum,
        };

        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        // Commits: 800 (initial main) -> 801 (CL v1) -> 802 (CL v2), and 810 (advanced main).
        mc04_seed(
            &storage,
            vec![
                mc04_commit_row(800, &[]),
                mc04_commit_row(801, &[800]),
                mc04_commit_row(802, &[801]),
                mc04_commit_row(810, &[800]),
            ],
        )
        .await;
        let cl_stg = storage.cl_storage();
        let conn = cl_stg.get_connection();

        let connection = ::redis::aio::ConnectionManager::new_lazy_with_config(
            ::redis::Client::open("redis://127.0.0.1:6379").expect("redis client"),
            ::redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy connection manager");
        let api = MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection,
                prefix: "clb15-test".to_string(),
            }),
        };
        let editor =
            OnpushCodeEdit::from("/", "main", &mc04_sha(800), &api, storage.mono_storage());

        // 1. create_new_cl -> head_revision = 1 and 1 cl.created outbox row.
        let cl1 = editor
            .update_or_create_cl(&storage, &mc04_sha(800), &mc04_sha(801), "tester")
            .await
            .expect("create CL");
        assert_eq!(cl1.head_revision, 1);

        let rows1 = cl_webhook_outbox::Entity::find()
            .filter(cl_webhook_outbox::Column::ClLink.eq(&cl1.link))
            .order_by_asc(cl_webhook_outbox::Column::Id)
            .all(conn)
            .await
            .expect("query outbox after create");
        assert_eq!(rows1.len(), 1);
        assert_eq!(rows1[0].event_type, "cl.created");
        assert_eq!(rows1[0].head_revision, 1);
        let p1: serde_json::Value = serde_json::from_str(&rows1[0].payload_json).unwrap();
        assert_eq!(p1["cl"]["head_revision"], 1);
        assert_eq!(p1["cl"]["head_commit"], mc04_sha(801));

        // Seed a PASSED CiStatus row at head_revision = 1, ci_generation = 7.
        let now = chrono::Utc::now().naive_utc();
        check_result::Model {
            id: crate::callisto::entity_ext::generate_id(),
            created_at: now,
            updated_at: now,
            path: cl1.path.clone(),
            cl_link: cl1.link.clone(),
            commit_id: cl1.to_hash.clone(),
            check_type_code: CheckTypeEnum::CiStatus,
            status: "PASSED".to_owned(),
            message: "green v1".to_owned(),
            head_revision: 1,
            ci_generation: 7,
            ci_task_id: Some("tsk_v1".to_owned()),
            ci_attempt_id: Some("att_v1".to_owned()),
            details_url: Some("https://ci.example/tasks/tsk_v1".to_owned()),
        }
        .into_active_model()
        .insert(conn)
        .await
        .expect("seed CiStatus v1");

        // 2a. Repeat push with unchanged to_hash -> no-op (no head_revision bump, no invalidation, no outbox).
        let cl_noop = editor
            .update_or_create_cl(&storage, &mc04_sha(800), &mc04_sha(801), "tester")
            .await
            .expect("repeat push");
        assert_eq!(cl_noop.head_revision, 1);
        assert_eq!(
            cl_webhook_outbox::Entity::find()
                .filter(cl_webhook_outbox::Column::ClLink.eq(&cl1.link))
                .all(conn)
                .await
                .unwrap()
                .len(),
            1
        );

        // 2b. update_existing_cl with new to_hash -> head_revision = 2, CiStatus = INVALIDATED (ci_generation=7 preserved), cl.updated outbox row.
        let cl2 = editor
            .update_or_create_cl(&storage, &mc04_sha(800), &mc04_sha(802), "tester")
            .await
            .expect("update CL");
        assert_eq!(cl2.head_revision, 2);

        let ci_after_update = check_result::Entity::find()
            .filter(check_result::Column::ClLink.eq(&cl1.link))
            .filter(check_result::Column::CheckTypeCode.eq(CheckTypeEnum::CiStatus))
            .one(conn)
            .await
            .expect("load ci")
            .expect("ci row exists");
        assert_eq!(ci_after_update.status, "INVALIDATED");
        assert_eq!(ci_after_update.ci_generation, 7);
        assert_eq!(ci_after_update.ci_task_id, None);
        assert_eq!(ci_after_update.ci_attempt_id, None);
        assert_eq!(ci_after_update.details_url, None);

        let rows2 = cl_webhook_outbox::Entity::find()
            .filter(cl_webhook_outbox::Column::ClLink.eq(&cl1.link))
            .order_by_asc(cl_webhook_outbox::Column::Id)
            .all(conn)
            .await
            .expect("query outbox after update");
        assert_eq!(rows2.len(), 2);
        assert_eq!(rows2[1].event_type, "cl.updated");
        assert_eq!(rows2[1].head_revision, 2);
        assert_ne!(rows2[0].event_id, rows2[1].event_id);
        let p2: serde_json::Value = serde_json::from_str(&rows2[1].payload_json).unwrap();
        assert_eq!(p2["cl"]["head_revision"], 2);
        assert_eq!(p2["cl"]["head_commit"], mc04_sha(802));

        // 3a. Seed main ref at cl2.from_hash (800) -> update_branch returns "Already up-to-date" with ZERO new outbox rows.
        let main_ref = mega_refs::Model {
            id: crate::callisto::entity_ext::generate_id(),
            path: "/".to_owned(),
            ref_name: "refs/heads/main".to_owned(),
            ref_commit_hash: mc04_sha(800),
            ref_tree_hash: MC04_TREE.to_owned(),
            created_at: now,
            updated_at: now,
            is_cl: false,
        };
        storage
            .mono_storage()
            .save_refs(main_ref.clone(), None)
            .await
            .expect("save main ref");
        // OnpushCodeEdit already persisted and advanced refs/cl/<link> atomically.
        let cl_ref = storage
            .mono_storage()
            .get_ref_by_name(&format!("refs/cl/{}", cl1.link))
            .await
            .expect("query cl ref")
            .expect("refs/cl/<link> must already exist from OnpushCodeEdit");
        assert_eq!(cl_ref.ref_commit_hash, mc04_sha(802));

        let up_to_date = api
            .update_branch("tester", &cl1.link)
            .await
            .expect("update_branch up to date");
        assert_eq!(up_to_date, "Already up-to-date");
        assert_eq!(
            storage
                .cl_storage()
                .get_cl(&cl1.link)
                .await
                .unwrap()
                .unwrap()
                .head_revision,
            2
        );
        assert_eq!(
            cl_webhook_outbox::Entity::find()
                .filter(cl_webhook_outbox::Column::ClLink.eq(&cl1.link))
                .all(conn)
                .await
                .unwrap()
                .len(),
            2,
            "Already up-to-date must enqueue zero outbox rows"
        );

        // 3b. Mark CiStatus PASSED at head_revision = 2, ci_generation = 11, then advance main ref to 810 and call update_branch.
        let mut ci_active: check_result::ActiveModel = ci_after_update.into();
        ci_active.status = sea_orm::Set("PASSED".to_owned());
        ci_active.head_revision = sea_orm::Set(2);
        ci_active.ci_generation = sea_orm::Set(11);
        ci_active.ci_task_id = sea_orm::Set(Some("tsk_v2".to_owned()));
        ci_active.ci_attempt_id = sea_orm::Set(Some("att_v2".to_owned()));
        sea_orm::ActiveModelTrait::update(ci_active, conn)
            .await
            .expect("update CiStatus to PASSED v2");

        let mut moved_main = main_ref;
        moved_main.ref_commit_hash = mc04_sha(810);
        storage
            .mono_storage()
            .update_ref(moved_main, None)
            .await
            .expect("advance main ref");

        let rebased_head = api
            .update_branch("tester", &cl1.link)
            .await
            .expect("update_branch advances head_revision");
        assert_eq!(rebased_head, mc04_sha(810));

        let cl3 = storage
            .cl_storage()
            .get_cl(&cl1.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cl3.head_revision, 3);

        let ci_after_branch = check_result::Entity::find()
            .filter(check_result::Column::ClLink.eq(&cl1.link))
            .filter(check_result::Column::CheckTypeCode.eq(CheckTypeEnum::CiStatus))
            .one(conn)
            .await
            .expect("load ci")
            .expect("ci row exists");
        assert_eq!(ci_after_branch.status, "INVALIDATED");
        assert_eq!(ci_after_branch.ci_generation, 11);
        assert_eq!(ci_after_branch.ci_task_id, None);
        assert_eq!(ci_after_branch.ci_attempt_id, None);

        let rows3 = cl_webhook_outbox::Entity::find()
            .filter(cl_webhook_outbox::Column::ClLink.eq(&cl1.link))
            .order_by_asc(cl_webhook_outbox::Column::Id)
            .all(conn)
            .await
            .expect("query outbox after update_branch");
        assert_eq!(rows3.len(), 3);
        assert_eq!(rows3[2].event_type, "cl.updated");
        assert_eq!(rows3[2].head_revision, 3);
        let p3: serde_json::Value = serde_json::from_str(&rows3[2].payload_json).unwrap();
        assert_eq!(p3["cl"]["head_revision"], 3);
        assert_eq!(p3["cl"]["head_commit"], mc04_sha(810));
    }

    #[tokio::test]
    async fn cl_head_revision_outbox_create_rolls_back_when_cl_ref_creation_fails() {
        use crate::{
            callisto::{cl_webhook_outbox, mega_cl, mega_refs},
            ceres::code_edit::{
                model::{
                    CLRefUpdateAcceptor, CLRefUpdateVisitor, Checker, CodeEditService,
                    ConversationMessageFormater, DefualtDirector,
                },
                on_edit::OneditCodeEdit,
            },
        };

        struct DummyFormater;
        impl ConversationMessageFormater for DummyFormater {}
        struct FailingVisitor;
        impl CLRefUpdateVisitor for FailingVisitor {
            async fn visit(
                &self,
                _: &mega_cl::Model,
                _: &str,
                _: &str,
                _: &sea_orm::DatabaseTransaction,
            ) -> Result<mega_refs::Model, crate::common::errors::MegaError> {
                Err(crate::common::errors::MegaError::Other(
                    "injected cl ref write failure".to_owned(),
                ))
            }
        }
        struct FailingAcceptor;
        impl CLRefUpdateAcceptor<FailingVisitor> for FailingAcceptor {}
        struct DummyChecker;
        impl Checker for DummyChecker {}

        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        mc04_seed(
            &storage,
            vec![mc04_commit_row(900, &[]), mc04_commit_row(901, &[900])],
        )
        .await;

        let connection = ::redis::aio::ConnectionManager::new_lazy_with_config(
            ::redis::Client::open("redis://127.0.0.1:6379").expect("redis client"),
            ::redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy connection manager");
        let api = MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection,
                prefix: "clb15-ref-atomicity".to_string(),
            }),
        };

        // 1. Failing CL ref write must roll back both mega_cl and cl_webhook_outbox.
        let failing_editor = CodeEditService::new(
            "/",
            "main",
            &mc04_sha(900),
            DummyFormater,
            FailingVisitor,
            FailingAcceptor,
            DummyChecker,
            DefualtDirector {
                handler: api.clone(),
            },
        );
        let err = failing_editor
            .create_new_cl(&storage, "/", &mc04_sha(900), &mc04_sha(901), "tester")
            .await
            .expect_err("injected cl ref write failure must fail create_new_cl");
        assert!(err.to_string().contains("injected cl ref write failure"));

        let cl_stg = storage.cl_storage();
        let conn = cl_stg.get_connection();
        assert!(
            mega_cl::Entity::find().all(conn).await.unwrap().is_empty(),
            "failed CL ref creation must not commit mega_cl"
        );
        assert!(
            cl_webhook_outbox::Entity::find()
                .all(conn)
                .await
                .unwrap()
                .is_empty(),
            "failed CL ref creation must not commit cl_webhook_outbox"
        );

        // 2. Real OneditCodeEdit commits mega_cl, refs/cl/<link>, and cl_webhook_outbox atomically.
        let onedit =
            OneditCodeEdit::from("/", "main", &mc04_sha(900), &api, storage.mono_storage());
        let created = onedit
            .create_new_cl(&storage, "/", &mc04_sha(900), &mc04_sha(901), "tester")
            .await
            .expect("onedit create_new_cl succeeds");
        let cl_ref = storage
            .mono_storage()
            .get_ref_by_name(&format!("refs/cl/{}", created.link))
            .await
            .expect("query cl ref")
            .expect("refs/cl/<link> committed atomically");
        assert_eq!(cl_ref.ref_commit_hash, mc04_sha(901));
        let outbox_rows = cl_webhook_outbox::Entity::find()
            .filter(cl_webhook_outbox::Column::ClLink.eq(&created.link))
            .all(conn)
            .await
            .expect("query outbox");
        assert_eq!(outbox_rows.len(), 1);
        assert_eq!(outbox_rows[0].event_type, "cl.created");
        assert_eq!(outbox_rows[0].head_revision, 1);

        // 3. OnpushCodeEdit creates CL + refs/cl/<cl.link> + cl_webhook_outbox atomically
        //    with the same generated link, and advances refs/cl/<cl.link> atomically on update.
        let onpush = crate::ceres::code_edit::on_push::OnpushCodeEdit::from(
            "/project",
            "main",
            &mc04_sha(900),
            &api,
            storage.mono_storage(),
        );
        let pushed_cl = onpush
            .create_new_cl(
                &storage,
                "/project",
                &mc04_sha(900),
                &mc04_sha(901),
                "tester",
            )
            .await
            .expect("onpush create_new_cl succeeds");
        let pushed_ref_name = format!("refs/cl/{}", pushed_cl.link);
        let pushed_ref = storage
            .mono_storage()
            .get_ref_by_name(&pushed_ref_name)
            .await
            .expect("query pushed ref")
            .expect("refs/cl/<link> must be persisted atomically");
        assert_eq!(pushed_ref.ref_commit_hash, mc04_sha(901));
        let push_outbox = cl_webhook_outbox::Entity::find()
            .filter(cl_webhook_outbox::Column::ClLink.eq(&pushed_cl.link))
            .all(conn)
            .await
            .expect("query push outbox");
        assert_eq!(push_outbox.len(), 1);
        assert_eq!(push_outbox[0].cl_link, pushed_cl.link);

        // Advance existing CL to commit 902 -> refs/cl/<link> must advance atomically.
        mc04_seed(&storage, vec![mc04_commit_row(902, &[901])]).await;
        onpush
            .update_existing_cl(
                pushed_cl.clone(),
                &storage,
                &mc04_sha(900),
                &mc04_sha(902),
                "tester",
            )
            .await
            .expect("onpush update_existing_cl succeeds");
        let advanced_ref = storage
            .mono_storage()
            .get_ref_by_name(&pushed_ref_name)
            .await
            .expect("query advanced ref")
            .expect("refs/cl/<link> exists");
        assert_eq!(advanced_ref.ref_commit_hash, mc04_sha(902));
    }
}
