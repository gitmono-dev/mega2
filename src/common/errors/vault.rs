//! The `crate::common::errors::vault` module defines RustyVault error codes and implements
//! necessary traits for them.
//!
//! The error code defined in this module are used widely in RustyVault.

use std::{
    io,
    path::PathBuf,
    sync::{PoisonError, RwLockReadGuard, RwLockWriteGuard},
};

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
    #[error("invalid vault secret name")]
    InvalidSecretName,
    #[error("failed to read from vault API: {0}")]
    ReadApi(String),
    #[error("failed to write to vault API: {0}")]
    WriteApi(String),
    #[error("failed to delete from vault API: {0}")]
    DeleteApi(String),
}

impl From<VaultError> for MegaError {
    fn from(err: VaultError) -> Self {
        MegaError::Other(err.to_string())
    }
}

/// Error types that can occur during cryptographic operations.
///
/// This enum provides a unified error type for all cryptographic operations
/// in the module, including encryption, decryption, serialization, and
/// other crypto-related errors.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// A custom error with a descriptive message.
    ///
    /// Used for errors that don't fit into the other categories,
    /// such as invalid input data or unsupported operations.
    #[error("Crypto error: {0}")]
    Custom(String),

    /// An error that occurred during JSON serialization or deserialization.
    ///
    /// This error is automatically converted from `serde_json::Error`
    /// and typically occurs when encrypting/decrypting data that
    /// cannot be properly serialized or deserialized.
    #[error("Some serde_json error happened, {:?}", .source)]
    SerdeJson {
        #[from]
        source: serde_json::Error,
    },

    /// An error that occurred during OpenSSL cryptographic operations.
    ///
    /// This error is automatically converted from `openssl::error::ErrorStack`
    /// and typically occurs during encryption, decryption, or key generation
    /// operations when the underlying OpenSSL library encounters an error.
    #[error("Some openssl error happened, {:?}", .source)]
    OpenSSL {
        #[from]
        source: openssl::error::ErrorStack,
    },

    /// An error that occurred in the RustyVault core system.
    ///
    /// This error is automatically converted from `RvError` and typically occurs
    /// when the cryptographic operation interacts with other parts of the
    /// RustyVault system.
    #[error("Some RustyVault error happened, {:?}", .source)]
    RvError { source: Box<RvError> },
}

impl From<RvError> for CryptoError {
    fn from(source: RvError) -> Self {
        Self::RvError {
            source: Box::new(source),
        }
    }
}

/// Error types that can occur during SealBox operations.
///
/// This enum provides a unified error type for all SealBox operations,
/// including creation, sealing, unsealing, and data access operations.
#[derive(Debug, Error)]
pub enum SealBoxError {
    /// The SealBox is currently sealed and data access is not allowed.
    ///
    /// This error occurs when trying to access data from a sealed SealBox.
    /// The SealBox must be unsealed with sufficient shares before data can be accessed.
    #[error("SealBox is sealed")]
    Sealed,

    /// The SealBox is not sealed when it should be.
    ///
    /// This error occurs when trying to perform operations that require
    /// the SealBox to be in a sealed state, but it's currently unsealed.
    #[error("SealBox is not sealed")]
    NotSealed,

    /// The SealBox is in the process of being unsealed but doesn't have enough shares yet.
    ///
    /// This error occurs when providing shares for unsealing, but the threshold
    /// number of shares hasn't been reached yet. Continue providing shares until
    /// the threshold is met.
    #[error("SealBox is unsealing")]
    Unsealing,

    /// The decryption operation failed.
    ///
    /// This error occurs when the AES-GCM decryption process fails, typically
    /// due to corrupted ciphertext, invalid authentication tag, or incorrect key.
    #[error("Decryption failed")]
    DecryptionFailed,

    /// The unsealing operation failed due to insufficient or invalid shares.
    ///
    /// This error occurs when the Shamir secret sharing reconstruction fails,
    /// typically due to insufficient shares, invalid shares, or corrupted share data.
    #[error("Unsealing failed: insufficient or invalid shares")]
    UnsealFailed,

    /// The unsealing operation failed due to a deprecated share.
    ///
    /// This error occurs when the provided share has already been used to unseal the box.
    #[error("Unsealing failed: deprecated share")]
    UnsealKeyDeprecated,

