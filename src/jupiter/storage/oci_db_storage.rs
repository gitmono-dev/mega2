use std::ops::Deref;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder,
    QuerySelect, Set, sea_query::OnConflict,
};

use crate::{
    callisto::{oci_blob_ref, oci_manifest, oci_tag, oci_upload},
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct OciDbStorage {
    pub base: BaseStorage,
}

impl Deref for OciDbStorage {
    type Target = BaseStorage;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl OciDbStorage {
    pub async fn upsert_tag(
        &self,
        repo_name: &str,
        tag: &str,
        digest: &str,
    ) -> Result<(), MegaError> {
        let now = chrono::Utc::now().fixed_offset();
        oci_tag::Entity::insert(oci_tag::ActiveModel {
            repo_name: Set(repo_name.to_owned()),
            tag: Set(tag.to_owned()),
            digest: Set(digest.to_owned()),
            updated_at: Set(now),
        })
        .on_conflict(
            OnConflict::columns([oci_tag::Column::RepoName, oci_tag::Column::Tag])
                .update_columns([oci_tag::Column::Digest, oci_tag::Column::UpdatedAt])
                .to_owned(),
        )
        .exec(self.get_connection())
        .await?;
        Ok(())
    }

    pub async fn get_tag(
        &self,
        repo_name: &str,
        tag: &str,
    ) -> Result<Option<oci_tag::Model>, MegaError> {
        Ok(
            oci_tag::Entity::find_by_id((repo_name.to_owned(), tag.to_owned()))
                .one(self.get_connection())
                .await?,
        )
    }

    pub async fn list_tags(
        &self,
        repo_name: &str,
        after_tag: Option<&str>,
        limit: u64,
    ) -> Result<Vec<oci_tag::Model>, MegaError> {
        let mut query = oci_tag::Entity::find()
            .filter(oci_tag::Column::RepoName.eq(repo_name))
            .order_by_asc(oci_tag::Column::Tag)
            .limit(limit);
        if let Some(after_tag) = after_tag {
            query = query.filter(oci_tag::Column::Tag.gt(after_tag));
        }
        Ok(query.all(self.get_connection()).await?)
    }

    pub async fn put_manifest(
        &self,
        repo_name: &str,
        digest: &str,
        media_type: &str,
        size: i64,
    ) -> Result<(), MegaError> {
        let now = chrono::Utc::now().fixed_offset();
        oci_manifest::Entity::insert(oci_manifest::ActiveModel {
            repo_name: Set(repo_name.to_owned()),
            digest: Set(digest.to_owned()),
            media_type: Set(media_type.to_owned()),
            size: Set(size),
            created_at: Set(now),
        })
        .on_conflict(
            OnConflict::columns([oci_manifest::Column::RepoName, oci_manifest::Column::Digest])
                .update_columns([oci_manifest::Column::MediaType, oci_manifest::Column::Size])
                .to_owned(),
        )
        .exec(self.get_connection())
        .await?;
        Ok(())
    }

    pub async fn get_manifest(
        &self,
        repo_name: &str,
        digest: &str,
    ) -> Result<Option<oci_manifest::Model>, MegaError> {
        Ok(
            oci_manifest::Entity::find_by_id((repo_name.to_owned(), digest.to_owned()))
                .one(self.get_connection())
                .await?,
        )
    }

    pub async fn put_blob_ref(
        &self,
        repo_name: &str,
        digest: &str,
        size: i64,
    ) -> Result<(), MegaError> {
        let now = chrono::Utc::now().fixed_offset();
        oci_blob_ref::Entity::insert(oci_blob_ref::ActiveModel {
            repo_name: Set(repo_name.to_owned()),
            digest: Set(digest.to_owned()),
            size: Set(size),
            created_at: Set(now),
        })
        .on_conflict(
            OnConflict::columns([oci_blob_ref::Column::RepoName, oci_blob_ref::Column::Digest])
                .update_column(oci_blob_ref::Column::Size)
                .to_owned(),
        )
        .exec(self.get_connection())
        .await?;
        Ok(())
    }

    pub async fn get_blob_ref(
        &self,
        repo_name: &str,
        digest: &str,
    ) -> Result<Option<oci_blob_ref::Model>, MegaError> {
        Ok(
            oci_blob_ref::Entity::find_by_id((repo_name.to_owned(), digest.to_owned()))
                .one(self.get_connection())
                .await?,
        )
    }

    pub async fn blob_ref_exists(&self, repo_name: &str, digest: &str) -> Result<bool, MegaError> {
        Ok(self.get_blob_ref(repo_name, digest).await?.is_some())
    }

    /// Repository existence without an `oci_repository` table (ADR-DR-03):
    /// any tag, manifest, or blob_ref row for `repo_name`.
    pub async fn repo_exists(&self, repo_name: &str) -> Result<bool, MegaError> {
        if oci_tag::Entity::find()
            .filter(oci_tag::Column::RepoName.eq(repo_name))
            .limit(1)
            .one(self.get_connection())
            .await?
            .is_some()
        {
            return Ok(true);
        }
        if oci_manifest::Entity::find()
            .filter(oci_manifest::Column::RepoName.eq(repo_name))
            .limit(1)
            .one(self.get_connection())
            .await?
            .is_some()
        {
            return Ok(true);
        }
        Ok(oci_blob_ref::Entity::find()
            .filter(oci_blob_ref::Column::RepoName.eq(repo_name))
            .limit(1)
            .one(self.get_connection())
            .await?
            .is_some())
    }

    pub async fn insert_upload(&self, uuid: &str, repo_name: &str) -> Result<(), MegaError> {
        let now = chrono::Utc::now().fixed_offset();
        oci_upload::Entity::insert(oci_upload::ActiveModel {
            uuid: Set(uuid.to_owned()),
            repo_name: Set(repo_name.to_owned()),
            offset: Set(0),
            chunks: Set(0),
            started_at: Set(now),
            updated_at: Set(now),
        })
        .exec(self.get_connection())
        .await?;
        Ok(())
    }

    pub async fn get_upload(&self, uuid: &str) -> Result<Option<oci_upload::Model>, MegaError> {
        Ok(oci_upload::Entity::find_by_id(uuid.to_owned())
            .one(self.get_connection())
            .await?)
    }

    pub async fn update_upload_offset_and_chunks(
        &self,
        uuid: &str,
        offset: i64,
        chunks: i64,
    ) -> Result<Option<oci_upload::Model>, MegaError> {
        let Some(upload) = self.get_upload(uuid).await? else {
            return Ok(None);
        };
        let mut upload = upload.into_active_model();
        upload.offset = Set(offset);
        upload.chunks = Set(chunks);
        upload.updated_at = Set(chrono::Utc::now().fixed_offset());
        Ok(Some(upload.update(self.get_connection()).await?))
    }

    pub async fn delete_upload(&self, uuid: &str) -> Result<(), MegaError> {
        oci_upload::Entity::delete_by_id(uuid.to_owned())
            .exec(self.get_connection())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn storage() -> (tempfile::TempDir, OciDbStorage) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        (
            temp_dir,
            OciDbStorage {
                base: BaseStorage::new(std::sync::Arc::new(db)),
            },
        )
    }

    #[tokio::test]
    async fn tag_upsert_read_roundtrip() {
        let (_temp_dir, storage) = storage().await;

        storage
            .upsert_tag("team/image", "v1", "sha256:first")
            .await
            .expect("insert tag");
        storage
            .upsert_tag("team/image", "v1", "sha256:second")
            .await
            .expect("upsert tag");
        storage
            .upsert_tag("team/image", "v2", "sha256:third")
            .await
            .expect("insert second tag");

        let tag = storage
            .get_tag("team/image", "v1")
            .await
            .expect("read tag")
            .expect("tag exists");
        assert_eq!(tag.digest, "sha256:second");

        let tags = storage
            .list_tags("team/image", Some("v1"), 10)
            .await
            .expect("list tags");
        assert_eq!(
            tags.into_iter().map(|tag| tag.tag).collect::<Vec<_>>(),
            ["v2"]
        );
    }

    #[tokio::test]
    async fn manifest_put_read_roundtrip() {
        let (_temp_dir, storage) = storage().await;

        storage
            .put_manifest(
                "team/image",
                "sha256:manifest",
                "application/vnd.oci.image.manifest.v1+json",
                123,
            )
            .await
            .expect("put manifest");

        let manifest = storage
            .get_manifest("team/image", "sha256:manifest")
            .await
            .expect("read manifest")
            .expect("manifest exists");
        assert_eq!(
            manifest.media_type,
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(manifest.size, 123);
    }

    #[tokio::test]
    async fn blob_ref_put_read_exists_roundtrip() {
        let (_temp_dir, storage) = storage().await;

        storage
            .put_blob_ref("team/image", "sha256:blob", 456)
            .await
            .expect("put blob reference");

        let blob_ref = storage
            .get_blob_ref("team/image", "sha256:blob")
            .await
            .expect("read blob reference")
            .expect("blob reference exists");
        assert_eq!(blob_ref.size, 456);
        assert!(
            storage
                .blob_ref_exists("team/image", "sha256:blob")
                .await
                .expect("check blob reference")
        );
        assert!(
            !storage
                .blob_ref_exists("team/image", "sha256:missing")
                .await
                .expect("check missing blob reference")
        );
    }

    #[tokio::test]
    async fn upload_update_roundtrip() {
        let (_temp_dir, storage) = storage().await;

        storage
            .insert_upload("upload-1", "team/image")
            .await
            .expect("insert upload");
        let upload = storage
            .update_upload_offset_and_chunks("upload-1", 1024, 2)
            .await
            .expect("update upload")
            .expect("upload exists");
        assert_eq!((upload.offset, upload.chunks), (1024, 2));

        storage
            .delete_upload("upload-1")
            .await
            .expect("delete upload");
        assert!(
            storage
                .get_upload("upload-1")
                .await
                .expect("read deleted upload")
                .is_none()
        );
    }
}
