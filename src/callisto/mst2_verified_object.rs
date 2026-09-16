//! MST/2 verified object record (spec 08 §2, T03).
//!
//! One row per verified object: Git identity + raw content digest + 64-bit
//! size + verification state. The natural key is
//! `(storage_domain, git_oid, object_kind)`; rows are insert-only with
//! ON CONFLICT DO NOTHING — a row only ever appears after the content was
//! fetched and hashed by this service (write-through verification).

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "mst2_verified_object")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// Storage partition (spec 08); `git` is the shared Git-authoritative
    /// domain. Not an authorization boundary.
    pub storage_domain: String,
    pub git_oid: String,
    /// `blob` in this profile; trees/commits are verified structurally.
    pub object_kind: String,
    /// SHA-256 over the raw content bytes (32 bytes binary).
    pub raw_sha256: Vec<u8>,
    /// 64-bit verified content length (covers the `mega_blob.size: i32` gap).
    #[sea_orm(column_type = "BigInteger")]
    pub size: i64,
    pub verification_version: i32,
    /// `VERIFIED` in this profile; state machine widens with T06.
    pub state: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
