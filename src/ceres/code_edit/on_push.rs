use crate::{
    callisto::{mega_cl, mega_refs},
    ceres::{api_service::mono_api_service::MonoApiService, code_edit::model::CLRefUpdateVisitor},
    common::errors::MegaError,
};

pub struct OnpushFormator;
impl crate::ceres::code_edit::model::ConversationMessageFormater for OnpushFormator {}

pub struct OnpushVisitor {}
impl crate::ceres::code_edit::model::CLRefUpdateVisitor for OnpushVisitor {
    async fn visit(
        &self,
        _: &mega_cl::Model,
        _: &str,
        _: &str,
    ) -> Result<mega_refs::Model, MegaError> {
        panic!("visit not implemented");
    }
}

pub struct OnpushAcceptor {}

impl<VT: CLRefUpdateVisitor> crate::ceres::code_edit::model::CLRefUpdateAcceptor<VT>
    for OnpushAcceptor
{
    async fn accept(&self, _: &VT, _: &mega_cl::Model, _: &str, _: &str) -> Result<(), MegaError> {
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
    ) -> Self {
        Self::new(
            repo_path,
            base_branch,
            from_hash,
            OnpushFormator {},
            OnpushVisitor {},
            OnpushAcceptor {},
            OnpushChecker {},
            crate::ceres::code_edit::model::DefualtDirector {
                handler: handler.clone(),
            },
        )
    }
}
