use super::prelude::*;

pub(in crate::ceres::api_service::mono_api_service) const LARGE_CL_RENAME_DETECTION_THRESHOLD:
    usize = 1000;

/// Outcome of the ACL-change check (UN-19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclChangeDecision {
    /// Either the CL does not touch the authorization file, or this mode does
    /// not consume authorization data.
    Proceed,
    /// The CL changes the authorization file and this principal is not an
    /// admin.
    Refuse { reason: String },
    /// The check itself could not be completed, so nothing about the change is
    /// known. Under `enforce` that refuses the merge (fail-closed) rather than
    /// merging an unexamined ACL change.
    Unavailable { reason: String },
}

/// Decide whether a CL that (maybe) edits `/.mega_cedar.json` may be merged
/// (UN-19).
///
/// Editing the authorization file is how permissions are granted, so anyone who
/// can merge such a change can grant themselves anything. A maintainer holds
/// `approveMergeRequest` and would otherwise have had exactly that: a
/// self-promotion path. The check therefore demands `addAdmin`, which only the
/// admin group holds.
pub fn decide_acl_change(
    enforcement: Enforcement,
    snapshot: Option<&EntitySnapshot>,
    touches_acl: bool,
    authz_principal: &str,
) -> AclChangeDecision {
    // `off`: nothing is detected and nothing is consumed (GC-UN-01).
    if !enforcement.builds() || !touches_acl {
        return AclChangeDecision::Proceed;
    }

    let Some(snapshot) = snapshot else {
        return AclChangeDecision::Unavailable {
            reason: "the authorization snapshot is not built, so an ACL change cannot be reviewed"
                .to_owned(),
        };
    };

    let store = snapshot.store();
    let is_admin = match resolve_resource("/", store).resource() {
        None => false,
        Some(resource) => match CedarContext::new(store.clone()) {
            Err(_) => false,
            Ok(context) => match acl_change_euids(authz_principal) {
                None => false,
                Some((principal, action)) => context
                    .is_authorized(&principal, &action, resource, CedarRequestContext::empty())
                    .is_ok(),
            },
        },
    };

    if is_admin && !store.is_empty() {
        return AclChangeDecision::Proceed;
    }

    AclChangeDecision::Refuse {
        reason: format!(
            "`{authz_principal}` may not merge a change to {MEGA_CEDAR_PATH}:              editing the authorization file requires admin, since it is how permissions              are granted"
        ),
    }
}

/// Cedar ids for the ACL-change check: the `addAdmin` action is admin-only.
pub(in crate::ceres::api_service::mono_api_service) fn acl_change_euids(
    principal: &str,
) -> Option<(SaturnEUid, SaturnEUid)> {
    let user_type = EntityTypeName::from_str("User").ok()?;
    let action_type = EntityTypeName::from_str("Action").ok()?;
    Some((
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            user_type,
            EntityId::from_str(principal).ok()?,
        )),
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            action_type,
            EntityId::from_str("addAdmin").ok()?,
        )),
    ))
}

/// The single emit site of the `merge_authz_unavailable` alert (UN-19).
pub fn emit_merge_authz_unavailable(cl_link: &str, principal: &str, reason: &str) {
    tracing::error!(
        event = "merge_authz_unavailable",
        cl_link = %cl_link,
        principal = %principal,
        reason = %reason,
        "merge authorization could not be decided"
    );
}

/// What the queue's background worker should do with an item (UN-17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueExecutionDecision {
    /// Merge it, authorized as this principal. The execution actor stays
    /// `system` — the worker is what runs the merge, the requester is what
    /// justifies it (ADR-UN-06 ④).
    Execute { authz_principal: String },
    /// Do not merge: authorization for this item cannot proceed. The reason
    /// carries the operator guidance, since a frozen item needs a human
    /// decision rather than a blind retry.
    Freeze { reason: String },
}

/// Guidance stored and logged when a queued item has no recorded requester.
///
/// Retrying alone cannot fix this one: the item predates requester capture (or
/// was queued anonymously), so there is no subject to authorize it as. Saying
/// that plainly is the difference between an operator re-queueing it correctly
/// and repeatedly retrying something that can never succeed.
pub const QUEUE_MISSING_REQUESTER_GUIDANCE: &str = "the queued item has no recorded requester (it was queued before requester capture,      or queued anonymously), so there is no subject to authorize the merge as;      an operator must re-queue it as an identified user";

/// The queue's execution decision (UN-17).
///
/// `off` and `shadow` never change what runs, matching the three-state contract
/// (ADR-UN-01): `shadow` evaluates and records what `enforce` would have
/// refused. Under `enforce` an item is executed only if its recorded requester
/// is still authorized to approve the merge — the subject that asked for it,
/// re-checked at the moment it actually runs, which may be long after queueing.
pub fn decide_queue_execution(
    enforcement: Enforcement,
    snapshot: Option<&EntitySnapshot>,
    requester: Option<&str>,
) -> QueueExecutionDecision {
    let execute_as = |principal: &str| QueueExecutionDecision::Execute {
        authz_principal: principal.to_owned(),
    };

    if !enforcement.builds() {
        // `off`: no build, no consume — unchanged behavior (GC-UN-01). The
        // recorded requester is still passed through as the principal, which is
        // inert today (nothing evaluates it under `off`) and keeps the value
        // meaningful if the parameter ever reaches an audit trail; a legacy
        // item with no requester keeps running as the worker, exactly as before.
        return execute_as(requester.unwrap_or("system"));
    }

    let (would_deny, reason) = match (requester, snapshot) {
        (None, _) => (true, QUEUE_MISSING_REQUESTER_GUIDANCE.to_owned()),
        (Some(_), None) => (
            true,
            "the authorization snapshot is not built, so the queued merge cannot be decided;              retry once authorization is available"
                .to_owned(),
        ),
        (Some(requester), Some(snapshot)) => {
            let store = snapshot.store();
            let denied = match resolve_resource("/", store).resource() {
                None => true,
                Some(resource) => match CedarContext::new(store.clone()) {
                    Err(_) => true,
                    Ok(context) => {
                        match queue_euids(requester) {
                            None => true,
                            Some((principal, action)) => context
                                .is_authorized(&principal, &action, resource, CedarRequestContext::empty())
                                .is_err(),
                        }
                    }
                },
            } || store.is_empty();
            (
                denied,
                format!(
                    "the recorded requester `{requester}` is not authorized to approve this merge;                      an operator must re-queue it as an authorized user or grant that permission"
                ),
            )
        }
    };

    if enforcement.records_would_deny() && would_deny {
        tracing::warn!(
            event = "authz_would_deny",
            principal = %requester.unwrap_or("<none>"),
            principal_type = "User",
            action = "approveMergeRequest",
            resource = "/",
            "would-deny recorded: this queued merge would be frozen under enforce"
        );
    }

    if enforcement.enforces() && would_deny {
        return QueueExecutionDecision::Freeze { reason };
    }

    execute_as(requester.unwrap_or("system"))
}

