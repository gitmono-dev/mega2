//! Trace target identity. `project_path` locates the current project.
//! Deactivating a target and creating another row for the same path assigns
//! a new UUID; the old row stays.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "trace_target")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false, column_type = "Text")]
    pub id: String,
    #[sea_orm(column_type = "Text")]
    pub target_kind: String,
    #[sea_orm(column_type = "Text")]
    pub project_path: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub backing_import_repo_id: Option<String>,
    pub active: bool,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    use uuid::Uuid;

    use super::Column;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    fn target(path: &str, kind: &str, backing: Option<&str>) -> super::ActiveModel {
        super::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            target_kind: Set(kind.to_owned()),
            project_path: Set(path.to_owned()),
            backing_import_repo_id: Set(backing.map(str::to_owned)),
            active: Set(true),
            created_at: Set(Utc::now().fixed_offset()),
        }
    }

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let path = "/team/app";
        let first = target(path, "native", None)
            .insert(&db)
            .await
            .expect("first target");
        let second_active = target(path, "native", None).insert(&db).await;
        assert!(second_active.is_err(), "one active target per path");

        let mut stored: super::ActiveModel = super::Entity::find_by_id(first.id.clone())
            .one(&db)
            .await
            .expect("load first")
            .expect("first row")
            .into();
        stored.active = Set(false);
        stored.update(&db).await.expect("deactivate target");

        let rebuilt = target(path, "import", Some("import-repo-synthetic"))
            .insert(&db)
            .await
            .expect("rebuilt path gets a new target");
        assert_ne!(rebuilt.id, first.id);
        assert_eq!(rebuilt.project_path, path);
        assert_eq!(rebuilt.target_kind, "import");
        assert_eq!(
            rebuilt.backing_import_repo_id.as_deref(),
            Some("import-repo-synthetic")
        );

        let rows = super::Entity::find()
            .filter(Column::ProjectPath.eq(path))
            .all(&db)
            .await
            .expect("load targets");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.id == first.id && !row.active));
        assert!(rows.iter().any(|row| row.id == rebuilt.id && row.active));
        assert!(Uuid::parse_str(&rebuilt.id).is_ok());
    }
}
