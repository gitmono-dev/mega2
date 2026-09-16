use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use chrono::prelude::*;
use futures::{Stream, StreamExt};
use rand::prelude::*;
use reqwest::Method;

use crate::{
    callisto::lfs_locks,
    ceres::lfs::{
        digest::{LfsDigest, LfsDigestAlgorithm},
        lfs_structs::{
            BatchRequest, BatchResponse, Lock, LockList, LockListQuery, LockRequest, MetaObject,
            ObjectError, Operation, RequestObject, ResCondition, ResponseObject, TransferMode,
            UnlockRequest, VerifiableLockList, VerifiableLockRequest,
        },
    },
    common::errors::{GitLFSError, MegaError},
    jupiter::{
        service::lfs_service::LfsService,
        storage::{lfs_db_storage::LfsDbStorage, object_storage::MegaObjectStorageWrapper},
        utils::into_obj_stream::IntoObjectStream,
    },
};
#[rustfmt::skip]
use crate::orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace};

/// Namespaces an LFS lock row key by repository so that identical ref names in
/// different repositories do not share a lock bucket. The `\u{1f}` (unit
/// separator) cannot appear in a Git ref name, keeping the composite key
/// unambiguous. An empty `repo` (non repo-scoped mount, e.g. `/api/v1/lfs`)
/// preserves the legacy bare-ref key for backward compatibility.
fn scoped_lock_ref(repo: &str, refspec: &str) -> String {
    if repo.is_empty() {
        refspec.to_owned()
    } else {
        format!("{repo}\u{1f}{refspec}")
    }
}

pub async fn lfs_retrieve_lock(
    storage: LfsDbStorage,
    repo: &str,
    query: LockListQuery,
) -> Result<LockList, GitLFSError> {
    let mut lock_list = LockList {
        locks: vec![],
        next_cursor: "".to_string(),
    };
    match lfs_get_filtered_locks(
        storage,
        &scoped_lock_ref(repo, &query.refspec),
        &query.path,
        &query.cursor,
        &query.limit,
    )
    .await
    {
        Ok((locks, next)) => {
            lock_list.locks = locks;
            lock_list.next_cursor = next;
            Ok(lock_list)
        }
        // Client-input errors (e.g. a malformed `limit`) must reach the router
        // unmasked so `map_lfs_error` can classify them as 400; only genuine
        // lookup failures are hidden behind the generic message.
        // Ported from mega@2398a92 `ceres/src/lfs/handler.rs` (#2175).
        Err(GitLFSError::GeneralError(msg)) if msg.starts_with("Invalid") => {
            Err(GitLFSError::GeneralError(msg))
        }
        Err(_) => Err(GitLFSError::GeneralError(
            "Lookup operation failed!".to_string(),
        )),
    }
}

pub async fn lfs_verify_lock(
    storage: LfsDbStorage,
    repo: &str,
    req: VerifiableLockRequest,
) -> Result<VerifiableLockList, MegaError> {
    let mut limit = req.limit.unwrap_or(0);
    if limit == 0 {
        limit = 100;
    }
    let res = lfs_get_filtered_locks(
        storage,
        &scoped_lock_ref(repo, &req.refs.name),
        "",
        &req.cursor.clone().unwrap_or("".to_string()).to_string(),
        &limit.to_string(),
    )
    .await;

    let mut lock_list = VerifiableLockList {
        ours: vec![],
        theirs: vec![],
        next_cursor: "".to_string(),
    };
    match res {
        Ok((locks, next_cursor)) => {
            lock_list.next_cursor = next_cursor;

            for lock in locks.iter() {
                if Option::is_none(&lock.owner) {
                    lock_list.ours.push(lock.clone());
                } else {
                    lock_list.theirs.push(lock.clone());
                }
            }
        }
        Err(_) => return Err(MegaError::Other("Lookup operation failed!".to_string())),
    };
    Ok(lock_list)
}

pub async fn lfs_create_lock(
    storage: LfsDbStorage,
    repo: &str,
    req: LockRequest,
) -> Result<Lock, GitLFSError> {
    let lock_ref = scoped_lock_ref(repo, &req.refs.name);
    let res =
        lfs_get_filtered_locks(storage.clone(), &lock_ref, &req.path.to_string(), "", "1").await;

    match res {
        Ok((locks, _)) => {
            if !locks.is_empty() {
                return Err(GitLFSError::GeneralError("Lock already exist".to_string()));
            }
        }
        Err(_) => {
            return Err(GitLFSError::GeneralError(
                "Failed when filtering locks!".to_string(),
            ));
        }
    };

    let lock = Lock {
        id: {
            let mut random_num = String::new();
            let mut rng = rand::rng();
            for _ in 0..8 {
                random_num += &(rng.random_range(0..9)).to_string();
            }
            random_num
        },
        path: req.path.to_owned(),
        owner: None,
        locked_at: {
            let locked_at: DateTime<Utc> = Utc::now();
            locked_at.to_rfc3339()
        },
    };

    match lfs_add_lock(storage.clone(), &lock_ref, vec![lock.clone()]).await {
        Ok(_) => Ok(lock),
        Err(_) => Err(GitLFSError::GeneralError(
            "Failed when adding locks!".to_string(),
        )),
    }
}

pub async fn lfs_delete_lock(
    storage: LfsDbStorage,
    repo: &str,
    id: &str,
    unlock_request: UnlockRequest,
) -> Result<Lock, GitLFSError> {
    if id.is_empty() {
        return Err(GitLFSError::GeneralError("Invalid lock id!".to_string()));
    }
    let res = delete_lock(
        storage,
        &scoped_lock_ref(repo, &unlock_request.refs.name),
        None,
        id,
        unlock_request.force.unwrap_or(false),
    )
    .await;
    match res {
        Ok(deleted_lock) => {
            if deleted_lock.id.is_empty()
                && deleted_lock.path.is_empty()
                && deleted_lock.owner.is_none()
                && deleted_lock.locked_at == DateTime::<Utc>::MIN_UTC.to_rfc3339()
            {
                Err(GitLFSError::GeneralError(
                    "Unable to find lock!".to_string(),
                ))
            } else {
                Ok(deleted_lock)
            }
        }
        Err(_) => Err(GitLFSError::GeneralError(
            "Delete operation failed!".to_string(),
        )),
    }
}