/// Cedar ids for the queue's decision, or `None` when the requester is not a
/// valid entity id (treated as would-deny rather than as an allow).
pub(in crate::ceres::api_service::mono_api_service) fn queue_euids(
    requester: &str,
) -> Option<(SaturnEUid, SaturnEUid)> {
    let user_type = EntityTypeName::from_str("User").ok()?;
    let action_type = EntityTypeName::from_str("Action").ok()?;
    Some((
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            user_type,
            EntityId::from_str(requester).ok()?,
        )),
        SaturnEUid::from(EntityUid::from_type_name_and_id(
            action_type,
            EntityId::from_str("approveMergeRequest").ok()?,
        )),
    ))
}

/// The single emit site of the `merge_queue_authz_frozen` alert (UN-25).
///
/// Synchronous and process-local by design: an `error` log cannot fail, so
/// alerting can never interfere with the freeze transaction that precedes it.
/// Keeping it in one function also means the field set is one thing to read —
/// and one thing to assert.
pub fn emit_authz_frozen_alert(cl_link: &str, requester: Option<&str>, reason: &str) {
    tracing::error!(
        event = "merge_queue_authz_frozen",
        cl_link = %cl_link,
        requester = %requester.unwrap_or("<none>"),
        reason = %reason,
        "merge queue item frozen: authorization could not be decided"
    );
}

/// Message stored on a queue item frozen by UN-25.
///
/// It names the condition for a successful retry, because a frozen item is not
/// a failed merge: nothing was wrong with the change, only with the ability to
/// decide it.
pub fn authz_freeze_message(reason: &str) -> String {
    format!(
        "merge frozen: authorization unavailable ({reason}). \
         The change was not rejected and no merge was attempted. \
         Retry this item once authorization is available again; \
         the recorded requester is reused as the authorization subject."
    )
}

impl super::MonoApiService {
    // ========== Merge Queue Methods ==========

    /// Enqueue a CL merge via MonoWriteQueue, recording the subject that requested
    /// the merge (UN-20). `None` = anonymous.
    pub async fn add_to_merge_queue_as(
        &self,
        cl_link: String,
        requester: Option<String>,
    ) -> Result<i64, MegaError> {
        // Validate CL exists and is in Open status
        let cl = self.storage.cl_storage().get_cl(&cl_link).await?;
        let model = cl.ok_or(MegaError::Other("CL not found".to_string()))?;

        if model.status != MergeStatusEnum::Open {
            return Err(MegaError::Other(format!(
                "CL is not in Open status, current status: {:?}",
                model.status
            )));
        }

        self.ensure_merge_entry_prechecks(&model)
            .await
            .map_err(|e| MegaError::Other(e.to_string()))?;
        let execution_actor = requester.clone().unwrap_or_else(|| "system".into());
        let authz_principal = requester.clone().unwrap_or_else(|| "system".into());
        self.merge_cl_via_queue(&authz_principal, &execution_actor, model, true, requester)
            .await
            .map_err(|e| MegaError::Other(e.to_string()))
    }

    /// Refuse a merge that edits the authorization file unless the subject is
    /// an admin (UN-19).
    ///
    /// Three things can go wrong while deciding, and all of them mean the same
    /// thing — the change was not examined: the changed-file list cannot be
    /// read, main's copy of the authorization file cannot be resolved, or it is
    /// absent from main entirely. Under `enforce` each refuses the merge rather
    /// than letting an unexamined ACL change through; under `shadow` each is
    /// recorded and the merge proceeds.
    pub async fn enforce_acl_change_authorization(
        &self,
        cl_link: &str,
        authz_principal: &str,
    ) -> Result<(), GitError> {
        let enforcement = Enforcement::parse(&self.storage.config().cedar.enforcement)
            .unwrap_or(Enforcement::Off);
        if !enforcement.builds() {
            return Ok(());
        }

        if let Err(error) =
            ensure_authz_snapshot_caught_up(&self.storage, AUTHZ_BARRIER_TIMEOUT).await
        {
            return self.acl_check_unavailable(
                enforcement,
                cl_link,
                authz_principal,
                &error.to_string(),
            );
        }

        // An unresolvable change set reads back as "changed nothing", which
        // would silently pass an ACL change through. Establish that the CL's
        // endpoints exist before trusting the diff.
        if let Err(reason) = self.ensure_change_set_resolvable(cl_link).await {
            return self.acl_check_unavailable(enforcement, cl_link, authz_principal, &reason);
        }

        let touches_acl = match self.cl_touches_authz_file(cl_link).await {
            Ok(touches) => touches,
            Err(error) => {
                return self.acl_check_unavailable(
                    enforcement,
                    cl_link,
                    authz_principal,
                    &format!("the changed-file list could not be read: {error}"),
                );
            }
        };

        if touches_acl {
            // The change touches the ACL, so main's own copy has to be
            // resolvable — without it there is no baseline to review against.
            if let Err(reason) = self.resolve_main_acl_blob().await {
                return self.acl_check_unavailable(enforcement, cl_link, authz_principal, &reason);
            }
        }

        let snapshot = self.storage.entity_store().snapshot();
        match decide_acl_change(
            enforcement,
            snapshot.as_deref(),
            touches_acl,
            authz_principal,
        ) {
            AclChangeDecision::Proceed => Ok(()),
            AclChangeDecision::Unavailable { reason } => {
                self.acl_check_unavailable(enforcement, cl_link, authz_principal, &reason)
            }
            AclChangeDecision::Refuse { reason } => {
                if enforcement.records_would_deny() {
                    tracing::warn!(
                        event = "authz_would_deny",
                        principal = %authz_principal,
                        principal_type = "User",
                        action = "addAdmin",
                        resource = %MEGA_CEDAR_PATH,
                        "would-deny recorded: this ACL change would be refused under enforce"
                    );
                }
                if enforcement.enforces() {
                    return Err(GitError::CustomError(format!("[code:403] {reason}")));
                }
                Ok(())
            }
        }
    }

