use object_store::Error as ObjectStoreError;

#[derive(Debug, thiserror::Error)]
pub enum IoOrbitError {
    #[error("object store error: {message}")]
    ObjectStore { message: String, not_found: bool },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("TOML deserialize error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("write manifest precondition failed")]
    WriteManifestPreconditionFailed,

    #[error("other error: {0}")]
    Other(String),
}

pub type OrbitResult<T> = Result<T, IoOrbitError>;

impl IoOrbitError {
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            IoOrbitError::ObjectStore {
                not_found: true,
                ..
            }
        )
    }

    pub fn object_store(message: impl Into<String>) -> Self {
        IoOrbitError::ObjectStore {
            message: message.into(),
            not_found: false,
        }
    }

    pub fn object_store_not_found(message: impl Into<String>) -> Self {
        IoOrbitError::ObjectStore {
            message: message.into(),
            not_found: true,
        }
    }
}

impl From<String> for IoOrbitError {
    fn from(err: String) -> Self {
        IoOrbitError::Other(err)
    }
}

impl From<&str> for IoOrbitError {
    fn from(err: &str) -> Self {
        IoOrbitError::Other(err.to_string())
    }
}

impl From<ObjectStoreError> for IoOrbitError {
    fn from(err: ObjectStoreError) -> Self {
        match err {
            ObjectStoreError::NotFound { .. } => {
                IoOrbitError::object_store_not_found(err.to_string())
            }
            other => IoOrbitError::object_store(other.to_string()),
        }
    }
}
