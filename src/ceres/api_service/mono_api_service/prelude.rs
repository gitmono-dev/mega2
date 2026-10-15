pub(super) use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, LazyLock},
    time::Duration,
};

pub(super) use async_trait::async_trait;
pub(super) use bytes::Bytes;
pub(super) use cedar_policy::{
    Context as CedarRequestContext, EntityId, EntityTypeName, EntityUid,
};
pub(super) use git_internal::{
    DiffItem,
    diff::Diff as GitDiff,
    errors::GitError,
    hash::{ObjectHash, get_hash_kind},
    internal::{
        metadata::EntryMeta,
        object::{
            blob::Blob,
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    },
};
pub(super) use regex::Regex;
pub(super) use sea_orm::DatabaseTransaction;
pub(super) use tracing::debug;

pub(super) use crate::{
    callisto::{
        mega_blob, mega_cl, mega_refs, mega_tag, mega_tree,
        sea_orm_active_enums::{
            CheckTypeEnum, ConvTypeEnum, MergeStatusEnum, PushQueueKindEnum, PushQueueStatusEnum,
        },
    },
    ceres::{
        api_service::{
            ApiHandler, buck_tree_builder::BuckCommitBuilder, cache::GitObjectCache,
            state::ProtocolApiState, tree_ops,
        },
        code_edit::{on_edit::OneditCodeEdit, utils as edit_utils},
        diff::tree_diff,
        model::{
            buck::{
                CompletePayload, CompleteResponse, DEFAULT_MODE, FileChange,
                FileToUpload as ApiFileToUpload, ManifestPayload, ManifestResponse,
            },
            change_list::{ClDiffFile, ClFilesChangedItemSchema, UpdateBranchStatusRes},
            git::{
                CreateEntryInfo, CreateEntryResult, DeleteEntryInfo, DeleteEntryResult, EditCLMode,
                EditFilePayload, EditFileResult, MoveEntryInfo, MoveEntryResult,
                PathProvisionResult, common_parent_path, join_entry_path, normalize_parent_path,
                validate_entry_target,
            },
            tag::TagInfo,
            third_party::{ThirdPartyClient, ThirdPartyRepoTrait},
        },
        pack::{
            api_tip_lander::land_api_tip_push, import_repo::ImportRepo, materialize,
            monorepo::Monorepo, path_policy,
        },
        protocol::{ServiceType, SmartSession, TransportProtocol},
    },
    common::{
        errors::{BuckError, MegaError, PathPolicyError},
        utils::{MEGA_BRANCH_NAME, ZERO_ID, format_commit_msg},
    },
    config::PushPolicy,
    contract::{
        api::common::Pagination,
        policy::{
            builder::EntitySnapshot,
            context::CedarContext,
            enforcement::Enforcement,
            entitystore::MEGA_CEDAR_PATH,
            notify::{
                AUTHZ_BARRIER_TIMEOUT, authz_blob_id, ensure_authz_snapshot_caught_up,
                notify_authz_changed_best_effort,
            },
            resource::resolve_resource,
            util::SaturnEUid,
        },
        vault::server_signing::ServerSigningContext,
    },
    jupiter::{
        service::{
            buck_service::{
                CommitArtifacts, CompletePayload as SvcCompletePayload,
                CompleteResponse as SvcCompleteResponse,
            },
            push_queue_service::{
                EnqueueRequest, ExecuteOutcome, ExecuteRequest, MergeExecContext, MergePayload,
                PushPayload, QueueWaitResult, merge_operation_id,
            },
        },
        storage::{
            Storage,
            base_storage::StorageConnector,
            buck_storage::{session_status, upload_status},
            mono_storage::RefUpdateData,
        },
        utils::converter::{
            FromMegaModel, IntoMegaModel, generate_git_keep_with_timestamp, sort_git_tree_items,
        },
    },
};