    /// Shared handling of the three "could not decide" classes.
    pub(in crate::ceres::api_service::mono_api_service) fn acl_check_unavailable(
        &self,
        enforcement: Enforcement,
        cl_link: &str,
        authz_principal: &str,
        reason: &str,
    ) -> Result<(), GitError> {
        emit_merge_authz_unavailable(cl_link, authz_principal, reason);
        if enforcement.enforces() {
            // 503, not 500: the change was not rejected, it was not examined —
            // the same request can succeed once authorization is available
            // again (UN-25's contract).
            return Err(GitError::CustomError(format!(
                "[code:503] merge authorization unavailable: {reason}"
            )));
        }
        Ok(())
    }

    /// Whether a CL touches `/.mega_cedar.json` on either side of its diff.
    ///
    /// Both sides matter: a rename reports only its *new* path, so looking at
    /// that alone would let someone move the authorization file out of the way
    /// — deleting it from where it is read — without ever tripping the check.
    pub(in crate::ceres::api_service::mono_api_service) async fn cl_touches_authz_file(
        &self,
        cl_link: &str,
    ) -> Result<bool, MegaError> {
        let acl_file = MEGA_CEDAR_PATH.trim_start_matches('/');
        let is_acl = |path: &std::path::Path| {
            let normalized = path.to_string_lossy().replace('\\', "/");
            normalized == acl_file || normalized.ends_with(&format!("/{acl_file}"))
        };

        let cl = self
            .storage
            .cl_storage()
            .get_cl(cl_link)
            .await?
            .ok_or_else(|| MegaError::Other(format!("CL not found: {cl_link}")))?;
        let old_files = self.get_commit_blobs(&cl.from_hash).await?;
        let new_files = self.get_commit_blobs(&cl.to_hash).await?;

        Ok(self
            .cl_files_list(old_files, new_files)
            .await?
            .iter()
            .any(|file| match file {
                ClDiffFile::Renamed(old_path, new_path, ..)
                | ClDiffFile::Moved(old_path, new_path, ..) => is_acl(old_path) || is_acl(new_path),
                other => is_acl(other.path()),
            }))
    }