    /// The encryption operation failed.
    ///
    /// This error occurs when the AES-GCM encryption process fails, typically
    /// due to issues with key generation, nonce generation, or encryption parameters.
    #[error("Encryption failed")]
    EncryptionFailed,

    /// The Shamir secret splitting operation failed.
    ///
    /// This error occurs when creating shares from the master key fails,
    /// typically due to invalid threshold or total shares parameters.
    #[error("Shamir secret split failed")]
    ShamirSecretSplitFailed,

    /// The Shamir secret combining operation failed.
    ///
    /// This error occurs when reconstructing the master key from shares fails,
    /// typically due to insufficient shares or corrupted share data.
    #[error("Shamir secret combine failed")]
    ShamirSecretCombineFailed,
}

/// Centralized error enumeration for `RustyVault`.
///
/// `RvError` enumerates the common error conditions surfaced by the
/// library and the server. It implements `std::error::Error` via
/// `thiserror` and includes helpers such as `response_status()` to map
/// library errors to HTTP response codes.
#[derive(Error, Debug)]
pub enum RvError {
    #[error("Cipher operation update failed.")]
    ErrCryptoCipherUpdateFailed,
    #[error("Cipher operation finalization failed.")]
    ErrCryptoCipherFinalizeFailed,
    #[error("Cipher initialization failed.")]
    ErrCryptoCipherInitFailed,
    #[error("Cipher not initialized.")]
    ErrCryptoCipherNotInited,
    #[error("Cipher operation not supported.")]
    ErrCryptoCipherOPNotSupported,
    #[error("AEAD Cipher tag is missing.")]
    ErrCryptoCipherNoTag,
    #[error("AEAD Cipher tag should not be present.")]
    ErrCryptoCipherAEADTagPresent,
    #[error("Config path is invalid.")]
    ErrConfigPathInvalid,
    #[error("Config load failed.")]
    ErrConfigLoadFailed,
    #[error("Config storage not found.")]
    ErrConfigStorageNotFound,
    #[error("Config listener not found.")]
    ErrConfigListenerNotFound,
    #[error("Core is not initialized.")]
    ErrCoreNotInit,
    #[error("Core logical backend already exists.")]
    ErrCoreLogicalBackendExist,
    #[error("Core logical backend does not exist.")]
    ErrCoreLogicalBackendNoExist,
    #[error("Core router not handling.")]
    ErrCoreRouterNotHandling,
    #[error("Core handler already exists.")]
    ErrCoreHandlerExist,
    #[error("Core seal config is invalid.")]
    ErrCoreSealConfigInvalid,
    #[error("Core seal config not found.")]
    ErrCoreSealConfigNotFound,
    #[error("Core unseal key set not found.")]
    ErrCoreDeprecatedUnsealKeySetNotFound,
    #[error("Physical configuration item is missing.")]
    ErrPhysicalConfigItemMissing,
    #[error("Physical type is invalid.")]
    ErrPhysicalTypeInvalid,
    #[error("Physical backend prefix is invalid.")]
    ErrPhysicalBackendPrefixInvalid,
    #[error("Physical backend key is invalid.")]
    ErrPhysicalBackendKeyInvalid,
    #[error("RustyVault key sanity check failed.")]
    ErrBarrierKeySanityCheckFailed,
    #[error("RustyVault is already initialized.")]
    ErrBarrierAlreadyInit,
    #[error("RustyVault unseal key is invalid.")]
    ErrBarrierKeyInvalid,
    #[error("RustyVault unseal key is deprecated.")]
    ErrBarrierKeyDeprecated,
    #[error("RustyVault is not initialized.")]
    ErrBarrierNotInit,
    #[error("RustyVault is sealed.")]
    ErrBarrierSealed,
    #[error("RustyVault is unsealing.")]
    ErrBarrierUnsealing,
    #[error("RustyVault is unsealed.")]
    ErrBarrierUnsealed,
    #[error("RustyVault unseal failed.")]
    ErrBarrierUnsealFailed,
    #[error("RustyVualt barrier epoch do not match.")]
    ErrBarrierEpochMismatch,
    #[error("RustyVault barrier version do not match.")]
    ErrBarrierVersionMismatch,
    #[error("RustyVault barrier key generation failed.")]
    ErrBarrierKeyGenerationFailed,
    #[error("Router mount conflict.")]
    ErrRouterMountConflict,
    #[error("Router mount not found.")]
    ErrRouterMountNotFound,
    #[error("Mount path is failed, cannot mount.")]
    ErrMountFailed,
    #[error("Mount path is protected, cannot mount.")]
    ErrMountPathProtected,
    #[error("Mount path already exists.")]
    ErrMountPathExist,
    #[error("Mount table not found.")]
    ErrMountTableNotFound,
    #[error("Mount table is not ready.")]
    ErrMountTableNotReady,
    #[error("Mount not match.")]
    ErrMountNotMatch,
    #[error("Logical backend path not supported.")]
    ErrLogicalPathUnsupported,
    #[error("Logical backend operation not supported.")]
    ErrLogicalOperationUnsupported,
    #[error("Request is not ready.")]
    ErrRequestNotReady,
    #[error("No data is available for the request.")]
    ErrRequestNoData,
    #[error("No data field is available for the request.")]
    ErrRequestNoDataField,
    #[error("Request is invalid.")]
    ErrRequestInvalid,
    #[error("Request client token is missing.")]
    ErrRequestClientTokenMissing,
    #[error("Request field is not found.")]
    ErrRequestFieldNotFound,
    #[error("Request field is invalid.")]
    ErrRequestFieldInvalid,
    #[error("Response data is invalid.")]
    ErrResponseDataInvalid,
    #[error("Handler is default.")]
    ErrHandlerDefault,
    #[error("Module kv data field is missing.")]
    ErrModuleKvDataFieldMissing,
    #[error("Rust downcast failed.")]
    ErrRustDowncastFailed,
    #[error("Shamir share count invalid.")]
    ErrShamirShareCountInvalid,
    #[error("Module conflict.")]
    ErrModuleConflict,
    #[error("Module is not init.")]
    ErrModuleNotInit,
    #[error("Module is not found.")]
    ErrModuleNotFound,
    #[error("Auth module is disabled.")]
    ErrAuthModuleDisabled,
    #[error("Auth token is not found.")]
    ErrAuthTokenNotFound,
    #[error("Auth token id is invalid.")]
    ErrAuthTokenIdInvalid,
    #[error("Lease is not found.")]
    ErrLeaseNotFound,
    #[error("Lease is not renewable.")]
    ErrLeaseNotRenewable,
    #[error("Permission denied.")]
    ErrPermissionDenied,
    #[error("PKI pem bundle is invalid.")]
    ErrPkiPemBundleInvalid,
    #[error("PKI ca public key of certificate does not match private key.")]
    ErrPkiCertKeyMismatch,
    #[error("PKI cert chain is incorrect.")]
    ErrPkiCertChainIncorrect,
    #[error("PKI cert is not ca.")]
    ErrPkiCertIsNotCA,
    #[error("PKI ca private key is not found.")]
    ErrPkiCaKeyNotFound,
    #[error("PKI ca is not config.")]
    ErrPkiCaNotConfig,
    #[error("PKI ca extension is incorrect.")]
    ErrPkiCaExtensionIncorrect,
    #[error("PKI key type is invalid.")]
    ErrPkiKeyTypeInvalid,
    #[error("PKI key bits is invalid.")]
    ErrPkiKeyBitsInvalid,
    #[error("PKI key_name already exists.")]
    ErrPkiKeyNameAlreadyExist,
    #[error("PKI key operation is invalid.")]
    ErrPkiKeyOperationInvalid,
    #[error("PKI certificate is not found.")]
    ErrPkiCertNotFound,
    #[error("PKI role is not found.")]
    ErrPkiRoleNotFound,
    #[error("PKI data is invalid.")]
    ErrPkiDataInvalid,
    #[error("PKI internal error.")]
    ErrPkiInternal,
    #[error("PKI SSH CA is not configured.")]
    ErrPkiSshCaNotConfig,
    #[error("PKI SSH role is not found.")]
    ErrPkiSshRoleNotFound,
    #[error("PKI SSH certificate type is invalid.")]
    ErrPkiSshCertTypeInvalid,
    #[error("PKI SSH public key is invalid.")]
    ErrPkiSshPublicKeyInvalid,
    #[error("PKI SSH principal is not allowed by role.")]
    ErrPkiSshPrincipalNotAllowed,
    #[error("PKI PGP key is not found.")]
    ErrPkiPgpKeyNotFound,
    #[error("PKI PGP key_name already exists.")]
    ErrPkiPgpKeyNameAlreadyExist,
    #[error("PKI PGP key generation failed.")]
    ErrPkiPgpKeyGenerationFailed,
    #[error("Credential is invalid.")]
    ErrCredentialInvalid,
    #[error("Credential is not config.")]
    ErrCredentialNotConfig,
    #[error("Storage backend doesn't require a lock.")]
    ErrStorageBackendLockless,
    #[error("Storage backend lock failed.")]
    ErrStorageBackendLockFailed,
    #[error("Storage backend unlock failed.")]
    ErrStorageBackendUnlockFailed,
    #[error("Some IO error happened, {:?}", .source)]
    IO {
        #[from]
        source: io::Error,
    },
    #[error("Some serde_json error happened, {:?}", .source)]
    SerdeJson {
        #[from]
        source: serde_json::Error,
    },
    #[error("Some serde_yaml error happened, {:?}", .source)]
    SerdeYaml {
        #[from]
        source: serde_yaml::Error,
    },
    #[error("HTTP client error: {source}")]
    Reqwest {
        #[from]
        source: reqwest::Error,
    },
    #[error("Some openssl error happened, {:?}", .source)]
    OpenSSL {
        #[from]
        source: openssl::error::ErrorStack,
    },
    #[error("Some pem error happened, {:?}", .source)]
    Pem {
        #[from]
        source: pem::PemError,
    },
    #[error("Some regex error happened, {:?}", .source)]
    Regex {
        #[from]
        source: regex::Error,
    },
    #[error("Some hex error happened, {:?}", .source)]
    Hex {
        #[from]
        source: hex::FromHexError,
    },
    #[error("Some hcl error happened, {:?}", .source)]
    Hcl {
        #[from]
        source: hcl::Error,
    },
    #[error("Some humantime duration error happened, {:?}", .source)]
    HumantimeDuration {
        #[from]
        source: humantime::DurationError,
    },
    #[error("Some humantime timestamp error happened, {:?}", .source)]
    HumantimeTimestamp {
        #[from]
        source: humantime::TimestampError,
    },
    #[error("Some system_time error happened, {:?}", .source)]
    SystemTimeError {
        #[from]
        source: std::time::SystemTimeError,
    },
    #[error("Some chrono error happened, {:?}", .source)]
    ChronoError {
        #[from]
        source: chrono::ParseError,
    },
    #[error("Some bcrypt error happened, {:?}", .source)]
    BcryptError {
        #[from]
        source: bcrypt::BcryptError,
    },
    #[error("Some ureq error happened, {:?}", .source)]
    UreqError { source: Box<ureq::Error> },
    #[error("RwLock was poisoned (reading)")]
    ErrRwLockReadPoison,
    #[error("RwLock was poisoned (writing)")]
    ErrRwLockWritePoison,

