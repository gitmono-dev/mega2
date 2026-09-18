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
    ) -> Result<mega_refs::Model, MegaError>;
}

pub(crate) trait CLRefUpdateAcceptor<VT: CLRefUpdateVisitor> {
    async fn accept(
        &self,
        visitor: &VT,
        cl: &mega_cl::Model,
        commit_hash: &str,
        tree_hash: &str,
    ) -> Result<(), MegaError> {
        visitor.visit(cl, commit_hash, tree_hash).await?;
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

        let from_same = cl.from_hash == from_hash;
        let to_same = cl.to_hash == to_hash;
        match (from_same, to_same) {
            (true, true) => {
                tracing::info!("repeat commit with change_list: {}, do nothing", cl.id);
            }
            _ => {
                // MC-04: the to_hash advance and the commit-listing rebuild
                // share one DB transaction — neither takes effect without the
                // other, so a listing failure rolls the advance back and a
                // retried push simply re-runs this path. The listing is the
                // complete chain over the frozen `(from_hash, to_hash]`, never
                // the push's increment.
                let chain = collect_cl_chain(storage, &cl.from_hash, to_hash).await?;
                let txn = storage.begin_db_transaction().await?;
                cl_stg
                    .update_cl_to_hash_in_txn(cl.clone(), to_hash, &txn)
                    .await?;
                cl_stg
                    .save_cl_commits_in_txn(&cl.link, &chain, &txn)
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
        let cl_link = generate_link();
        let dst_commit = Commit::from_mega_model(
            storage
                .mono_storage()
                .get_commit_by_hash(to_hash)
                .await?
                .expect("invalid to_hash"),
        );
        let cl_stg = storage.cl_storage();
        // MC-04: CL creation and the commit-listing rebuild share one DB
        // transaction — the listing is the complete `(from_hash, to_hash]`
        // chain, rebuilt at write time.
        let chain = collect_cl_chain(storage, from_hash, to_hash).await?;
        let txn = storage.begin_db_transaction().await?;
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
        txn.commit().await.map_err(MegaError::Db)?;

        self.clref_acceptor
            .accept(
                &self.clref_visitor,
                &cl,
                to_hash,
                &dst_commit.tree_id.to_string(),
            )
            .await?;
        storage
            .conversation_storage()
            .add_conversation(
                &cl.link,
                username,
                Some(self.formator.format(&cl, from_hash, to_hash, username)),
                ConvTypeEnum::Comment,
            )
            .await?;
        storage
            .webhook_service
            .dispatch(WebhookEvent::ClCreated, &cl);
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
        OnpushCodeEdit::from("/", "main", &mc04_sha(0), &api)
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
}
