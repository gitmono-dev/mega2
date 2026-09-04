use std::sync::Arc;

use crate::{
    bellatrix::Bellatrix,
    callisto::{mega_cl, mega_refs},
    ceres::{
        api_service::{cache::GitObjectCache, mono_api_service::MonoApiService},
        build_trigger::{BuildTriggerService, TriggerContext},
        code_edit::{model::CLRefUpdateVisitor, utils as edit_utils},
    },
    common::errors::MegaError,
    jupiter::storage::Storage,
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

pub struct OnpushTrigerBuilder {}

impl crate::ceres::code_edit::model::TriggerContextBuilder for OnpushTrigerBuilder {
    async fn get_context(
        &self,
        cl: &mega_cl::Model,
        username: &str,
    ) -> Result<TriggerContext, MegaError> {
        Ok(TriggerContext::from_git_push(
            cl.path.clone(),
            cl.from_hash.clone(),
            cl.to_hash.clone(),
            cl.link.clone(),
            Some(cl.id),
            Some(username.to_string()),
        ))
    }

    async fn trigger_build(
        &self,
        storage: Storage,
        git_cache: Arc<GitObjectCache>,
        bellatrix: Arc<Bellatrix>,
        cl: &mega_cl::Model,
        username: &str,
    ) -> Result<(), MegaError> {
        let cl_model = cl.clone();
        let username = username.to_string();

        tokio::spawn(async move {
            let repo_path =
                match edit_utils::resolve_build_repo_root(&storage, &cl_model.path).await {
                    Ok(repo_path) => repo_path,
                    Err(e) => {
                        tracing::error!(
                            cl_link = %cl_model.link,
                            cl_path = %cl_model.path,
                            "Failed to resolve build repo root for git push: {}",
                            e
                        );
                        return Err(e);
                    }
                };

            let context = TriggerContext::from_git_push(
                repo_path,
                cl_model.from_hash.clone(),
                cl_model.to_hash.clone(),
                cl_model.link.clone(),
                Some(cl_model.id),
                Some(username),
            );
            BuildTriggerService::build_by_context(storage, git_cache, bellatrix, context).await
        });

        Ok(())
    }
}

pub struct OnpushChecker {}

impl crate::ceres::code_edit::model::Checker for OnpushChecker {}

pub(crate) type OnpushCodeEdit = crate::ceres::code_edit::model::CodeEditService<
    OnpushFormator,
    OnpushVisitor,
    OnpushAcceptor,
    OnpushTrigerBuilder,
    OnpushChecker,
    MonoApiService,
    crate::ceres::code_edit::model::DefualtDirector<MonoApiService>,
>;

// impl<'a> crate::ceres::code_edit::model::CodeEditService<OnpushFormator, OnpushVisitor, OnpushAcceptor, OnpushTrigerBuilder, OnpushChecker, crate::ceres::model::DefualthDirector<'a, Monorepo>> {
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
            OnpushTrigerBuilder {},
            OnpushChecker {},
            crate::ceres::code_edit::model::DefualtDirector {
                handler: handler.clone(),
            },
        )
    }
}