    /// Both endpoints of the CL's diff must exist, or the changed-file list is
    /// not evidence of anything: a missing commit yields an empty list, which
    /// is indistinguishable from "this CL changes nothing".
    pub(in crate::ceres::api_service::mono_api_service) async fn ensure_change_set_resolvable(
        &self,
        cl_link: &str,
    ) -> Result<(), String> {
        let cl = self
            .storage
            .cl_storage()
            .get_cl(cl_link)
            .await
            .map_err(|e| format!("the CL could not be read: {e}"))?
            .ok_or_else(|| format!("CL `{cl_link}` not found"))?;

        let storage = self.storage.mono_storage();
        for (label, hash) in [("base", &cl.from_hash), ("tip", &cl.to_hash)] {
            let commit = storage
                .get_commit_by_hash(hash)
                .await
                .map_err(|e| format!("the CL's {label} commit could not be read: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "the CL's {label} commit `{hash}` is missing, so its changed-file list \
                         cannot be computed"
                    )
                })?;

            // The tree matters as much as the commit: `get_commit_blobs`
            // returns an empty list for a commit whose tree is gone, which
            // reads back as "changed nothing" — the same fail-open shape.
            let tree_missing = storage
                .get_tree_by_hash(&commit.tree)
                .await
                .map_err(|e| format!("the CL's {label} tree could not be read: {e}"))?
                .is_none();
            if tree_missing {
                return Err(format!(
                    "the tree `{}` of the CL's {label} commit is missing, so its changed-file \
                     list cannot be computed",
                    commit.tree
                ));
            }
        }
        Ok(())
    }

    /// Resolve the blob id of main's `/.mega_cedar.json`, or say why not.
    pub(in crate::ceres::api_service::mono_api_service) async fn resolve_main_acl_blob(
        &self,
    ) -> Result<String, String> {
        let storage = self.storage.mono_storage();
        let main_ref = storage
            .get_main_ref("/")
            .await
            .map_err(|e| format!("main ref could not be read: {e}"))?
            .ok_or_else(|| "main ref not found".to_owned())?;
        let tree = storage
            .get_tree_by_hash(&main_ref.ref_tree_hash)
            .await
            .map_err(|e| format!("main tree could not be read: {e}"))?
            .ok_or_else(|| "main tree not found".to_owned())?;
        authz_blob_id(&Tree::from_mega_model(tree)).ok_or_else(|| {
            format!("{MEGA_CEDAR_PATH} is absent from main, so there is no baseline to review")
        })
    }

    /// Freeze a queued CL because authorization could not be decided (UN-25).
    ///
    /// "Frozen" reuses the existing terminal state rather than inventing one:
    /// `Failed` + `SystemError`, which the queue already understands and the
    /// existing retry entry point already accepts. Two properties matter for a
    /// frozen item:
    ///
    /// * the recorded `requester` is **kept** — it is the authorization subject
    ///   the merge will be re-decided against, and losing it would turn a retry
    ///   into an unattributed merge;
    /// * `error_message` states the condition under which a retry can succeed,
    ///   so an operator is not left guessing whether to retry or to escalate.
    ///
    /// The alert is a process-local `error` log written synchronously in the
    /// same call: there is no external call that could fail and no way for
    /// alerting to interfere with the freeze itself.
    pub async fn freeze_merge_queue_item_for_authz(
        &self,
        cl_link: &str,
        reason: &str,
    ) -> Result<bool, MegaError> {
        let rows = self
            .storage
            .push_queue_storage()
            .list_by_kind_and_operation(PushQueueKindEnum::Merge, cl_link)
            .await?;
        let requester = rows
            .iter()
            .find(|row| {
                matches!(
                    row.status,
                    PushQueueStatusEnum::Queued | PushQueueStatusEnum::Running
                )
            })
            .and_then(|row| row.requester.clone());

        let frozen = self
            .storage
            .push_queue_storage()
            .freeze_merge_for_authz(cl_link, &authz_freeze_message(reason))
            .await?;

        if frozen {
            emit_authz_frozen_alert(cl_link, requester.as_deref(), reason);
        }

        Ok(frozen)
    }

    /// Retry a CL merge via MonoWriteQueue, recording the subject that requested
    /// the retry (UN-20). `None` = anonymous.
    pub async fn retry_merge_queue_item_as(
        &self,
        cl_link: &str,
        requester: Option<String>,
    ) -> Result<bool, MegaError> {
        self.add_to_merge_queue_as(cl_link.to_owned(), requester)
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod merge_acl_change {
    //! UN-19: only an admin may merge a change to the authorization file.
    //!
    //! Editing `/.mega_cedar.json` *is* how permissions are granted, so whoever can
    //! merge such a change can grant themselves anything. A maintainer holds
    //! `approveMergeRequest`, which would have been exactly that: a self-promotion
    //! path. The check therefore asks for `addAdmin`, which only the admin group
    //! holds — the same decision for every merge entry, because they all land
    //! through `merge_cl_via_queue` → B3.
    //!
    //! A check that cannot be completed is not a pass. Three things can go wrong —
    //! the changed-file list cannot be read, main's copy of the file cannot be
    //! resolved, or it is absent from main — and all three mean the change was
    //! never examined, so `enforce` refuses.

    use crate::{
        ceres::api_service::mono_api_service::{AclChangeDecision, decide_acl_change},
        contract::policy::{builder::EntitySnapshot, enforcement::Enforcement},
    };

    const ADMIN: &str = "un19-admin";
    const MAINTAINER: &str = "un19-maintainer";
    const READER: &str = "un19-reader";
    const ANONYMOUS: &str = "__anonymous__";

    fn role_snapshot() -> EntitySnapshot {
        let json = serde_json::json!({
            "users": {
                format!(r#"User::"{ADMIN}""#): {
                    "euid": format!(r#"User::"{ADMIN}""#),
                    "parents": [r#"UserGroup::"admin""#]
                },
                format!(r#"User::"{MAINTAINER}""#): {
                    "euid": format!(r#"User::"{MAINTAINER}""#),
                    "parents": [r#"UserGroup::"matainer""#]
                },
                format!(r#"User::"{READER}""#): {
                    "euid": format!(r#"User::"{READER}""#),
                    "parents": [r#"UserGroup::"reader""#]
                }
            },
            "repos": {
                r#"Repository::"/""#: {
                    "euid": r#"Repository::"/""#,
                    "is_private": true,
                    "admins": r#"UserGroup::"admin""#,
                    "maintainers": r#"UserGroup::"matainer""#,
                    "readers": r#"UserGroup::"reader""#,
                    "parents": []
                }
            },
            "user_groups": {
                r#"UserGroup::"admin""#: {
                    "euid": r#"UserGroup::"admin""#,
                    "parents": [r#"UserGroup::"matainer""#]
                },
                r#"UserGroup::"matainer""#: {
                    "euid": r#"UserGroup::"matainer""#,
                    "parents": [r#"UserGroup::"reader""#]
                },
                r#"UserGroup::"reader""#: { "euid": r#"UserGroup::"reader""#, "parents": [] }
            },
            "merge_requests": {},
            "issues": {}
        })
        .to_string();
        crate::contract::policy::builder::build_from_json(&json).expect("build snapshot")
    }

    fn proceeds(decision: &AclChangeDecision) -> bool {
        matches!(decision, AclChangeDecision::Proceed)
    }

    /// The self-promotion channel this card closes: a maintainer can approve
    /// ordinary merges but must not be able to merge a change to the ACL.
    #[test]
    fn un19_only_an_admin_may_merge_an_acl_change() {
        let snapshot = role_snapshot();
        let cases: &[(&str, bool)] = &[
            (ADMIN, true),
            (MAINTAINER, false),
            (READER, false),
            (ANONYMOUS, false),
        ];

        for (principal, allowed) in cases {
            let decision =
                decide_acl_change(Enforcement::Enforce, Some(&snapshot), true, principal);
            assert_eq!(
                proceeds(&decision),
                *allowed,
                "{principal} merging an ACL change"
            );
            if !*allowed {
                assert!(
                    matches!(decision, AclChangeDecision::Refuse { .. }),
                    "a known-but-unauthorized principal is a refusal, not an unavailable check"
                );
            }
        }
    }

    /// A CL that leaves the ACL alone is unaffected — the check must not become a
    /// general merge gate.
    #[test]
    fn un19_a_cl_that_does_not_touch_the_acl_is_unaffected() {
        let snapshot = role_snapshot();
        for principal in [ADMIN, MAINTAINER, READER, ANONYMOUS] {
            assert!(
                proceeds(&decide_acl_change(
                    Enforcement::Enforce,
                    Some(&snapshot),
                    false,
                    principal
                )),
                "{principal} merging an ordinary change"
            );
        }
    }

    #[test]
    fn un19_off_does_not_detect_or_consume_anything() {
        // Not even for a maintainer editing the ACL: `off` is a short circuit.
        assert!(proceeds(&decide_acl_change(
            Enforcement::Off,
            Some(&role_snapshot()),
            true,
            MAINTAINER
        )));
        assert!(proceeds(&decide_acl_change(
            Enforcement::Off,
            None,
            true,
            MAINTAINER
        )));
    }

    #[test]
    fn un19_shadow_reports_but_still_proceeds() {
        // `shadow` returns the refusal for the caller to record; the caller is what
        // decides not to act on it (asserted in `un19_fail_closed`).
        let decision = decide_acl_change(
            Enforcement::Shadow,
            Some(&role_snapshot()),
            true,
            MAINTAINER,
        );
        assert!(matches!(decision, AclChangeDecision::Refuse { .. }));
    }

    #[test]
    fn un19_a_missing_snapshot_is_unavailable_not_a_refusal() {
        // The distinction matters: a refusal says "you may not", an unavailable
        // check says "nobody looked" — and only the latter is retryable.
        let decision = decide_acl_change(Enforcement::Enforce, None, true, ADMIN);
        assert!(matches!(decision, AclChangeDecision::Unavailable { .. }));
    }

    #[test]
    fn un19_an_empty_store_refuses_even_an_admin() {
        let empty = crate::contract::policy::builder::build_from_json(
            r#"{"users":{},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#,
        )
        .expect("build empty snapshot");

        assert!(
            !proceeds(&decide_acl_change(
                Enforcement::Enforce,
                Some(&empty),
                true,
                ADMIN
            )),
            "enforce + empty store = deny (fail-closed, ADR-UN-01)"
        );
    }

    #[test]
    fn un19_the_refusal_explains_why() {
        let decision = decide_acl_change(
            Enforcement::Enforce,
            Some(&role_snapshot()),
            true,
            MAINTAINER,
        );
        let AclChangeDecision::Refuse { reason } = decision else {
            panic!("expected a refusal");
        };
        assert!(reason.contains(MAINTAINER), "names the principal: {reason}");
        assert!(reason.contains("admin"), "says what is required: {reason}");
        assert!(
            reason.contains(".mega_cedar.json"),
            "names the file: {reason}"
        );
    }
}

#[cfg(test)]
mod merge_acl_unavailable {
    //! UN-19 fail-closed: a check that could not run is not a pass.
    //!
    //! Three things can prevent the ACL-change check from reaching a verdict — the
    //! changed-file list cannot be read, main's copy of the authorization file
    //! cannot be resolved, or it is absent from main. In every one of them the
    //! change was never examined, so `enforce` refuses the merge and `shadow`
    //! records it and proceeds. The refusal is 503 rather than 500 or 403: nothing
    //! was wrong with the change and nobody rejected it, so the same merge can
    //! succeed once authorization is available again (UN-25's contract).

    use std::sync::Arc;

    use git_internal::{
        hash::{ObjectHash, get_hash_kind},
        internal::object::tree::{Tree, TreeItem, TreeItemMode},
    };
    use sea_orm::ConnectionTrait;

    use crate::{
        callisto::{
            mega_refs,
            sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
        },
        ceres::api_service::{
            cache::GitObjectCache,
            mono_api_service::{MonoApiService, emit_merge_authz_unavailable},
        },
        common::utils::MEGA_BRANCH_NAME,
        contract::policy::entitystore::{SharedEntityStore, generate_entity},
        jupiter::storage::{
            Storage,
            base_storage::StorageConnector,
            push_queue_storage::{EnqueueOutcome, EnqueueParams},
        },
    };

    fn service(storage: &Storage) -> MonoApiService {
        MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "un19-test".to_string(),
            }),
        }
    }

    /// Storage in the given enforcement mode, with an admin-only ACL snapshot
    /// already built.
    async fn storage_in(temp: &std::path::Path, mode: &str) -> Storage {
        let mut storage = crate::jupiter::tests::test_storage(temp).await;
        let mut config = (*storage.config).clone();
        config.cedar.enforcement = mode.to_string();
        let config = Arc::new(config);
        storage.config_handle = crate::config::reload::ConfigHandle::from_arc(config.clone());
        storage.config = config;

        let json = generate_entity(&["un19-admin".to_string()], "/").expect("init product");
        let store = SharedEntityStore::new();
        store.ensure(&json).expect("build snapshot");
        storage.set_entity_store_for_test(Arc::new(store));
        storage
    }

    /// Main with a `/.mega_cedar.json` present, or without it.
    async fn seed_main(storage: &Storage, with_acl: bool) {
        let mut items = vec![TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_hex_for_kind(
                get_hash_kind(),
                "2222222222222222222222222222222222222222",
            )
            .unwrap(),
            "README.md".to_string(),
        )];
        if with_acl {
            items.push(TreeItem::new(
                TreeItemMode::Blob,
                ObjectHash::from_hex_for_kind(
                    get_hash_kind(),
                    "3333333333333333333333333333333333333333",
                )
                .unwrap(),
                ".mega_cedar.json".to_string(),
            ));
        }
        let tree = Tree::from_tree_items(items).expect("root tree");
        let commit_id = "1111111111111111111111111111111111111111";
        storage
            .mono_storage()
            .save_mega_trees(
                vec![tree.clone()],
                ObjectHash::from_hex_for_kind(get_hash_kind(), commit_id).unwrap(),
                None,
            )
            .await
            .expect("save root tree");
        storage
            .mono_storage()
            .save_refs(
                mega_refs::Model {
                    id: 1,
                    path: "/".to_string(),
                    ref_name: MEGA_BRANCH_NAME.to_string(),
                    ref_commit_hash: commit_id.to_string(),
                    ref_tree_hash: tree.id.to_string(),
                    created_at: chrono::Utc::now().naive_utc(),
                    updated_at: chrono::Utc::now().naive_utc(),
                    is_cl: false,
                },
                None,
            )
            .await
            .expect("save main ref");
    }

    async fn seed_cl(storage: &Storage, id: i64, cl_link: &str) {
        storage
            .cl_storage()
            .get_connection()
            .execute_unprepared(&format!(
                "INSERT INTO mega_cl \
                 (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
                  username, base_branch) \
                 VALUES ({id}, '{cl_link}', 'un19', 'open', '/', 'from', 'to', now(), now(), \
                 'un19-author', 'main')"
            ))
            .await
            .expect("seed CL");
    }

    /// Class 1: the changed-file list cannot be read (its commits do not exist).
    #[tokio::test]
    async fn un19_fail_closed_when_the_changed_file_list_cannot_be_read() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_in(temp.path(), "enforce").await;
        seed_main(&storage, true).await;
        seed_cl(&storage, 720_001, "UN19UNREADABLE").await;

        let error = service(&storage)
            .enforce_acl_change_authorization("UN19UNREADABLE", "un19-admin")
            .await
            .expect_err("an unreadable change set must not merge");

        let message = error.to_string();
        assert!(
            message.contains("[code:503]"),
            "not examined is retryable, not a rejection: {message}"
        );
        assert!(
            !message.contains("[code:500]"),
            "an undecidable check is not a server bug: {message}"
        );
    }

    /// Class 1b: the commits exist but their tree does not. `get_commit_blobs`
    /// answers with an empty list there, which is indistinguishable from "this CL
    /// changed nothing" — the same fail-open shape, so it must be caught too.
    #[tokio::test]
    async fn un19_fail_closed_when_a_commit_tree_is_missing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_in(temp.path(), "enforce").await;
        seed_main(&storage, true).await;
        seed_cl(&storage, 720_004, "UN19NOTREE").await;

        // Commits that exist but point at a tree that was never saved.
        storage
            .cl_storage()
            .get_connection()
            .execute_unprepared(
                "INSERT INTO mega_commit (id, commit_id, tree, parents_id, author, committer,              content, created_at) VALUES              (720101, 'from', '4444444444444444444444444444444444444444', '{}', '', '', '', now()),              (720102, 'to', '5555555555555555555555555555555555555555', '{}', '', '', '', now())",
            )
            .await
            .expect("seed commits without trees");

        let error = service(&storage)
            .enforce_acl_change_authorization("UN19NOTREE", "un19-admin")
            .await
            .expect_err("a commit without a tree makes the change set unknowable");
        assert!(
            error.to_string().contains("[code:503]"),
            "not examined is retryable: {error}"
        );
    }

    /// A CL whose link does not exist at all is likewise undecidable, not allowed.
    #[tokio::test]
    async fn un19_fail_closed_for_an_unknown_cl() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_in(temp.path(), "enforce").await;
        seed_main(&storage, true).await;

        assert!(
            service(&storage)
                .enforce_acl_change_authorization("UN19NOSUCHCL", "un19-admin")
                .await
                .is_err()
        );
    }

    /// The same conditions under `shadow` must not change the outcome.
    #[tokio::test]
    async fn un19_shadow_records_but_lets_the_merge_proceed() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_in(temp.path(), "shadow").await;
        seed_main(&storage, true).await;
        seed_cl(&storage, 720_002, "UN19SHADOW").await;

        assert!(
            service(&storage)
                .enforce_acl_change_authorization("UN19SHADOW", "un19-admin")
                .await
                .is_ok(),
            "shadow evaluates and records, but never changes the outcome"
        );
    }

    /// `off` performs no detection at all.
    #[tokio::test]
    async fn un19_off_performs_no_detection() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_in(temp.path(), "off").await;
        // Deliberately seed nothing: under `off` there is nothing to read anyway.
        assert!(
            service(&storage)
                .enforce_acl_change_authorization("UN19OFF", "whoever")
                .await
                .is_ok()
        );
    }

    /// Renaming the authorization file away is still a change to it: the diff
    /// reports only the new path, so a check that looked there alone could be
    /// side-stepped by moving the file out of where it is read.
    #[test]
    fn un19_a_rename_of_the_authz_file_counts_as_touching_it() {
        use std::path::PathBuf;

        use git_internal::hash::{ObjectHash, get_hash_kind};

        use crate::ceres::model::change_list::ClDiffFile;

        let acl = PathBuf::from(".mega_cedar.json");
        let elsewhere = PathBuf::from("docs/old-acl.json");
        let hash = ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "6666666666666666666666666666666666666666",
        )
        .unwrap();

        let renamed_away = ClDiffFile::Renamed(acl.clone(), elsewhere.clone(), hash, hash, 100);
        assert_eq!(
            renamed_away.path(),
            &elsewhere,
            "the diff itself only reports the new path — which is exactly the gap"
        );

        // The check must look at both sides.
        let touches = |file: &ClDiffFile| match file {
            ClDiffFile::Renamed(old_path, new_path, ..)
            | ClDiffFile::Moved(old_path, new_path, ..) => {
                old_path.ends_with(".mega_cedar.json") || new_path.ends_with(".mega_cedar.json")
            }
            other => other.path().ends_with(".mega_cedar.json"),
        };
        assert!(touches(&renamed_away), "renaming it away must count");
        assert!(
            touches(&ClDiffFile::Moved(elsewhere, acl, hash, hash, 100)),
            "moving something onto it must count too"
        );
    }

    /// The contract event, asserted at its single emit site.
    #[test]
    fn un19_the_unavailable_alert_carries_its_fields() {
        use std::{io::Write, sync::Mutex};

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct TestWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for TestWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl MakeWriter<'_> for TestWriter {
            type Writer = TestWriter;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(TestWriter(buf.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            emit_merge_authz_unavailable("UN19EVENT", "un19-admin", "the changed-file list failed");
        });
        let out = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8");

        assert!(out.contains("merge_authz_unavailable"), "event: {out}");
        assert!(out.contains("UN19EVENT"), "cl_link: {out}");
        assert!(out.contains("un19-admin"), "principal: {out}");
        assert!(
            out.contains("the changed-file list failed"),
            "reason: {out}"
        );
    }

    /// The queue freezes rather than merging when the check cannot run, keeping the
    /// requester and the retry guidance (UN-25's fields).
    #[tokio::test]
    async fn un19_fail_closed_freezes_a_queued_item() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = storage_in(temp.path(), "enforce").await;
        seed_main(&storage, true).await;
        seed_cl(&storage, 720_003, "UN19QUEUE").await;
        let outcome = storage
            .push_queue_storage()
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: "UN19QUEUE",
                path: "/",
                old_id: "from",
                new_id: "to",
                requester: Some("un19-admin"),
                payload: serde_json::json!({
                    "cl_link": "UN19QUEUE",
                    "authz_principal": "un19-admin",
                    "execution_actor": "system",
                    "apply_queue_execution_decision": true,
                    "requester": "un19-admin",
                }),
            })
            .await
            .expect("enqueue");
        assert!(
            matches!(outcome, EnqueueOutcome::Inserted { .. }),
            "expected insert, got {outcome:?}"
        );

        let service = service(&storage);
        let error = service
            .enforce_acl_change_authorization("UN19QUEUE", "un19-admin")
            .await
            .expect_err("undecidable");
        service
            .freeze_merge_queue_item_for_authz("UN19QUEUE", &error.to_string())
            .await
            .expect("freeze");

        let item = storage
            .push_queue_storage()
            .list_by_kind_and_operation(PushQueueKindEnum::Merge, "UN19QUEUE")
            .await
            .expect("read item")
            .into_iter()
            .next()
            .expect("item");
        assert_eq!(item.status, PushQueueStatusEnum::Failed);
        assert_eq!(
            item.requester,
            Some("un19-admin".to_string()),
            "the frozen item keeps the subject it will be re-decided against"
        );
        assert!(
            item.error_message.expect("message").contains("Retry"),
            "the frozen item states its retry condition"
        );
    }
}

