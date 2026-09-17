//! Vault error types.
//!
//! [`VaultError`] is mega2's own surface: the conditions the integration
//! layer in `src/contract/vault/` reports to its callers. [`RvError`] — the
//! library's own error enum — is re-exported from the `libvault` crate rather
//! than defined here, so there is exactly one definition of it and no copy to
//! keep in step with the upstream one. The crate's crypto and seal errors
//! (`libvault::utils::crypto::CryptoError`, `libvault::utils::seal::SealBoxError`)
//! are deliberately not re-exported: nothing in mega2 consumes them, and a
//! re-export nobody uses is just a second name for the same type.

use std::path::PathBuf;

pub use libvault::errors::RvError;
use thiserror::Error;

use super::MegaError;

pub type VaultResult<T> = Result<T, VaultError>;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("failed to create vault directory at {path}: {source}")]
    DirectoryCreate {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to restrict vault directory permissions at {path}: {source}")]
    DirectoryPermissions {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "vault core key file is missing at {path}; restore key material or run an explicit reset"
    )]
    CoreKeyMissing { path: PathBuf },
    #[error("vault core key file exists at {path}, but vault storage is not initialized")]
    CoreKeyExistsWithoutInitializedStorage { path: PathBuf },
    #[error("failed to read vault core key file at {path}: {source}")]
    CoreKeyRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse vault core key file at {path}: {source}")]
    CoreKeyDeserialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to create vault core key file at {path}: {source}")]
    CoreKeyWrite {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize vault core key file at {path}: {source}")]
    CoreKeySerialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("vault core key file contains {actual} shares, but {expected} are required")]
    CoreKeyTooFewShares { expected: usize, actual: usize },
    #[error("failed to create RustyVault instance: {0}")]
    RustyVaultCreate(String),
    #[error("failed to inspect vault initialization state: {0}")]
    InitializationState(String),
    #[error("failed to initialize vault database storage: {0}")]
    DatabaseStorage(String),
    #[error("failed to initialize vault core: {0}")]
    Initialize(String),
    #[error("failed to unseal vault core: {0}")]
    Unseal(String),
    #[error("failed to rekey vault unseal shares: {0}")]
    Rekey(String),
    #[error("vault root token is required to create missing runtime credentials")]
    RootTokenRequiredForRuntimeCredentials,
    #[error("failed to write vault runtime policy {policy}: {message}")]
    RuntimePolicyWrite { policy: String, message: String },
    #[error("failed to create vault runtime token for policy {policy}: {message}")]
    RuntimeTokenCreate { policy: String, message: String },
    #[error("vault runtime token response for policy {policy} did not include a client token")]
    RuntimeTokenMissing { policy: String },
    #[error("failed to ensure vault pki mount: {0}")]
    PkiMount(String),
    #[error("failed to revoke initialized vault root token")]
    RootTokenRevoke,
    #[error("failed to reset vault storage: {0}")]
    Reset(String),
    #[error("invalid vault secret name")]
    InvalidSecretName,
    #[error("failed to read from vault API: {0}")]
    ReadApi(String),
    #[error("failed to write to vault API: {0}")]
    WriteApi(String),
    #[error("failed to delete from vault API: {0}")]
    DeleteApi(String),
    #[error(
        "vault storage is not initialized; a readonly open reports this rather than initializing it"
    )]
    ReadonlyNotInitialized,
    #[error(
        "vault core key file does not carry a complete runtime token set; creating the missing \
         tokens is a write, which a readonly open will not perform"
    )]
    ReadonlyRuntimeTokensIncomplete,
    #[error("vault was opened readonly; the write was denied")]
    ReadonlyWriteDenied,
    /// The library refused something during the readonly bootstrap.
    ///
    /// Distinct from [`VaultError::Unseal`], which is specifically about the
    /// barrier: this covers the rest of the readonly open — module setup,
    /// barrier views, the policy store — where the only thing the library can
    /// hand back is an `RvError` with no mega2-side meaning attached.
    #[error("failed to open vault readonly: {0}")]
    ReadonlyOpen(String),
    /// The stored vault state is missing or in an older format, and a readonly
    /// open will not repair it.
    ///
    /// This is the identity the library's readonly "state incomplete" error used
    /// to carry. `libvault` has no such variant, and it should not: the condition
    /// belongs to mega2's readonly bootstrap, not to the library. `detail`
    /// names which piece of state, so an operator is not left to guess.
    #[error(
        "vault is being opened readonly and its stored {detail} is missing or in an older format; \
         readonly mode will not repair it"
    )]
    ReadonlyStateIncomplete { detail: &'static str },
    /// A readonly open was requested while the readonly bootstrap is not built.
    ///
    /// Fail-closed placeholder: it exists so the window has a name rather than a
    /// fallback to the writable path, which would repair the very state a
    /// readonly open exists to leave alone.
    #[error(
        "vault readonly bootstrap is unavailable in this build; refusing to fall back to the \
         writable open, which would repair stored state"
    )]
    ReadonlyUnavailable,
}

impl From<VaultError> for MegaError {
    fn from(err: VaultError) -> Self {
        MegaError::Other(err.to_string())
    }
}
