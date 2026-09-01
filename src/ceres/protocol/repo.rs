use std::path::PathBuf;

use crate::{
    callisto::git_repo,
    common::{errors::ProtocolError, utils::generate_id},
};

/// The `repo` struct maintains the relationship between `repo_id` and `repo_path`.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct Repo {
    pub repo_id: i64,
    pub repo_path: String,
    pub repo_name: String,
    pub is_monorepo: bool,
}

impl Repo {
    pub fn new(path: PathBuf, is_monorepo: bool) -> Result<Self, ProtocolError> {
        let repo_path = path.to_str().map(String::from).ok_or_else(|| {
            ProtocolError::InvalidInput("repository path is not valid UTF-8".to_owned())
        })?;
        let repo_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(String::from)
            .ok_or_else(|| {
                ProtocolError::InvalidInput("repository path has no valid name".to_owned())
            })?;
        Ok(Self {
            repo_id: generate_id()?,
            repo_path,
            repo_name,
            is_monorepo,
        })
    }
}

impl From<git_repo::Model> for Repo {
    fn from(value: git_repo::Model) -> Self {
        Self {
            repo_id: value.id,
            repo_path: value.repo_path,
            repo_name: value.repo_name,
            is_monorepo: false,
        }
    }
}

impl From<Repo> for git_repo::Model {
    fn from(value: Repo) -> Self {
        git_repo::Model {
            id: value.repo_id,
            repo_path: value.repo_path,
            repo_name: value.repo_name,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_new_accepts_valid_utf8_path() {
        let repo = Repo::new(PathBuf::from("/srv/git/project.git"), false).unwrap();

        assert_eq!(repo.repo_path, "/srv/git/project.git");
        assert_eq!(repo.repo_name, "project.git");
        assert!(!repo.is_monorepo);
    }

    #[test]
    #[cfg(unix)]
    fn repo_new_rejects_non_utf8_path() {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
        let err = Repo::new(path, false).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn repo_new_rejects_path_without_file_name() {
        let err = Repo::new(PathBuf::from("/"), false).unwrap_err();

        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("no valid name"));
    }
}