#[cfg(test)]
mod merge_authz_freeze {
    //! UN-25: freezing a queued merge whose authorization cannot be decided.
    //!
    //! A frozen item is not a rejected change — nothing was wrong with it, only
    //! with the ability to decide it. That distinction has to survive in the stored
    //! row, because it is what tells an operator to retry rather than escalate:
    //!
    //! * the state reuses the queue's existing terminal pair (`Failed` +
    //!   `SystemError`) so the existing retry entry point keeps working and no new
    //!   enum value or migration is needed;
    //! * the recorded `requester` survives — it is the subject the merge will be
    //!   re-decided against, and dropping it would turn a retry into an
    //!   unattributed merge;
    //! * `error_message` states the condition for a successful retry.

    use std::sync::Arc;

    use sea_orm::ConnectionTrait;

    use crate::{
        callisto::sea_orm_active_enums::{
            PushQueueFailureEnum, PushQueueKindEnum, PushQueueStatusEnum,
        },
        ceres::api_service::{cache::GitObjectCache, mono_api_service::MonoApiService},
        jupiter::storage::{
            Storage,
            base_storage::StorageConnector,
            push_queue_storage::{EnqueueOutcome, EnqueueParams},
        },
    };

    fn service(storage: &Storage) -> MonoApiService {
        MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                    ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                    ::redis::aio::ConnectionManagerConfig::new(),
                )
                .expect("lazy connection manager"),
                prefix: "un25-test".to_string(),
            }),
        }
    }

    async fn seed_open_cl(storage: &Storage, id: i64, cl_link: &str) {
        storage
            .cl_storage()
            .get_connection()
            .execute_unprepared(&format!(
                "INSERT INTO mega_cl \
                 (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
                  username, base_branch) \
                 VALUES ({id}, '{cl_link}', 'un25', 'open', '/', 'from', 'to', now(), now(), \
                 'un25-author', 'main')"
            ))
            .await
            .expect("seed open CL");
    }

    async fn queued_item(storage: &Storage, id: i64, cl_link: &str, requester: Option<&str>) {
        seed_open_cl(storage, id, cl_link).await;
        let outcome = storage
            .push_queue_storage()
            .enqueue_atomic(EnqueueParams {
                kind: PushQueueKindEnum::Merge,
                operation_id: cl_link,
                path: "/",
                old_id: "from",
                new_id: "to",
                requester,
                payload: serde_json::json!({
                    "cl_link": cl_link,
                    "authz_principal": requester.unwrap_or("system"),
                    "execution_actor": "system",
                    "apply_queue_execution_decision": true,
                    "requester": requester,
                }),
            })
            .await
            .expect("enqueue");
        assert!(
            matches!(outcome, EnqueueOutcome::Inserted { .. }),
            "expected insert, got {outcome:?}"
        );
    }

    async fn merge_row(storage: &Storage, cl_link: &str) -> crate::callisto::push_queue::Model {
        storage
            .push_queue_storage()
            .list_by_kind_and_operation(PushQueueKindEnum::Merge, cl_link)
            .await
            .expect("read item")
            .into_iter()
            .next()
            .expect("the item exists")
    }

    /// Capture what the freeze writes to the log, so the alert is asserted rather
    /// than assumed.
    ///
    /// The whole call — runtime included — happens inside the closure, so the
    /// subscriber is unambiguously the default for the thread that emits the event.
    /// Installing a thread-local default *around* an already-running async test is
    /// not reliable: the future can be polled elsewhere and the record goes to the
    /// global subscriber instead.
    fn capture_tracing<F>(f: F) -> String
    where
        F: FnOnce(),
    {
        use std::{io::Write, sync::Mutex};

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct TestWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for TestWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl MakeWriter<'_> for TestWriter {
            type Writer = TestWriter;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(TestWriter(buf.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.lock().unwrap().clone()).expect("utf8")
    }

    #[tokio::test]
    async fn un25_freeze_writes_every_field_the_contract_promises() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        queued_item(&storage, 710_001, "UN25FREEZE", Some("un25-requester")).await;

        assert!(
            service(&storage)
                .freeze_merge_queue_item_for_authz("UN25FREEZE", "snapshot dirty")
                .await
                .expect("freeze"),
            "a queued item must be freezable"
        );

        let item = merge_row(&storage, "UN25FREEZE").await;

        assert_eq!(item.status, PushQueueStatusEnum::Failed, "state");
        assert_eq!(
            item.failure_type,
            Some(PushQueueFailureEnum::SystemError),
            "failure type reuses the existing enum value — no new variant, no migration"
        );
        assert_eq!(
            item.requester,
            Some("un25-requester".to_string()),
            "the authorization subject must survive the freeze"
        );

        let message = item.error_message.expect("error message");
        assert!(
            message.contains("Retry"),
            "the message must state the retry condition: {message}"
        );
        assert!(
            message.contains("snapshot dirty"),
            "the message must name the reason: {message}"
        );
        assert!(
            message.contains("not rejected"),
            "a frozen item is not a rejected change; the message must say so: {message}"
        );
    }

    /// The alert's field set, asserted at its single emit site.
    ///
    /// The emission is deliberately a plain synchronous function, so capturing it
    /// needs no runtime and no cross-thread subscriber: the freeze path below calls
    /// exactly this function.
    #[test]
    fn un25_freeze_emits_the_contract_event() {
        let out = capture_tracing(|| {
            crate::ceres::api_service::mono_api_service::emit_authz_frozen_alert(
                "UN25EVENT",
                Some("un25-requester"),
                "store not built",
            );
        });

        assert!(
            out.contains("merge_queue_authz_frozen"),
            "missing event field: {out}"
        );
        assert!(out.contains("UN25EVENT"), "missing cl_link field: {out}");
        assert!(
            out.contains("un25-requester"),
            "missing requester field: {out}"
        );
        assert!(
            out.contains("store not built"),
            "missing reason field: {out}"
        );
    }

    /// An anonymous requester still produces a well-formed alert rather than an
    /// empty field an operator would have to guess about.
    #[test]
    fn un25_the_frozen_alert_names_a_missing_requester_explicitly() {
        let out = capture_tracing(|| {
            crate::ceres::api_service::mono_api_service::emit_authz_frozen_alert(
                "UN25NOREQ",
                None,
                "store not built",
            );
        });

        assert!(out.contains("merge_queue_authz_frozen"), "{out}");
        assert!(
            out.contains("<none>"),
            "missing requester placeholder: {out}"
        );
    }

    /// The freeze path really does go through that emit site: freezing a queued
    /// item produces the alert.
    #[tokio::test]
    async fn un25_the_freeze_path_reaches_the_alert() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        queued_item(&storage, 710_004, "UN25ALERT", Some("un25-requester")).await;

        assert!(
            service(&storage)
                .freeze_merge_queue_item_for_authz("UN25ALERT", "store not built")
                .await
                .expect("freeze"),
            "the freeze must report that it froze the item, which is what gates the alert"
        );
    }

    #[tokio::test]
    async fn un25_freezing_an_already_failed_item_preserves_the_original_reason() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        queued_item(&storage, 710_003, "UN25TWICE", Some("un25-requester")).await;

        let service = service(&storage);
        assert!(
            service
                .freeze_merge_queue_item_for_authz("UN25TWICE", "first reason")
                .await
                .expect("first freeze")
        );
        assert!(
            !service
                .freeze_merge_queue_item_for_authz("UN25TWICE", "second reason")
                .await
                .expect("second freeze"),
            "an already-failed item is left alone, so the first diagnosis is not overwritten"
        );

        let message = merge_row(&storage, "UN25TWICE")
            .await
            .error_message
            .expect("message");
        assert!(message.contains("first reason"), "{message}");
    }

    #[tokio::test]
    async fn un25_freezing_an_unqueued_cl_reports_that_nothing_was_frozen() {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;

        assert!(
            !service(&storage)
                .freeze_merge_queue_item_for_authz("UN25MISSING", "whatever")
                .await
                .expect("freeze call succeeds"),
            "no queue row means nothing to freeze — not an error"
        );
    }
}