    #[error("Some net addr parse error happened, {:?}", .source)]
    AddrParseError {
        #[from]
        source: std::net::AddrParseError,
    },
    #[error("Some ipnetwork error happened, {:?}", .source)]
    IpNetworkError {
        #[from]
        source: ipnetwork::IpNetworkError,
    },

    #[error("Some url error happened, {:?}", .source)]
    UrlError {
        #[from]
        source: url::ParseError,
    },

    #[error("Some rustls error happened, {:?}", .source)]
    RustlsError {
        #[from]
        source: rustls::Error,
    },

    #[error("Some rustls_pemfile error happened")]
    RustlsPemFileError(rustls_pemfile::Error),

    #[error("Some rustls_pki_types error happened")]
    RustlsPkiTypesPemFileError(rustls::pki_types::pem::Error),

    #[error("Some tokio task error happened")]
    TokioTaskJoinError {
        #[from]
        source: tokio::task::JoinError,
    },

    #[error("Some string utf8 error happened, {:?}", .source)]
    StringUtf8Error {
        #[from]
        source: std::string::FromUtf8Error,
    },

    #[error("Some lockfile error happened, {:?}", .source)]
    LockfileError {
        #[from]
        source: lockfile::Error,
    },

    #[error("Database connection info invalid")]
    ErrDatabaseConnectionInfoInvalid,