///
///
/// Reference:
///     1. [Git LFS Batch API](https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md)
pub async fn lfs_process_batch(
    service: &LfsService,
    request: BatchRequest,
    listen_addr: &str,
) -> Result<BatchResponse, GitLFSError> {
    let algorithm = request
        .prepare_digest_domain()
        .map_err(|e| GitLFSError::GeneralError(e.to_string()))?;
    let objects = request.objects;

    let mut response_objects = Vec::new();
    let file_storage = service.obj_storage.clone();
    let db_storage = service.lfs_storage.clone();
    for mut object in objects {
        let digest = object
            .parse_lfs_digest(algorithm)
            .map_err(|e| GitLFSError::GeneralError(e.to_string()))?;
        object.oid = digest.hex().to_string();
        let meta_res = lfs_get_meta(&db_storage, &object.oid).await?;
        let meta = match meta_res {
            Some(meta) => meta,
            None => {
                if request.operation == Operation::Upload {
                    // Save to database if not exist.
                    let meta = MetaObject::new(&object);
                    db_storage
                        .new_lfs_object(meta.clone().into())
                        .await
                        .map_err(|e| lfs_storage_error("create LFS object metadata", e))?;
                    meta
                } else {
                    response_objects.push(ResponseObject::failed_with_err(
                        &object,
                        ObjectError {
                            code: 404,
                            message: "Not found".to_owned(),
                        },
                    ));
                    continue;
                }
            }
        };
        let file_exist = lfs_object_exists(&file_storage, &meta.oid).await;
        let download_url = match lfs_download_url(&file_storage, &meta.oid, listen_addr).await {
            Ok(url) => url,
            Err(e) => {
                tracing::error!("Failed to generate download URL for {}: {}", meta.oid, e);
                response_objects.push(ResponseObject::failed_with_err(
                    &object,
                    ObjectError {
                        code: 500,
                        message: format!("Failed to generate download URL: {}", e),
                    },
                ));
                continue;
            }
        };
        let upload_url = match lfs_upload_url(&file_storage, &meta.oid, listen_addr).await {
            Ok(url) => url,
            Err(e) => {
                tracing::error!("Failed to generate upload URL for {}: {}", meta.oid, e);
                response_objects.push(ResponseObject::failed_with_err(
                    &object,
                    ObjectError {
                        code: 500,
                        message: format!("Failed to generate upload URL: {}", e),
                    },
                ));
                continue;
            }
        };

        response_objects.push(ResponseObject::new(
            &meta,
            ResCondition {
                file_exist,
                operation: request.operation.clone(),
                use_tus: false,
            },
            &download_url,
            &upload_url,
        ));
    }

    Ok(BatchResponse {
        transfer: TransferMode::BASIC,
        objects: response_objects,
        hash_algo: algorithm.as_str().to_string(),
    })
}

/// Upload object to storage.
/// if server enable split, split the object and upload each part to storage, save the relationship to database.
pub async fn lfs_upload_object(
    service: &LfsService,
    req_obj: &RequestObject,
    body_bytes: Vec<u8>,
) -> Result<(), GitLFSError> {
    let db_storage: LfsDbStorage = service.lfs_storage.clone();

    let claimed = LfsDigest::from_hex_for_algorithm(LfsDigestAlgorithm::Sha256, &req_obj.oid)
        .map_err(|e| GitLFSError::GeneralError(e.to_string()))?;
    let meta = if let Some(meta) = lfs_get_meta(&db_storage, claimed.hex()).await? {
        tracing::debug!("upload lfs object {} size: {}", meta.oid, meta.size);
        meta
    } else {
        return Err(GitLFSError::GeneralError(String::from("Not found ")));
    };

    // Content-addressed immutability. The raw transfer PUT is a capability URL
    // issued by the auth-gated batch endpoint and is not itself per-request
    // authenticated, so verify the uploaded bytes actually address the claimed
    // OID (LFS uses sha256) and match the registered size. This makes objects
    // immutable by content: an unauthenticated PUT can only (re)store the exact
    // bytes that hash to the OID — it cannot corrupt or spoof an existing one.
    if body_bytes.len() as i64 != meta.size {
        return Err(GitLFSError::GeneralError(format!(
            "Invalid LFS upload: size {} does not match registered size {} for oid {}",
            body_bytes.len(),
            meta.size,
            meta.oid
        )));
    }
    let digest = LfsDigest::sha256_of(&body_bytes);
    if digest.hex() != meta.oid {
        return Err(GitLFSError::GeneralError(format!(
            "Invalid LFS upload: content hash {} does not match oid {}",
            digest.hex(),
            meta.oid
        )));
    }
    // Objects are immutable by OID; if it is already stored, a matching upload
    // is a no-op and must not overwrite the existing blob.
    if lfs_object_exists(&service.obj_storage, &meta.oid).await {
        return Ok(());
    }

    let key = lfs_object_key(&meta.oid);
    let size = meta.size;
    let res = service
        .obj_storage
        .inner
        .put_stream(
            &key,
            body_bytes.into_stream(),
            ObjectMeta {
                size,
                ..Default::default()
            },
        )
        .await;
    if let Err(_e) = res {
        if let Err(delete_err) = lfs_delete_meta(&db_storage, req_obj).await {
            tracing::error!(
                "Failed to cleanup LFS metadata for oid {} after upload failure: {}",
                meta.oid,
                delete_err
            );
        }
        return Err(GitLFSError::GeneralError(String::from(
            "Header not acceptable!",
        )));
    }

    // WH-05 (plan-20260912 / ADR-WH-04/05): exactly one `lfs.object.uploaded`
    // when THIS request's put_stream actually stored the object. The exists
    // no-op, hash/size rejections, batch metadata creation and the presigned
    // direct-upload path never reach this point. The exists-then-put check is
    // not atomic, so concurrent uploads may both succeed and both notify — no
    // first-write exactly-once is claimed. Scope is unknown (null); only
    // `include_unscoped_lfs = true` targets receive it.
    let event = crate::jupiter::service::storage_event::CommittedEvent {
        event_id: uuid::Uuid::new_v4(),
        event_type: crate::jupiter::service::storage_event::EventType::LfsObjectUploaded,
        occurred_at: chrono::Utc::now().timestamp() as u64,
        source: crate::jupiter::service::storage_event::EventSource::Lfs,
        scope: crate::jupiter::service::storage_event::EventScope::LfsUnscoped,
        data: crate::jupiter::service::storage_event::EventData::LfsObjectUploaded {
            oid: meta.oid.clone(),
            size: meta.size as u64,
        },
    };
    let _ = service.storage_event_emitter.try_emit(event);
    Ok(())
}