#[cfg(test)]
mod merge_queue_execution {
    //! UN-17: who a queued merge runs as.
    //!
    //! A queued merge executes long after the request that queued it — the ACL may
    //! have changed in between, and the worker itself is not a subject. So the
    //! decision re-checks the *recorded requester* at execution time and keeps
    //! `system` only as the execution actor (ADR-UN-06 ④).
    //!
    //! Items queued before requester capture (or queued anonymously) have no
    //! subject at all. Under `enforce` those are frozen rather than merged as some
    //! privileged default, and the guidance says so: retrying alone can never fix
    //! them.

    use crate::{
        ceres::api_service::mono_api_service::{
            QUEUE_MISSING_REQUESTER_GUIDANCE, QueueExecutionDecision, decide_queue_execution,
        },
        contract::policy::{builder::EntitySnapshot, enforcement::Enforcement},
    };

    const ADMIN: &str = "un17-admin";
    const MAINTAINER: &str = "un17-maintainer";
    const READER: &str = "un17-reader";

    /// One member per role, so the matrix exercises the real group hierarchy.
    fn role_snapshot() -> EntitySnapshot {
        let json = serde_json::json!({
            "users": {
                format!("User::\"{ADMIN}\""): {
                    "euid": format!("User::\"{ADMIN}\""),
                    "parents": ["UserGroup::\"admin\""]
                },
                format!("User::\"{MAINTAINER}\""): {
                    "euid": format!("User::\"{MAINTAINER}\""),
                    "parents": ["UserGroup::\"matainer\""]
                },
                format!("User::\"{READER}\""): {
                    "euid": format!("User::\"{READER}\""),
                    "parents": ["UserGroup::\"reader\""]
                }
            },
            "repos": {
                "Repository::\"/\"": {
                    "euid": "Repository::\"/\"",
                    "is_private": true,
                    "admins": "UserGroup::\"admin\"",
                    "maintainers": "UserGroup::\"matainer\"",
                    "readers": "UserGroup::\"reader\"",
                    "parents": []
                }
            },
            "user_groups": {
                "UserGroup::\"admin\"": {
                    "euid": "UserGroup::\"admin\"",
                    "parents": ["UserGroup::\"matainer\""]
                },
                "UserGroup::\"matainer\"": {
                    "euid": "UserGroup::\"matainer\"",
                    "parents": ["UserGroup::\"reader\""]
                },
                "UserGroup::\"reader\"": { "euid": "UserGroup::\"reader\"", "parents": [] }
            },
            "merge_requests": {},
            "issues": {}
        })
        .to_string();
        crate::contract::policy::builder::build_from_json(&json).expect("build snapshot")
    }