    #[cfg(any())]
    #[error("Some etcd client error happened, {:?}", .source)]
    EtcdClientError {
        #[from]
        source: etcd_client::Error,
    },

    #[error(transparent)]
    ErrOther(#[from] anyhow::Error),
    #[error("Some error happend, response text: {0}")]
    ErrResponse(String),
    #[error("Some error happend, status: {0}, response text: {1}")]
    ErrResponseStatus(u16, String),
    #[error("{0}")]
    ErrString(String),
    #[error("Unknown error.")]
    ErrUnknown,
}

impl RvError {
    pub fn response_status(&self) -> u16 {
        match self {
            RvError::ErrRequestNoData
            | RvError::ErrBarrierAlreadyInit
            | RvError::ErrBarrierKeyInvalid
            | RvError::ErrBarrierNotInit
            | RvError::ErrBarrierUnsealed
            | RvError::ErrBarrierUnsealFailed
            | RvError::ErrRequestNoDataField
            | RvError::ErrRequestInvalid
            | RvError::ErrRequestClientTokenMissing
            | RvError::ErrRequestFieldNotFound
            | RvError::ErrRequestFieldInvalid
            | RvError::ErrPkiSshCertTypeInvalid
            | RvError::ErrPkiSshPublicKeyInvalid => 400,
            RvError::ErrBarrierSealed => 503,
            RvError::ErrPermissionDenied => 403,
            RvError::ErrRouterMountNotFound => 404,
            _ => 500,
        }
    }
}

/// PartialEq is implemented to allow simple equality checks between
/// `RvError` variants (useful in tests and conditional error handling).
impl PartialEq for RvError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RvError::ErrCryptoCipherUpdateFailed, RvError::ErrCryptoCipherUpdateFailed)
            | (RvError::ErrCryptoCipherFinalizeFailed, RvError::ErrCryptoCipherFinalizeFailed)
            | (RvError::ErrCryptoCipherInitFailed, RvError::ErrCryptoCipherInitFailed)
            | (RvError::ErrCryptoCipherNotInited, RvError::ErrCryptoCipherNotInited)
            | (RvError::ErrCryptoCipherOPNotSupported, RvError::ErrCryptoCipherOPNotSupported)
            | (RvError::ErrCryptoCipherNoTag, RvError::ErrCryptoCipherNoTag)
            | (RvError::ErrCryptoCipherAEADTagPresent, RvError::ErrCryptoCipherAEADTagPresent)
            | (RvError::ErrCoreLogicalBackendExist, RvError::ErrCoreLogicalBackendExist)
            | (RvError::ErrCoreNotInit, RvError::ErrCoreNotInit)
            | (RvError::ErrCoreLogicalBackendNoExist, RvError::ErrCoreLogicalBackendNoExist)
            | (RvError::ErrCoreSealConfigInvalid, RvError::ErrCoreSealConfigInvalid)
            | (RvError::ErrCoreSealConfigNotFound, RvError::ErrCoreSealConfigNotFound)
            | (RvError::ErrCoreRouterNotHandling, RvError::ErrCoreRouterNotHandling)
            | (RvError::ErrCoreHandlerExist, RvError::ErrCoreHandlerExist)
            | (RvError::ErrPhysicalConfigItemMissing, RvError::ErrPhysicalConfigItemMissing)
            | (RvError::ErrPhysicalTypeInvalid, RvError::ErrPhysicalTypeInvalid)
            | (
                RvError::ErrPhysicalBackendPrefixInvalid,
                RvError::ErrPhysicalBackendPrefixInvalid,
            )
            | (RvError::ErrPhysicalBackendKeyInvalid, RvError::ErrPhysicalBackendKeyInvalid)
            | (RvError::ErrBarrierKeySanityCheckFailed, RvError::ErrBarrierKeySanityCheckFailed)
            | (RvError::ErrBarrierAlreadyInit, RvError::ErrBarrierAlreadyInit)
            | (RvError::ErrBarrierKeyInvalid, RvError::ErrBarrierKeyInvalid)
            | (RvError::ErrBarrierNotInit, RvError::ErrBarrierNotInit)
            | (RvError::ErrBarrierSealed, RvError::ErrBarrierSealed)
            | (RvError::ErrBarrierUnsealed, RvError::ErrBarrierUnsealed)
            | (RvError::ErrBarrierUnsealFailed, RvError::ErrBarrierUnsealFailed)
            | (RvError::ErrBarrierEpochMismatch, RvError::ErrBarrierEpochMismatch)
            | (RvError::ErrBarrierVersionMismatch, RvError::ErrBarrierVersionMismatch)
            | (RvError::ErrBarrierKeyGenerationFailed, RvError::ErrBarrierKeyGenerationFailed)
            | (RvError::ErrRouterMountConflict, RvError::ErrRouterMountConflict)
            | (RvError::ErrRouterMountNotFound, RvError::ErrRouterMountNotFound)
            | (RvError::ErrMountFailed, RvError::ErrMountFailed)
            | (RvError::ErrMountPathProtected, RvError::ErrMountPathProtected)
            | (RvError::ErrMountPathExist, RvError::ErrMountPathExist)
            | (RvError::ErrMountTableNotFound, RvError::ErrMountTableNotFound)
            | (RvError::ErrMountTableNotReady, RvError::ErrMountTableNotReady)
            | (RvError::ErrMountNotMatch, RvError::ErrMountNotMatch)
            | (RvError::ErrLogicalPathUnsupported, RvError::ErrLogicalPathUnsupported)
            | (RvError::ErrLogicalOperationUnsupported, RvError::ErrLogicalOperationUnsupported)
            | (RvError::ErrRequestNotReady, RvError::ErrRequestNotReady)
            | (RvError::ErrRequestNoData, RvError::ErrRequestNoData)
            | (RvError::ErrRequestNoDataField, RvError::ErrRequestNoDataField)
            | (RvError::ErrRequestInvalid, RvError::ErrRequestInvalid)
            | (RvError::ErrRequestClientTokenMissing, RvError::ErrRequestClientTokenMissing)
            | (RvError::ErrRequestFieldNotFound, RvError::ErrRequestFieldNotFound)
            | (RvError::ErrRequestFieldInvalid, RvError::ErrRequestFieldInvalid)
            | (RvError::ErrResponseDataInvalid, RvError::ErrResponseDataInvalid)
            | (RvError::ErrHandlerDefault, RvError::ErrHandlerDefault)
            | (RvError::ErrModuleKvDataFieldMissing, RvError::ErrModuleKvDataFieldMissing)
            | (RvError::ErrRustDowncastFailed, RvError::ErrRustDowncastFailed)
            | (RvError::ErrShamirShareCountInvalid, RvError::ErrShamirShareCountInvalid)
            | (RvError::ErrRwLockReadPoison, RvError::ErrRwLockReadPoison)
            | (RvError::ErrRwLockWritePoison, RvError::ErrRwLockWritePoison)
            | (RvError::ErrConfigPathInvalid, RvError::ErrConfigPathInvalid)
            | (RvError::ErrConfigLoadFailed, RvError::ErrConfigLoadFailed)
            | (RvError::ErrConfigStorageNotFound, RvError::ErrConfigStorageNotFound)
            | (RvError::ErrConfigListenerNotFound, RvError::ErrConfigListenerNotFound)
            | (RvError::ErrModuleConflict, RvError::ErrModuleConflict)
            | (RvError::ErrModuleNotInit, RvError::ErrModuleNotInit)
            | (RvError::ErrModuleNotFound, RvError::ErrModuleNotFound)
            | (RvError::ErrAuthModuleDisabled, RvError::ErrAuthModuleDisabled)
            | (RvError::ErrAuthTokenNotFound, RvError::ErrAuthTokenNotFound)
            | (RvError::ErrAuthTokenIdInvalid, RvError::ErrAuthTokenIdInvalid)
            | (RvError::ErrLeaseNotFound, RvError::ErrLeaseNotFound)
            | (RvError::ErrLeaseNotRenewable, RvError::ErrLeaseNotRenewable)
            | (RvError::ErrPermissionDenied, RvError::ErrPermissionDenied)
            | (RvError::ErrPkiPemBundleInvalid, RvError::ErrPkiPemBundleInvalid)
            | (RvError::ErrPkiCertKeyMismatch, RvError::ErrPkiCertKeyMismatch)
            | (RvError::ErrPkiCertChainIncorrect, RvError::ErrPkiCertChainIncorrect)
            | (RvError::ErrPkiCertIsNotCA, RvError::ErrPkiCertIsNotCA)
            | (RvError::ErrPkiCaKeyNotFound, RvError::ErrPkiCaKeyNotFound)
            | (RvError::ErrPkiCaNotConfig, RvError::ErrPkiCaNotConfig)
            | (RvError::ErrPkiCaExtensionIncorrect, RvError::ErrPkiCaExtensionIncorrect)
            | (RvError::ErrPkiKeyTypeInvalid, RvError::ErrPkiKeyTypeInvalid)
            | (RvError::ErrPkiKeyBitsInvalid, RvError::ErrPkiKeyBitsInvalid)
            | (RvError::ErrPkiKeyNameAlreadyExist, RvError::ErrPkiKeyNameAlreadyExist)
            | (RvError::ErrPkiKeyOperationInvalid, RvError::ErrPkiKeyOperationInvalid)
            | (RvError::ErrPkiCertNotFound, RvError::ErrPkiCertNotFound)
            | (RvError::ErrPkiRoleNotFound, RvError::ErrPkiRoleNotFound)
            | (RvError::ErrPkiDataInvalid, RvError::ErrPkiDataInvalid)
            | (RvError::ErrPkiInternal, RvError::ErrPkiInternal)
            | (RvError::ErrPkiSshCaNotConfig, RvError::ErrPkiSshCaNotConfig)
            | (RvError::ErrPkiSshRoleNotFound, RvError::ErrPkiSshRoleNotFound)
            | (RvError::ErrPkiSshCertTypeInvalid, RvError::ErrPkiSshCertTypeInvalid)
            | (RvError::ErrPkiSshPublicKeyInvalid, RvError::ErrPkiSshPublicKeyInvalid)
            | (RvError::ErrPkiPgpKeyNotFound, RvError::ErrPkiPgpKeyNotFound)
            | (RvError::ErrPkiPgpKeyNameAlreadyExist, RvError::ErrPkiPgpKeyNameAlreadyExist)
            | (RvError::ErrPkiPgpKeyGenerationFailed, RvError::ErrPkiPgpKeyGenerationFailed)
            | (RvError::ErrCredentialInvalid, RvError::ErrCredentialInvalid)
            | (RvError::ErrCredentialNotConfig, RvError::ErrCredentialNotConfig)
            | (RvError::ErrUnknown, RvError::ErrUnknown) => true,
            (RvError::ErrResponse(a), RvError::ErrResponse(b)) => a == b,
            (RvError::ErrResponseStatus(sa, ta), RvError::ErrResponseStatus(sb, tb)) => {
                sa == sb && ta == tb
            }
            (RvError::ErrString(a), RvError::ErrString(b)) => a == b,
            _ => false,
        }
    }
}

