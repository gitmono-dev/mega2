use std::sync::Arc;

use git_internal::{
    hash::ObjectHash,
    internal::object::{
        commit::{ArchivedCommit, Commit},
        tree::{ArchivedTree, Tree},
    },
};
use rkyv::rancor::Error;

use crate::{
    common::errors::MegaError,
    jupiter::redis::{AsyncCommands, ConnectionManager},
};

#[derive(Clone)]
pub struct GitObjectCache {
    pub connection: ConnectionManager,
    pub prefix: String,
}

const DEFAULT_EXPIRY_SECONDS: u64 = 60 * 60 * 24; // 1 days

impl GitObjectCache {
    fn caching_enabled(&self) -> bool {
        !self.prefix.is_empty() && self.prefix != "disabled"
    }

    pub async fn get_tree<F, Fut>(
        &self,
        oid: ObjectHash,
        fetch_tree: F,
    ) -> Result<Arc<Tree>, MegaError>
    where
        F: Fn(ObjectHash) -> Fut,
        Fut: Future<Output = Result<Tree, MegaError>>,
    {
        let key = format!("{}:tree:{}", self.prefix, oid);
        let mut conn = self.connection.clone();

        if self.caching_enabled()
            && let Ok(data) = conn.get::<_, Vec<u8>>(&key).await
            && !data.is_empty()
        {
            match rkyv::access::<ArchivedTree, Error>(&data) {
                Ok(archived) => {
                    let tree = rkyv::deserialize::<Tree, Error>(archived)?;
                    return Ok(Arc::new(tree));
                }
                Err(err) => {
                    tracing::error!("deserialize failed with fetch key: {:?}, err{:?}", key, err);
                    let _: Result<(), _> = conn.del(&key).await;
                }
            }
        }

        let tree_raw = fetch_tree(oid).await?;
        let tree = Arc::new(tree_raw);
        if self.caching_enabled() {
            match rkyv::to_bytes::<Error>(tree.as_ref()) {
                Ok(serialized) => {
                    if let Err(err) = conn
                        .set_ex::<_, _, ()>(&key, serialized.as_slice(), DEFAULT_EXPIRY_SECONDS)
                        .await
                    {
                        // Cache write must not fail the Git protocol response. Shared
                        // Redis FLUSHALL / transient connection errors otherwise surface
                        // to clients as `error: …` (`bad line length character: erro`).
                        tracing::warn!(
                            key = %key,
                            error = %err,
                            "git object tree cache write failed; serving fetched object"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        key = %key,
                        error = %err,
                        "git object tree cache serialize failed; serving fetched object"
                    );
                }
            }
        }

        Ok(tree)
    }

    pub async fn get_commit<F, Fut>(
        &self,
        oid: ObjectHash,
        fetch_commit: F,
    ) -> Result<Arc<Commit>, MegaError>
    where
        F: Fn(ObjectHash) -> Fut,
        Fut: Future<Output = Result<Commit, MegaError>>,
    {
        let mut conn = self.connection.clone();
        let key = format!("{}:commit:{}", self.prefix, oid);

        if self.caching_enabled()
            && let Ok(data) = conn.get::<_, Vec<u8>>(&key).await
            && !data.is_empty()
        {
            match rkyv::access::<ArchivedCommit, Error>(&data) {
                Ok(archived) => {
                    let commit = rkyv::deserialize::<Commit, Error>(archived)?;
                    return Ok(Arc::new(commit));
                }
                Err(err) => {
                    tracing::error!("deserialize failed with fetch key: {:?} err{:?}", key, err);
                    let _: Result<(), _> = conn.del(&key).await;
                }
            }
        }

        let commit_raw = fetch_commit(oid).await?;
        let commit = Arc::new(commit_raw);
        if self.caching_enabled() {
            match rkyv::to_bytes::<Error>(commit.as_ref()) {
                Ok(serialized) => {
                    if let Err(err) = conn
                        .set_ex::<_, _, ()>(&key, serialized.as_slice(), DEFAULT_EXPIRY_SECONDS)
                        .await
                    {
                        tracing::warn!(
                            key = %key,
                            error = %err,
                            "git object commit cache write failed; serving fetched object"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        key = %key,
                        error = %err,
                        "git object commit cache serialize failed; serving fetched object"
                    );
                }
            }
        }

        Ok(commit)
    }
}