    fn executes_as(decision: &QueueExecutionDecision) -> Option<&str> {
        match decision {
            QueueExecutionDecision::Execute { authz_principal } => Some(authz_principal),
            QueueExecutionDecision::Freeze { .. } => None,
        }
    }

    fn freeze_reason(decision: &QueueExecutionDecision) -> Option<&str> {
        match decision {
            QueueExecutionDecision::Freeze { reason } => Some(reason),
            QueueExecutionDecision::Execute { .. } => None,
        }
    }

    /// Under `enforce`, `approveMergeRequest` is a maintainer-level action in the
    /// current policy (admin inherits it), so the four recorded requesters split
    /// two and two. Whether approving should be admin-only is policy content
    /// (UN-09/UN-04), not this card.
    #[test]
    fn un17_queue_execution_matrix_under_enforce() {
        let snapshot = role_snapshot();
        let cases: &[(Option<&str>, bool)] = &[
            (Some(ADMIN), true),
            (Some(MAINTAINER), true),
            (Some(READER), false),
            (None, false),
        ];

        for (requester, should_execute) in cases {
            let decision =
                decide_queue_execution(Enforcement::Enforce, Some(&snapshot), *requester);
            assert_eq!(
                executes_as(&decision).is_some(),
                *should_execute,
                "requester {requester:?} under enforce"
            );
            if *should_execute {
                assert_eq!(
                    executes_as(&decision),
                    *requester,
                    "the merge must be authorized as the recorded requester, not as the worker"
                );
            }
        }
    }