impl<T> From<PoisonError<RwLockWriteGuard<'_, T>>> for RvError {
    fn from(_: PoisonError<RwLockWriteGuard<'_, T>>) -> Self {
        RvError::ErrRwLockWritePoison
    }
}

impl<T> From<PoisonError<RwLockReadGuard<'_, T>>> for RvError {
    fn from(_: PoisonError<RwLockReadGuard<'_, T>>) -> Self {
        RvError::ErrRwLockReadPoison
    }
}

impl From<rustls_pemfile::Error> for RvError {
    fn from(err: rustls_pemfile::Error) -> Self {
        RvError::RustlsPemFileError(err)
    }
}

impl From<ureq::Error> for RvError {
    fn from(source: ureq::Error) -> Self {
        RvError::UreqError {
            source: Box::new(source),
        }
    }
}

impl From<rustls::pki_types::pem::Error> for RvError {
    fn from(err: rustls::pki_types::pem::Error) -> Self {
        RvError::RustlsPkiTypesPemFileError(err)
    }
}

#[macro_export]
macro_rules! rv_error_string {
    ($message:expr) => {
        RvError::ErrString($message.to_string())
    };
}

#[macro_export]
macro_rules! rv_error_response {
    ($message:expr) => {
        RvError::ErrResponse($message.to_string())
    };
}

#[macro_export]
macro_rules! rv_error_response_status {
    ($status:expr, $message:expr) => {
        RvError::ErrResponseStatus($status, $message.to_string())
    };
}
