use crate::{
    callisto::{mega_cl, mega_refs},
    ceres::{api_service::mono_api_service::MonoApiService, code_edit::model::CLRefUpdateVisitor},
    common::{errors::MegaError, utils},
    jupiter::storage::mono_storage::MonoStorage,
};

pub struct OnpushFormator;
impl crate::ceres::code_edit::model::ConversationMessageFormater for OnpushFormator {}

pub struct OnpushVisitor {
    mono_storage: MonoStorage,
}
impl crate::ceres::code_edit::model::CLRefUpdateVisitor for OnpushVisitor {
    async fn visit(
        &self,
        cl: &mega_cl::Model,
        commit_hash: &str,
        tree_hash: &str,
        txn: &sea_orm::DatabaseTransaction,
    ) -> Result<mega_refs::Model, MegaError> {
        let ref_name = utils::cl_ref_name(&cl.link);
        if let Some(mut cl_ref) = self
            .mono_storage
            .get_ref_by_name_in_txn(&ref_name, txn)
            .await?
        {
            cl_ref.ref_commit_hash = commit_hash.to_string();
            cl_ref.ref_tree_hash = tree_hash.to_string();
            self.mono_storage
                .update_ref(cl_ref.clone(), Some(txn))
                .await?;
            Ok(cl_ref)
        } else {
            let cl_ref = mega_refs::Model::new(
                &cl.path,
                ref_name,
                commit_hash.to_string(),
                tree_hash.to_string(),
                true,
            );
            self.mono_storage
                .save_refs(cl_ref.clone(), Some(txn))
                .await?;
            Ok(cl_ref)
        }
    }
}

pub struct OnpushAcceptor {}

impl<VT: CLRefUpdateVisitor> crate::ceres::code_edit::model::CLRefUpdateAcceptor<VT>
    for OnpushAcceptor
{
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

pub struct OnpushChecker {}

impl crate::ceres::code_edit::model::Checker for OnpushChecker {}

pub(crate) type OnpushCodeEdit = crate::ceres::code_edit::model::CodeEditService<
    OnpushFormator,
    OnpushVisitor,
    OnpushAcceptor,
    OnpushChecker,
    MonoApiService,
    crate::ceres::code_edit::model::DefualtDirector<MonoApiService>,
>;

impl OnpushCodeEdit {
    pub fn from(
        repo_path: &str,
        base_branch: &str,
        from_hash: &str,
        handler: &MonoApiService,
        mono_storage: MonoStorage,
    ) -> Self {
        Self::new(
            repo_path,
            base_branch,
            from_hash,
            OnpushFormator {},
            OnpushVisitor { mono_storage },
            OnpushAcceptor {},
            OnpushChecker {},
            crate::ceres::code_edit::model::DefualtDirector {
                handler: handler.clone(),
            },
        )
    }
}
