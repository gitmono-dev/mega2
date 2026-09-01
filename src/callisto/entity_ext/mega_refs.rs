use std::path::Path;

use crate::{
    callisto::{entity_ext::generate_id, mega_refs},
    common::errors::MegaError,
};

impl mega_refs::Model {
    pub fn new<P: AsRef<Path>>(
        path: P,
        ref_name: String,
        ref_commit_hash: String,
        ref_tree_hash: String,
        is_cl: bool,
    ) -> Result<Self, MegaError> {
        let now = chrono::Utc::now().naive_utc();
        let path = path
            .as_ref()
            .to_str()
            .ok_or_else(|| MegaError::Other("reference path is not valid UTF-8".to_string()))?
            .to_string();
        Ok(Self {
            id: generate_id()?,
            path,
            ref_name,
            ref_commit_hash,
            ref_tree_hash,
            created_at: now,
            updated_at: now,
            is_cl,
        })
    }
}