/// Download object from storage.
/// when server enable split,  if OID is a complete object, then splice the object and return it.
pub async fn lfs_download_object(
    service: LfsService,
    oid: String,
) -> Result<impl Stream<Item = Result<Bytes, GitLFSError>>, GitLFSError> {
    let db_storage = service.lfs_storage.clone();
    let file_storage = service.obj_storage.clone();

    let meta = lfs_get_meta(&db_storage, &oid).await?;
    match meta {
        Some(meta) => {
            // Fetch object from unified object storage.
            let key = lfs_object_key(&meta.oid);
            let (stream, _meta) = match file_storage.inner.get_stream(&key).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("Failed to get LFS object {}: {}", meta.oid, e);
                    return Err(GitLFSError::GeneralError(format!(
                        "Failed to retrieve object: {}",
                        e
                    )));
                }
            };
            // Map storage's `ObjectByteStream` into the expected `GitLFSError` stream type.
            let mapped = stream.map(|chunk| match chunk {
                Ok(bytes) => Ok(bytes),
                Err(e) => Err(GitLFSError::GeneralError(format!(
                    "Stream error while reading object: {}",
                    e
                ))),
            });
            Ok(mapped)
        }
        None => Err(GitLFSError::GeneralError(format!(
            "LFS object not found: {}",
            oid
        ))),
    }
}

fn lfs_storage_error(context: &str, error: impl std::fmt::Display) -> GitLFSError {
    GitLFSError::GeneralError(format!("{context}: {error}"))
}

async fn lfs_get_filtered_locks(
    storage: LfsDbStorage,
    refspec: &str,
    path: &str,
    cursor: &str,
    limit: &str,
) -> Result<(Vec<Lock>, String), GitLFSError> {
    let mut locks = lfs_get_locks(storage, refspec).await?;

    tracing::debug!("Locks retrieved: {:?}", locks);

    if !cursor.is_empty() {
        let mut last_seen = -1;
        for (i, v) in locks.iter().enumerate() {
            if v.id == *cursor {
                last_seen = i as i32;
                break;
            }
        }

        if last_seen > -1 {
            locks = locks.split_off(last_seen as usize);
        } else {
            // Cursor not found.
            return Err(GitLFSError::GeneralError("".to_string()));
        }
    }

    if !path.is_empty() {
        let mut filterd = Vec::<Lock>::new();
        for lock in locks.iter() {
            if lock.path == *path {
                filterd.push(Lock {
                    id: lock.id.to_owned(),
                    path: lock.path.to_owned(),
                    owner: lock.owner.clone(),
                    locked_at: lock.locked_at.to_owned(),
                });
            }
        }
        locks = filterd;
    }

    apply_lock_limit(locks, limit)
}

/// Applies the `limit` parameter to an ordered lock list, returning the page and
/// the id of the first lock past the page (empty when no further page exists).
///
/// The limit comes straight from the query string, so anything non-numeric
/// (including negative values) is rejected instead of being parsed leniently.
/// Ported from mega@2398a92 `ceres/src/lfs/handler.rs` (#2175).
fn apply_lock_limit(locks: Vec<Lock>, limit: &str) -> Result<(Vec<Lock>, String), GitLFSError> {
    let mut next = "".to_string();
    let mut locks = locks;
    if !limit.is_empty() {
        let size = limit
            .parse::<usize>()
            .map_err(|_| GitLFSError::GeneralError(format!("Invalid limit parameter: {limit}")))?;
        let size = size.min(locks.len());

        if size + 1 < locks.len() {
            locks[size].id.clone_into(&mut next);
        }
        let _ = locks.split_off(size);
    }

    Ok((locks, next))
}

async fn lfs_get_locks(storage: LfsDbStorage, refspec: &str) -> Result<Vec<Lock>, GitLFSError> {
    let result = storage
        .get_lock_by_id(refspec)
        .await
        .map_err(|e| lfs_storage_error("get LFS lock row", e))?;
    match result {
        Some(val) => {
            let data = val.data;
            let locks: Vec<Lock> = serde_json::from_str(&data)
                .map_err(|e| lfs_storage_error("parse LFS lock data", e))?;
            Ok(locks)
        }
        None => Ok(Vec::new()),
    }
}