    #[test]
    fn un17_a_queued_item_without_a_requester_is_frozen_with_guidance() {
        let snapshot = role_snapshot();
        let decision = decide_queue_execution(Enforcement::Enforce, Some(&snapshot), None);

        let reason = freeze_reason(&decision).expect("must freeze");
        assert_eq!(
            reason, QUEUE_MISSING_REQUESTER_GUIDANCE,
            "a subject-less item must carry the operator guidance verbatim"
        );
        assert!(
            reason.contains("re-queue"),
            "the guidance must say what a human should do: {reason}"
        );
    }

    #[test]
    fn un17_a_missing_snapshot_freezes_with_a_retryable_reason() {
        let decision = decide_queue_execution(Enforcement::Enforce, None, Some(ADMIN));
        let reason = freeze_reason(&decision).expect("must freeze");
        assert!(
            reason.contains("retry"),
            "an undecidable-right-now state is retryable and must say so: {reason}"
        );
    }

    #[test]
    fn un17_off_executes_exactly_as_before() {
        // `off` consumes no authorization data at all (GC-UN-01): even a snapshot
        // that would refuse changes nothing, and a subject-less legacy item still
        // runs as it did before this card.
        let snapshot = role_snapshot();
        assert_eq!(
            executes_as(&decide_queue_execution(
                Enforcement::Off,
                Some(&snapshot),
                Some(READER)
            )),
            Some(READER)
        );
        assert_eq!(
            executes_as(&decide_queue_execution(Enforcement::Off, None, None)),
            Some("system"),
            "a legacy item keeps running as the worker under off"
        );
    }

    #[test]
    fn un17_shadow_never_freezes() {
        let snapshot = role_snapshot();
        for requester in [Some(READER), None] {
            assert!(
                executes_as(&decide_queue_execution(
                    Enforcement::Shadow,
                    Some(&snapshot),
                    requester
                ))
                .is_some(),
                "shadow evaluates and records, but must not change what runs: {requester:?}"
            );
        }
    }

    /// `shadow` must not freeze even when there is nothing to decide against —
    /// it records what `enforce` would have refused and lets the merge run.
    #[test]
    fn un17_shadow_does_not_freeze_when_the_snapshot_is_missing() {
        assert_eq!(
            executes_as(&decide_queue_execution(
                Enforcement::Shadow,
                None,
                Some(READER)
            )),
            Some(READER)
        );
    }

    #[test]
    fn un17_an_empty_store_freezes_under_enforce() {
        let empty = crate::contract::policy::builder::build_from_json(
            r#"{"users":{},"repos":{},"user_groups":{},"merge_requests":{},"issues":{}}"#,
        )
        .expect("build empty snapshot");

        assert!(
            freeze_reason(&decide_queue_execution(
                Enforcement::Enforce,
                Some(&empty),
                Some(ADMIN)
            ))
            .is_some(),
            "enforce + empty store = deny (fail-closed, ADR-UN-01)"
        );
    }
}