async fn lfs_add_lock(
    storage: LfsDbStorage,
    repo: &str,
    locks: Vec<Lock>,
) -> Result<(), GitLFSError> {
    let result = storage
        .get_lock_by_id(repo)
        .await
        .map_err(|e| lfs_storage_error("get LFS lock row", e))?;

    match result {
        // Update
        Some(val) => {
            let d = val.data.to_owned();
            let mut locks_from_data = if !d.is_empty() {
                let locks_from_data: Vec<Lock> = serde_json::from_str(&d)
                    .map_err(|e| lfs_storage_error("parse LFS lock data", e))?;
                locks_from_data
            } else {
                vec![]
            };
            let mut locks = locks;
            locks_from_data.append(&mut locks);

            locks_from_data.sort_by(|a, b| {
                a.locked_at
                    .partial_cmp(&b.locked_at)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let d = serde_json::to_string(&locks_from_data)
                .map_err(|e| lfs_storage_error("serialize LFS lock data", e))?;

            // must turn into `ActiveModel` before modify, or update failed.
            // let mut val = val.into_active_model();
            // val.data = Set(d);
            storage
                .update_lock(val, &d)
                .await
                .map_err(|e| lfs_storage_error("update LFS lock row", e))?;
            Ok(())
        }
        // Insert
        None => {
            let mut locks = locks;
            locks.sort_by(|a, b| {
                a.locked_at
                    .partial_cmp(&b.locked_at)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let data = serde_json::to_string(&locks)
                .map_err(|e| lfs_storage_error("serialize LFS lock data", e))?;
            let lock_to = lfs_locks::Model {
                id: repo.to_owned(),
                data: data.to_owned(),
            };

            storage
                .new_lock(lock_to)
                .await
                .map_err(|e| lfs_storage_error("create LFS lock row", e))?;
            Ok(())
        }
    }
}

async fn lfs_get_meta(
    storage: &LfsDbStorage,
    oid: &str,
) -> Result<Option<MetaObject>, GitLFSError> {
    Ok(storage
        .get_lfs_object(oid)
        .await
        .map_err(|e| lfs_storage_error("get LFS object metadata", e))?
        .map(|m| m.into()))
}

async fn lfs_delete_meta(
    storage: &LfsDbStorage,
    req_obj: &RequestObject,
) -> Result<(), GitLFSError> {
    let res = storage.delete_lfs_object(req_obj.oid.to_owned()).await;
    match res {
        Ok(_) => Ok(()),
        Err(_) => Err(GitLFSError::GeneralError("".to_string())),
    }
}

fn lfs_object_key(oid: &str) -> ObjectKey {
    ObjectKey {
        namespace: ObjectNamespace::Lfs,
        key: oid.to_string(),
    }
}

async fn lfs_object_exists(storage: &MegaObjectStorageWrapper, oid: &str) -> bool {
    let key = lfs_object_key(oid);

    match storage.inner.exists(&key).await {
        Ok(exists) => exists,
        Err(err) => {
            tracing::warn!("Failed to check LFS object {} existence: {}", oid, err);
            false
        }
    }
}

async fn lfs_download_url(
    storage: &MegaObjectStorageWrapper,
    oid: &str,
    hostname: &str,
) -> Result<String, MegaError> {
    let key = lfs_object_key(oid);

    if let Some(url) = storage
        .inner
        .signed_url(&key, Method::GET, Duration::from_secs(3600))
        .await?
    {
        return Ok(url);
    }

    Ok(format!("{}/info/lfs/objects/{}", hostname, oid))
}

async fn lfs_upload_url(
    storage: &MegaObjectStorageWrapper,
    oid: &str,
    hostname: &str,
) -> Result<String, MegaError> {
    let key = lfs_object_key(oid);

    if let Some(url) = storage
        .inner
        .signed_url(&key, Method::PUT, Duration::from_secs(3600))
        .await?
    {
        return Ok(url);
    }

    Ok(format!("{}/info/lfs/objects/{}", hostname, oid))
}

async fn delete_lock(
    storage: LfsDbStorage,
    repo: &str,
    _user: Option<String>,
    id: &str,
    force: bool,
) -> Result<Lock, GitLFSError> {
    let result = storage
        .get_lock_by_id(repo)
        .await
        .map_err(|e| lfs_storage_error("get LFS lock row", e))?;
    match result {
        // Exist, then delete.
        Some(val) => {
            let d = val.data.to_owned();
            let locks_from_data = if !d.is_empty() {
                let locks_from_data: Vec<Lock> = serde_json::from_str(&d)
                    .map_err(|e| lfs_storage_error("parse LFS lock data", e))?;
                locks_from_data
            } else {
                vec![]
            };

            let mut new_locks = Vec::<Lock>::new();
            let mut lock_to_delete = Lock {
                id: "".to_owned(),
                path: "".to_owned(),
                owner: None,
                locked_at: {
                    let locked_at: DateTime<Utc> = DateTime::<Utc>::MIN_UTC;
                    locked_at.to_rfc3339()
                },
            };

            for lock in locks_from_data.iter() {
                if lock.id == *id {
                    if Option::is_some(&lock.owner) && !force {
                        return Err(GitLFSError::GeneralError("".to_string()));
                    }
                    lock.id.clone_into(&mut lock_to_delete.id);
                    lock.path.clone_into(&mut lock_to_delete.path);
                    lock_to_delete.owner.clone_from(&lock.owner);
                    lock.locked_at.clone_into(&mut lock_to_delete.locked_at);
                } else if !lock.id.is_empty() {
                    new_locks.push(Lock {
                        id: lock.id.to_owned(),
                        path: lock.path.to_owned(),
                        owner: lock.owner.clone(),
                        locked_at: lock.locked_at.to_owned(),
                    });
                }
            }
            if lock_to_delete.id.is_empty() {
                return Err(GitLFSError::GeneralError("".to_string()));
            }

            // No locks remains, delete the repo from database.
            if new_locks.is_empty() {
                storage
                    .delete_lock_by_id(repo.to_owned())
                    .await
                    .map_err(|e| lfs_storage_error("delete LFS lock row", e))?;
                return Ok(lock_to_delete);
            }

            // Update remaining locks.
            let data = serde_json::to_string(&new_locks)
                .map_err(|e| lfs_storage_error("serialize LFS lock data", e))?;
            storage
                .update_lock(val, &data)
                .await
                .map_err(|e| lfs_storage_error("update LFS lock row", e))?;
            Ok(lock_to_delete)
        }
        // Not exist, error.
        None => Err(GitLFSError::GeneralError("".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        ceres::lfs::lfs_structs::{Action, Ref, ResCondition, ResponseObject},
        jupiter::storage::object_storage::mock_object_storage,
    };

    fn lock(id: &str) -> Lock {
        Lock {
            id: id.to_string(),
            path: format!("/dir/{id}.bin"),
            owner: None,
            locked_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn lock_limit_slices_page_and_reports_next_cursor() {
        let locks = vec![lock("1"), lock("2"), lock("3"), lock("4")];

        let (page, next) = apply_lock_limit(locks, "2").unwrap();
        assert_eq!(
            page.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(),
            ["1", "2"]
        );
        assert_eq!(next, "3");
    }

    #[test]
    fn lock_limit_beyond_list_returns_everything_without_cursor() {
        let locks = vec![lock("1"), lock("2")];

        let (page, next) = apply_lock_limit(locks, "10").unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(next, "");
    }

    #[test]
    fn empty_limit_returns_unpaged_locks() {
        let locks = vec![lock("1")];

        let (page, next) = apply_lock_limit(locks, "").unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(next, "");
    }

    #[test]
    fn non_numeric_and_negative_limits_are_rejected() {
        // The router classifies by message content ("Invalid..." -> 400), so the
        // rejection must carry that prefix to survive the lookup-error masking
        // in `lfs_retrieve_lock`.
        for limit in ["abc", "-1"] {
            match apply_lock_limit(vec![lock("1")], limit) {
                Err(GitLFSError::GeneralError(msg)) => {
                    assert!(msg.starts_with("Invalid"), "unexpected message: {msg}");
                }
                other => panic!("expected GeneralError, got {other:?}"),
            }
        }
    }

    /// SYNC-04 (mega@2398a92, #2175): the `Invalid` rejection must reach the
    /// caller unmasked so the router can classify it as 400.
    #[tokio::test]
    async fn retrieve_lock_passes_invalid_limit_error_through_unmasked() {
        use crate::ceres::lfs::lfs_structs::LockListQuery;

        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let lfs = storage.lfs_db_storage();

        let query = LockListQuery {
            path: String::new(),
            id: String::new(),
            cursor: String::new(),
            limit: "-1".to_string(),
            refspec: "refs/heads/main".to_string(),
        };
        match lfs_retrieve_lock(lfs, "/org/repo.git", query).await {
            Err(GitLFSError::GeneralError(msg)) => {
                assert!(
                    msg.starts_with("Invalid limit parameter"),
                    "unexpected message: {msg}"
                );
            }
            Ok(_) => panic!("expected the invalid limit to be rejected"),
        }
    }

    #[tokio::test]
    async fn locks_are_namespaced_by_repository() {
        use crate::ceres::lfs::lfs_structs::{LockListQuery, LockRequest};

        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let lfs = storage.lfs_db_storage();

        let mk_lock_req = || LockRequest {
            path: "assets/model.bin".to_string(),
            refs: Ref {
                name: "refs/heads/main".to_string(),
            },
        };
        let list_query = |refspec: &str| LockListQuery {
            path: String::new(),
            id: String::new(),
            cursor: String::new(),
            limit: String::new(),
            refspec: refspec.to_string(),
        };

        // Lock created under repo A.
        lfs_create_lock(lfs.clone(), "/org/repo-a.git", mk_lock_req())
            .await
            .expect("create lock in repo A");

        // Visible under repo A + same ref.
        let a = lfs_retrieve_lock(
            lfs.clone(),
            "/org/repo-a.git",
            list_query("refs/heads/main"),
        )
        .await
        .expect("list repo A");
        assert_eq!(a.locks.len(), 1);
        assert_eq!(a.locks[0].path, "assets/model.bin");

        // The identical ref name in a different repo must not see repo A's lock...
        let b = lfs_retrieve_lock(
            lfs.clone(),
            "/org/repo-b.git",
            list_query("refs/heads/main"),
        )
        .await
        .expect("list repo B");
        assert!(b.locks.is_empty());

        // ...and locking the same path/ref there must not collide with repo A.
        lfs_create_lock(lfs.clone(), "/org/repo-b.git", mk_lock_req())
            .await
            .expect("create lock in repo B without cross-repo collision");

        // The empty repo (legacy /api/v1/lfs mount) uses the bare ref key and
        // stays isolated from the repo-scoped rows above.
        let legacy = lfs_retrieve_lock(lfs.clone(), "", list_query("refs/heads/main"))
            .await
            .expect("list legacy mount");
        assert!(legacy.locks.is_empty());
    }

    #[tokio::test]
    async fn lfs_upload_rejects_content_not_matching_oid() {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let service = LfsService {
            lfs_storage: storage.lfs_db_storage(),
            obj_storage: mock_object_storage(),
            storage_event_emitter:
                crate::jupiter::service::storage_event_emitter::StorageEventEmitter::disabled(),
        };

        // Register metadata as an `upload` batch would, for the true sha256 of
        // the content and its exact size.
        let content = b"mega2 lfs content".to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let oid = hex::encode(hasher.finalize());
        service
            .lfs_storage
            .new_lfs_object(
                MetaObject {
                    oid: oid.clone(),
                    size: content.len() as i64,
                    exist: false,
                }
                .into(),
            )
            .await
            .expect("register lfs metadata");

        // Tampered bytes (same length, different hash) are rejected before any
        // object-storage write, so an unauthenticated PUT cannot corrupt the OID.
        let mut tampered = content.clone();
        tampered[0] ^= 0xff;
        let req = RequestObject {
            oid: oid.clone(),
            size: content.len() as i64,
            ..Default::default()
        };
        let err = lfs_upload_object(&service, &req, tampered)
            .await
            .expect_err("content not matching oid must be rejected");
        assert!(
            err.to_string().contains("does not match oid"),
            "unexpected error: {err}"
        );

        // A size mismatch is likewise rejected.
        let err2 = lfs_upload_object(&service, &req, vec![0u8; 5])
            .await
            .expect_err("size mismatch must be rejected");
        assert!(
            err2.to_string().contains("does not match registered size"),
            "unexpected error: {err2}"
        );
    }

    #[test]
    fn response_object_download_existing() {
        let meta = MetaObject {
            oid: "oid1".into(),
            size: 10,
            exist: true,
        };
        let res = ResponseObject::new(
            &meta,
            ResCondition {
                file_exist: true,
                operation: Operation::Download,
                use_tus: false,
            },
            "http://dl",
            "http://ul",
        );
        assert!(res.actions.is_some());
        let actions = res.actions.unwrap();
        assert!(actions.contains_key(&Action::Download));
        assert!(res.error.is_none());
    }

    #[test]
    fn response_object_upload_new() {
        let meta = MetaObject {
            oid: "oid2".into(),
            size: 20,
            exist: false,
        };
        let res = ResponseObject::new(
            &meta,
            ResCondition {
                file_exist: false,
                operation: Operation::Upload,
                use_tus: false,
            },
            "http://dl",
            "http://ul",
        );
        let actions = res.actions.expect("upload should provide actions");
        assert!(actions.contains_key(&Action::Upload));
        assert!(res.error.is_none());
    }

    #[test]
    fn response_object_download_missing_sets_error() {
        let meta = MetaObject {
            oid: "oid3".into(),
            size: 30,
            exist: false,
        };
        let res = ResponseObject::new(
            &meta,
            ResCondition {
                file_exist: false,
                operation: Operation::Download,
                use_tus: false,
            },
            "http://dl",
            "http://ul",
        );
        assert!(res.actions.is_none());
        assert!(res.error.is_some());
        assert_eq!(res.error.unwrap().code, 404);
    }

    #[test]
    fn unlock_request_defaults() {
        let req = UnlockRequest::default();
        assert!(req.force.is_none());
        assert_eq!(req.refs, Ref { name: "".into() });
    }

    #[tokio::test]
    async fn b3_02a_batch_rejects_blake3_and_wrong_width_oids() {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let service = LfsService {
            lfs_storage: storage.lfs_db_storage(),
            obj_storage: mock_object_storage(),
            storage_event_emitter:
                crate::jupiter::service::storage_event_emitter::StorageEventEmitter::disabled(),
        };
        let oid = LfsDigest::sha256_of(b"").hex().to_string();

        let blake3_err = match lfs_process_batch(
            &service,
            BatchRequest {
                operation: Operation::Upload,
                transfers: Vec::new(),
                objects: vec![RequestObject {
                    oid: oid.clone(),
                    size: 0,
                    ..Default::default()
                }],
                hash_algo: "blake3".to_string(),
            },
            "127.0.0.1:0",
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("blake3 batch must not enter LFS business path"),
        };
        assert!(
            blake3_err.to_string().contains("DEFER-B3-LFS-01"),
            "{blake3_err}"
        );

        let width_err = match lfs_process_batch(
            &service,
            BatchRequest {
                operation: Operation::Upload,
                transfers: Vec::new(),
                objects: vec![RequestObject {
                    oid: "a".repeat(40),
                    size: 0,
                    ..Default::default()
                }],
                hash_algo: "sha256".to_string(),
            },
            "127.0.0.1:0",
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("wrong-width oid must fail closed"),
        };
        assert!(
            width_err.to_string().contains("width mismatch"),
            "{width_err}"
        );
    }

    #[tokio::test]
    async fn b3_02a_batch_echoes_sha256_hash_algo() {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let service = LfsService {
            lfs_storage: storage.lfs_db_storage(),
            obj_storage: mock_object_storage(),
            storage_event_emitter:
                crate::jupiter::service::storage_event_emitter::StorageEventEmitter::disabled(),
        };
        let oid = LfsDigest::sha256_of(b"lfs-batch").hex().to_string();
        let response = lfs_process_batch(
            &service,
            BatchRequest {
                operation: Operation::Upload,
                transfers: Vec::new(),
                objects: vec![RequestObject {
                    oid: oid.clone(),
                    size: 9,
                    ..Default::default()
                }],
                hash_algo: String::new(),
            },
            "127.0.0.1:0",
        )
        .await
        .expect("standard sha256 batch");
        assert_eq!(response.hash_algo, "sha256");
        assert_eq!(response.objects.len(), 1);
        assert_eq!(response.objects[0].oid, oid);
    }

    // ------------------------------------------------------------------
    // WH-05 (plan-20260912 / ADR-WH-04/05): `lfs.object.uploaded` fires only
    // when this request's basic `put_stream` actually stored the object.
    // ------------------------------------------------------------------

    /// Recording fake transport (WH-09 seam): exact bodies + call counter.
    #[derive(Default)]
    struct RecordingTransport {
        calls: std::sync::atomic::AtomicUsize,
        bodies: std::sync::Mutex<Vec<Bytes>>,
        fail: bool,
    }

    impl RecordingTransport {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<Bytes> {
            self.bodies.lock().expect("bodies").clone()
        }
    }

    impl crate::jupiter::service::storage_event_transport::EventTransport for RecordingTransport {
        fn post(
            &self,
            _target: &crate::jupiter::service::storage_event_transport::EventTarget,
            body: Bytes,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::jupiter::service::storage_event_transport::TransportSuccess,
                            crate::jupiter::service::storage_event_transport::TransportError,
                        >,
                    > + Send,
            >,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.bodies.lock().expect("bodies").push(body);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(crate::jupiter::service::storage_event_transport::TransportError::Timeout)
                } else {
                    Ok(
                        crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                            status: 200,
                        },
                    )
                }
            })
        }
    }

    const WH05_INSTALLATION: &str = "it-wh05";

    /// One compiled target per id; `include_unscoped_lfs` per the flag.
    fn wh05_target(
        id: &str,
        include_unscoped_lfs: bool,
    ) -> (
        crate::config::StorageEventsTargetConfig,
        crate::jupiter::service::storage_event_transport::EventTarget,
    ) {
        let config = crate::config::StorageEventsTargetConfig {
            id: id.to_owned(),
            url: "https://events.example.invalid/ingest".to_owned(),
            secret_ref: format!("vault://secret/config/it/storage_events/targets/{id}/hmac#value"),
            events: vec!["lfs.object.uploaded".to_owned()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = crate::config::secret::SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled = crate::jupiter::service::storage_event_transport::EventTarget::compile(
            &config.id,
            &config.url,
            &secret,
        )
        .expect("compile target");
        (config, compiled)
    }

    /// LfsService over the mock object store with a recording emitter.
    async fn wh05_service(
        transport: Arc<RecordingTransport>,
        targets: Vec<(
            crate::config::StorageEventsTargetConfig,
            crate::jupiter::service::storage_event_transport::EventTarget,
        )>,
    ) -> (tempfile::TempDir, LfsService) {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some(WH05_INSTALLATION.to_owned());
        let emitter =
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
                &config, transport, targets,
            );
        let service = LfsService {
            lfs_storage: storage.lfs_db_storage(),
            obj_storage: mock_object_storage(),
            storage_event_emitter: emitter,
        };
        (temp, service)
    }

    /// Register the object's metadata the way an `upload` batch would.
    async fn wh05_register(service: &LfsService, content: &[u8]) -> RequestObject {
        let mut hasher = Sha256::new();
        hasher.update(content);
        let oid = hex::encode(hasher.finalize());
        service
            .lfs_storage
            .new_lfs_object(
                MetaObject {
                    oid: oid.clone(),
                    size: content.len() as i64,
                    exist: false,
                }
                .into(),
            )
            .await
            .expect("register lfs metadata");
        RequestObject {
            oid,
            size: content.len() as i64,
            ..Default::default()
        }
    }

    async fn wh05_wait_calls(transport: &RecordingTransport, n: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if transport.calls() >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delivery within 2s");
    }

    async fn wh05_shutdown(service: &LfsService) {
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            service.storage_event_emitter.shutdown(),
        )
        .await
        .expect("emitter shutdown within 3s");
    }

    /// Barrier-gated object store (WH-05 AC4): `exists` waits for both
    /// concurrent checks before answering (bounded), and `put_stream` is
    /// counted with an optional failure injection. Everything else delegates
    /// to the in-memory mock.
    struct GatedStore {
        inner: Arc<dyn crate::orbit_api::factory::MegaObjectStorageWithLog>,
        /// Exists checks that returned "missing" (completed reads only).
        missing_reads: Arc<std::sync::atomic::AtomicUsize>,
        puts: Arc<std::sync::atomic::AtomicUsize>,
        fail_puts: bool,
        /// A put only proceeds once this many exists checks happened —
        /// forces the exists-then-put race open deterministically (WH-05 AC4).
        puts_require_exists: usize,
    }

    impl GatedStore {
        fn new(fail_puts: bool, puts_require_exists: usize) -> Arc<Self> {
            Arc::new(Self {
                inner: crate::jupiter::storage::object_storage::mock_object_storage().inner,
                missing_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                puts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail_puts,
                puts_require_exists,
            })
        }

        fn puts(&self) -> usize {
            self.puts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::orbit_api::object_storage::MegaObjectStorage for GatedStore {
        async fn put_stream(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
            data: crate::orbit_api::object_storage::ObjectByteStream,
            meta: crate::orbit_api::object_storage::ObjectMeta,
        ) -> crate::orbit_api::error::OrbitResult<()> {
            // Hold this write until the required number of exists checks
            // returned "missing" (completed reads), so concurrent uploads all
            // observe a missing object before any of them writes. A timeout
            // fails the upload loudly instead of silently weakening the gate.
            let gate = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if self.missing_reads.load(std::sync::atomic::Ordering::SeqCst)
                        >= self.puts_require_exists
                    {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await;
            if gate.is_err() {
                return Err(crate::orbit_api::error::IoOrbitError::Other(
                    "gated store: put before enough missing reads".to_owned(),
                ));
            }
            self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_puts {
                return Err(crate::orbit_api::error::IoOrbitError::Other(
                    "injected put failure".to_owned(),
                ));
            }
            self.inner.put_stream(key, data, meta).await
        }

        async fn get_stream(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
        ) -> crate::orbit_api::error::OrbitResult<(
            crate::orbit_api::object_storage::ObjectByteStream,
            crate::orbit_api::object_storage::ObjectMeta,
        )> {
            self.inner.get_stream(key).await
        }

        async fn get_range_stream(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
            start: u64,
            end: Option<u64>,
        ) -> crate::orbit_api::error::OrbitResult<(
            crate::orbit_api::object_storage::ObjectByteStream,
            crate::orbit_api::object_storage::ObjectMeta,
        )> {
            self.inner.get_range_stream(key, start, end).await
        }

        async fn exists(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
        ) -> crate::orbit_api::error::OrbitResult<bool> {
            let found = self.inner.exists(key).await?;
            if !found {
                self.missing_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(found)
        }

        async fn signed_url(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
            method: axum::http::Method,
            expiry: std::time::Duration,
        ) -> crate::orbit_api::error::OrbitResult<Option<String>> {
            self.inner.signed_url(key, method, expiry).await
        }

        async fn put_many(
            &self,
            objects: crate::orbit_api::object_storage::MultiObjectByteStream<'_>,
            concurrency: usize,
        ) -> crate::orbit_api::error::OrbitResult<()> {
            self.inner.put_many(objects, concurrency).await
        }

        async fn delete(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
        ) -> crate::orbit_api::error::OrbitResult<()> {
            self.inner.delete(key).await
        }
    }

    #[async_trait::async_trait]
    impl crate::orbit_api::log_storage::LogStorage for GatedStore {
        async fn append(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
            data: crate::orbit_api::object_storage::ObjectByteStream,
            meta: crate::orbit_api::object_storage::ObjectMeta,
        ) -> crate::orbit_api::error::OrbitResult<()> {
            self.inner.append(key, data, meta).await
        }

        async fn read_range(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
            start: u64,
            end: u64,
        ) -> crate::orbit_api::error::OrbitResult<crate::orbit_api::object_storage::ObjectByteStream>
        {
            self.inner.read_range(key, start, end).await
        }

        async fn read_lines_range(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
            start_line: u64,
            end_line: u64,
        ) -> crate::orbit_api::error::OrbitResult<crate::orbit_api::object_storage::ObjectByteStream>
        {
            self.inner.read_lines_range(key, start_line, end_line).await
        }

        async fn append_concurrently(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
            data: crate::orbit_api::object_storage::ObjectByteStream,
            meta: crate::orbit_api::object_storage::ObjectMeta,
        ) -> crate::orbit_api::error::OrbitResult<()> {
            self.inner.append_concurrently(key, data, meta).await
        }

        async fn load_manifest(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
        ) -> crate::orbit_api::error::OrbitResult<crate::orbit_api::log_storage::LogManifest>
        {
            self.inner.load_manifest(key).await
        }

        async fn log_exists(
            &self,
            key: &crate::orbit_api::object_storage::ObjectKey,
        ) -> crate::orbit_api::error::OrbitResult<bool> {
            self.inner.log_exists(key).await
        }
    }

    /// WH-05 service over an arbitrary object store (gated/failing variants).
    async fn wh05_service_with_store(
        transport: Arc<RecordingTransport>,
        targets: Vec<(
            crate::config::StorageEventsTargetConfig,
            crate::jupiter::service::storage_event_transport::EventTarget,
        )>,
        store: Arc<GatedStore>,
    ) -> (tempfile::TempDir, LfsService) {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some(WH05_INSTALLATION.to_owned());
        let emitter =
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
                &config, transport, targets,
            );
        let service = LfsService {
            lfs_storage: storage.lfs_db_storage(),
            obj_storage: crate::orbit_api::factory::MegaObjectStorageWrapper::new(store),
            storage_event_emitter: emitter,
        };
        (temp, service)
    }

    #[tokio::test]
    async fn storage_event_basic_matrix() {
        let transport = Arc::new(RecordingTransport::default());
        let (_temp, service) =
            wh05_service(transport.clone(), vec![wh05_target("ops-main", true)]).await;

        // A real basic upload stores the object and delivers exactly one
        // event with the committed metadata snapshot (scope is null).
        let content = b"wh05 basic content".to_vec();
        let req = wh05_register(&service, &content).await;
        lfs_upload_object(&service, &req, content.clone())
            .await
            .expect("basic upload");
        wh05_wait_calls(&transport, 1).await;
        let bodies = transport.bodies();
        assert_eq!(bodies.len(), 1, "exactly one upload event");
        let envelope: serde_json::Value = serde_json::from_slice(&bodies[0]).expect("envelope");
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["event_type"], "lfs.object.uploaded");
        assert_eq!(envelope["source"], "lfs");
        assert!(envelope["scope"]["tenant_id"].is_null());
        assert!(envelope["scope"]["repo_path"].is_null());
        assert!(envelope["scope"]["oci_repository"].is_null());
        assert_eq!(envelope["data"]["oid"], req.oid);
        assert_eq!(envelope["data"]["size"], req.size);
        assert_eq!(envelope["data"]["transfer"], "basic");
        assert!(envelope["occurred_at"].as_u64().unwrap() > 0);

        // Repeat upload of the same OID hits the exists no-op: no event.
        lfs_upload_object(&service, &req, content.clone())
            .await
            .expect("repeat upload is a no-op");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "exists no-op delivers nothing");

        // Hash mismatch / size mismatch / unknown metadata: rejected before
        // any write, no events.
        let tampered = {
            let mut t = content.clone();
            t[0] ^= 0xff;
            t
        };
        lfs_upload_object(&service, &req, tampered)
            .await
            .expect_err("hash mismatch rejected");
        lfs_upload_object(&service, &req, vec![0u8; 3])
            .await
            .expect_err("size mismatch rejected");
        let ghost = RequestObject {
            oid: hex::encode(Sha256::digest(b"ghost")),
            size: 5,
            ..Default::default()
        };
        lfs_upload_object(&service, &ghost, b"ghost".to_vec())
            .await
            .expect_err("unregistered oid rejected");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "rejections deliver nothing");

        wh05_shutdown(&service).await;
        assert_eq!(transport.calls(), 1, "no late delivery after drain");

        // AC3: the unknown scope reaches only targets that explicitly opted
        // in with `include_unscoped_lfs = true`.
        let scoped_transport = Arc::new(RecordingTransport::default());
        let unscoped_transport = Arc::new(RecordingTransport::default());
        let config = {
            let mut c = crate::config::testing::isolated_config(
                tempfile::tempdir().unwrap().path().join("config"),
            );
            c.storage_events.enabled = true;
            c.storage_events.installation_id = Some(WH05_INSTALLATION.to_owned());
            c
        };
        let emitter =
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
                &config,
                unscoped_transport.clone(),
                vec![wh05_target("ops-main", true)],
            );
        // A second emitter whose only target did NOT opt in.
        let emitter_scoped =
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
                &config,
                scoped_transport.clone(),
                vec![wh05_target("ops-scoped", false)],
            );
        let event = crate::jupiter::service::storage_event::CommittedEvent {
            event_id: uuid::Uuid::new_v4(),
            event_type: crate::jupiter::service::storage_event::EventType::LfsObjectUploaded,
            occurred_at: 1,
            source: crate::jupiter::service::storage_event::EventSource::Lfs,
            scope: crate::jupiter::service::storage_event::EventScope::LfsUnscoped,
            data: crate::jupiter::service::storage_event::EventData::LfsObjectUploaded {
                oid: "ab".to_owned(),
                size: 1,
            },
        };
        assert!(matches!(
            emitter.try_emit(event.clone()),
            crate::jupiter::service::storage_event_emitter::AdmissionDisposition::Accepted { .. }
        ));
        assert!(matches!(
            emitter_scoped.try_emit(event),
            crate::jupiter::service::storage_event_emitter::AdmissionDisposition::DroppedFilter
        ));
        wh05_wait_calls(&unscoped_transport, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(scoped_transport.calls(), 0, "unscoped LFS stays opted out");
        emitter.shutdown().await;
        emitter_scoped.shutdown().await;

        // Batch metadata creation with an enabled emitter delivers nothing
        // (the batch step is not a committed object write).
        let batch_transport = Arc::new(RecordingTransport::default());
        let (_t3, service3) =
            wh05_service(batch_transport.clone(), vec![wh05_target("ops-main", true)]).await;
        let batch_content = b"wh05 batch only".to_vec();
        let batch_oid = hex::encode(Sha256::digest(&batch_content));
        let response = lfs_process_batch(
            &service3,
            BatchRequest {
                operation: Operation::Upload,
                transfers: Vec::new(),
                objects: vec![RequestObject {
                    oid: batch_oid.clone(),
                    size: batch_content.len() as i64,
                    ..Default::default()
                }],
                hash_algo: "sha256".to_owned(),
            },
            "127.0.0.1:0",
        )
        .await
        .expect("batch response");
        assert!(
            !response.objects.is_empty(),
            "batch must register the object"
        );
        // The metadata row exists now, but no upload happened yet: zero events.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(batch_transport.calls(), 0, "batch step delivers nothing");
        wh05_shutdown(&service3).await;
        assert_eq!(batch_transport.calls(), 0, "no delivery after drain");

        // A failed put_stream removes the metadata and delivers nothing.
        let fail_transport = Arc::new(RecordingTransport::default());
        let failing_store = GatedStore::new(true, 1);
        let (_t4, service4) = wh05_service_with_store(
            fail_transport.clone(),
            vec![wh05_target("ops-main", true)],
            failing_store.clone(),
        )
        .await;
        let fail_content = b"wh05 failing put".to_vec();
        let req4 = wh05_register(&service4, &fail_content).await;
        let err = lfs_upload_object(&service4, &req4, fail_content)
            .await
            .expect_err("injected put failure must error the upload");
        assert!(
            err.to_string().contains("not acceptable"),
            "unexpected error: {err}"
        );
        assert_eq!(failing_store.puts(), 1, "the failed put was attempted");
        // The cleanup removed the metadata row: a retry would be a fresh
        // upload, not a hidden duplicate.
        assert!(
            lfs_get_meta(&service4.lfs_storage, &req4.oid)
                .await
                .expect("meta lookup")
                .is_none(),
            "failed put must clean up the metadata row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(fail_transport.calls(), 0, "failed put delivers nothing");
        wh05_shutdown(&service4).await;
        assert_eq!(fail_transport.calls(), 0, "no delivery after drain");

        // Emitter failure never changes the upload result.
        let failing = Arc::new(RecordingTransport {
            fail: true,
            ..Default::default()
        });
        let (_t2, service2) =
            wh05_service(failing.clone(), vec![wh05_target("ops-main", true)]).await;
        let content2 = b"wh05 failing transport".to_vec();
        let req2 = wh05_register(&service2, &content2).await;
        lfs_upload_object(&service2, &req2, content2)
            .await
            .expect("upload succeeds despite emitter failure");
        wh05_wait_calls(&failing, 1).await;
        wh05_shutdown(&service2).await;
        assert_eq!(failing.calls(), 1);
    }

    /// AC4: the exists-then-put race, forced — a barrier-gated object store
    /// makes BOTH uploads' exists checks miss, so both put_stream and both
    /// may notify (no first-write exactly-once is claimed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn storage_event_concurrent_puts() {
        let transport = Arc::new(RecordingTransport::default());
        let gated = GatedStore::new(false, 2);
        let (_temp, service) = wh05_service_with_store(
            transport.clone(),
            vec![wh05_target("ops-main", true)],
            gated.clone(),
        )
        .await;
        let content = b"wh05 concurrent content".to_vec();
        let req = wh05_register(&service, &content).await;

        let first = {
            let service = service.clone();
            let req = RequestObject {
                oid: req.oid.clone(),
                size: req.size,
                ..Default::default()
            };
            let content = content.clone();
            tokio::spawn(async move { lfs_upload_object(&service, &req, content).await })
        };
        let second = {
            let service = service.clone();
            let req = RequestObject {
                oid: req.oid.clone(),
                size: req.size,
                ..Default::default()
            };
            tokio::spawn(async move { lfs_upload_object(&service, &req, content).await })
        };
        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
            tokio::join!(first, second)
        })
        .await
        .expect("both upload tasks join within the bound");
        joined.0.expect("first upload task").expect("first upload");
        joined
            .1
            .expect("second upload task")
            .expect("second upload");

        // The barrier forced both exists checks to miss: both stored.
        assert_eq!(gated.puts(), 2, "both concurrent uploads actually put");
        wh05_shutdown(&service).await;
        let calls = transport.calls();
        assert!(
            (1..=2).contains(&calls),
            "concurrent puts deliver one event per actual put: {calls}"
        );
    }
}
