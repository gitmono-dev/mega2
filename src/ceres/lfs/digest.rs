//! Independent Git Object Format vs LFS Digest hash domains (ADR-B3-07).
//!
//! Repository `MonoConfig.object_format` does not select or override the LFS
//! digest algorithm. LFS IDs are parsed with an explicit algorithm; a bare
//! 64-hex string is not SHA-256 just because of its length.

use sha2::{Digest, Sha256};

use crate::{common::errors::MegaError, config::MonoObjectFormat};

/// Digest algorithm for Git LFS object IDs. Independent of Git object format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LfsDigestAlgorithm {
    Sha256,
    Blake3,
}

impl LfsDigestAlgorithm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
        }
    }

    pub const fn hex_len(self) -> usize {
        64
    }

    pub fn parse_name(name: &str) -> Result<Self, MegaError> {
        match name.to_ascii_lowercase().as_str() {
            "sha256" => Ok(Self::Sha256),
            "blake3" => Ok(Self::Blake3),
            other => Err(MegaError::Other(format!(
                "unsupported LFS digest algorithm '{other}'; accepted: sha256|blake3"
            ))),
        }
    }
}

/// An LFS content digest with an explicit algorithm (never inferred from width).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LfsDigest {
    algorithm: LfsDigestAlgorithm,
    hex: String,
}

impl LfsDigest {
    pub fn algorithm(&self) -> LfsDigestAlgorithm {
        self.algorithm
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }

    pub fn to_tagged_string(&self) -> String {
        format!("{}:{}", self.algorithm.as_str(), self.hex)
    }

    /// Parse `sha256:<64hex>` / `blake3:<64hex>`. Untagged input fails closed.
    pub fn parse_tagged(input: &str) -> Result<Self, MegaError> {
        let (tag, hex) = input.split_once(':').ok_or_else(|| {
            MegaError::Other(
                "LFS digest is missing an algorithm tag; length does not imply sha256".to_string(),
            )
        })?;
        let algorithm = LfsDigestAlgorithm::parse_name(tag)?;
        Self::from_hex_for_algorithm(algorithm, hex)
    }

    /// Parse a raw hex OID for an explicit LFS algorithm (Git LFS `hash_algo` + `oid`).
    pub fn from_hex_for_algorithm(
        algorithm: LfsDigestAlgorithm,
        hex: &str,
    ) -> Result<Self, MegaError> {
        let expected = algorithm.hex_len();
        if hex.len() != expected {
            return Err(MegaError::Other(format!(
                "LFS {} digest width mismatch: expected {expected} hex chars, got {}",
                algorithm.as_str(),
                hex.len()
            )));
        }
        if !hex.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
            return Err(MegaError::Other(format!(
                "LFS {} digest is not hexadecimal",
                algorithm.as_str()
            )));
        }
        Ok(Self {
            algorithm,
            hex: hex.to_ascii_lowercase(),
        })
    }

    /// Standard LFS SHA-256 content digest (Git LFS default algorithm).
    pub fn sha256_of(bytes: &[u8]) -> Self {
        let hex = hex::encode(Sha256::digest(bytes));
        Self {
            algorithm: LfsDigestAlgorithm::Sha256,
            hex,
        }
    }
}

/// Explicit Git Object Format + LFS Digest pair. Fields are independent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashDomainPair {
    pub git: MonoObjectFormat,
    pub lfs: LfsDigestAlgorithm,
}

impl HashDomainPair {
    pub fn new(git: MonoObjectFormat, lfs: LfsDigestAlgorithm) -> Self {
        Self { git, lfs }
    }
}

/// LFS BLAKE3 endpoint / manifest / object-key work is DEFER-B3-LFS-01.
pub fn lfs_blake3_business_path(algorithm: LfsDigestAlgorithm) -> Result<(), MegaError> {
    match algorithm {
        LfsDigestAlgorithm::Sha256 => Ok(()),
        LfsDigestAlgorithm::Blake3 => Err(MegaError::Other(
            "LFS BLAKE3 business path is deferred (DEFER-B3-LFS-01); algorithm is expressible only"
                .to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{HashDomainPair, LfsDigest, LfsDigestAlgorithm, lfs_blake3_business_path};
    use crate::{
        ceres::lfs::lfs_structs::{BatchRequest, Operation, RequestObject},
        config::MonoObjectFormat,
    };

    const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn batch(hash_algo: &str, oid: &str) -> BatchRequest {
        BatchRequest {
            operation: Operation::Upload,
            transfers: Vec::new(),
            objects: vec![RequestObject {
                oid: oid.to_string(),
                size: 0,
                ..Default::default()
            }],
            hash_algo: hash_algo.to_string(),
        }
    }

    #[test]
    fn b3_02a_hash_domains_are_independent() {
        let git_blake3_lfs_sha256 =
            HashDomainPair::new(MonoObjectFormat::Blake3, LfsDigestAlgorithm::Sha256);
        let git_sha256_lfs_blake3 =
            HashDomainPair::new(MonoObjectFormat::Sha256, LfsDigestAlgorithm::Blake3);
        assert_eq!(git_blake3_lfs_sha256.git, MonoObjectFormat::Blake3);
        assert_eq!(git_blake3_lfs_sha256.lfs, LfsDigestAlgorithm::Sha256);
        assert_eq!(git_sha256_lfs_blake3.git, MonoObjectFormat::Sha256);
        assert_eq!(git_sha256_lfs_blake3.lfs, LfsDigestAlgorithm::Blake3);
        assert_ne!(
            git_blake3_lfs_sha256.git.as_str(),
            git_blake3_lfs_sha256.lfs.as_str()
        );

        let sha256 = LfsDigest::from_hex_for_algorithm(LfsDigestAlgorithm::Sha256, SHA256_EMPTY)
            .expect("standard LFS sha256 oid");
        assert_eq!(sha256.algorithm(), LfsDigestAlgorithm::Sha256);
        assert_eq!(sha256.hex(), SHA256_EMPTY);
        assert_eq!(LfsDigest::sha256_of(b""), sha256);
        assert_eq!(
            LfsDigest::parse_tagged(&format!("sha256:{SHA256_EMPTY}")).unwrap(),
            sha256
        );

        let blake3 = LfsDigest::from_hex_for_algorithm(LfsDigestAlgorithm::Blake3, SHA256_EMPTY)
            .expect("blake3 type is expressible at the same width");
        assert_eq!(blake3.algorithm(), LfsDigestAlgorithm::Blake3);
        assert_ne!(sha256, blake3);

        let missing_tag = LfsDigest::parse_tagged(SHA256_EMPTY).expect_err("untagged 64-hex");
        assert!(
            missing_tag.to_string().contains("algorithm tag"),
            "{missing_tag}"
        );

        let wrong_prefix =
            LfsDigest::parse_tagged(&format!("sha1:{SHA256_EMPTY}")).expect_err("git prefix");
        assert!(
            wrong_prefix.to_string().contains("unsupported LFS digest"),
            "{wrong_prefix}"
        );

        let wrong_width =
            LfsDigest::from_hex_for_algorithm(LfsDigestAlgorithm::Sha256, &"a".repeat(40))
                .expect_err("sha256 is not 40 hex");
        assert!(
            wrong_width.to_string().contains("width mismatch"),
            "{wrong_width}"
        );

        assert_eq!(
            batch("sha256", SHA256_EMPTY)
                .prepare_digest_domain()
                .expect("standard sha256 batch"),
            LfsDigestAlgorithm::Sha256
        );
        assert_eq!(
            batch("", SHA256_EMPTY)
                .prepare_digest_domain()
                .expect("empty hash_algo defaults to sha256"),
            LfsDigestAlgorithm::Sha256
        );
        assert!(
            batch("sha256", &"a".repeat(40))
                .prepare_digest_domain()
                .is_err()
        );

        let omitted: BatchRequest = serde_json::from_str(&format!(
            r#"{{"operation":"upload","transfers":[],"objects":[{{"oid":"{SHA256_EMPTY}","size":0}}]}}"#
        ))
        .expect("Git LFS clients may omit hash_algo");
        assert!(omitted.hash_algo.is_empty());
        assert_eq!(
            omitted.lfs_digest_algorithm().expect("spec default"),
            LfsDigestAlgorithm::Sha256
        );
    }

    #[test]
    fn b3_02a_lfs_blake3_business_path_is_deferred() {
        let digest = LfsDigest::parse_tagged(&format!("blake3:{SHA256_EMPTY}"))
            .expect("blake3 tagged digest is expressible");
        assert_eq!(digest.algorithm(), LfsDigestAlgorithm::Blake3);
        lfs_blake3_business_path(LfsDigestAlgorithm::Sha256).expect("sha256 business path is live");
        let err = lfs_blake3_business_path(LfsDigestAlgorithm::Blake3)
            .expect_err("blake3 business path is deferred");
        assert!(err.to_string().contains("DEFER-B3-LFS-01"), "{err}");
        let batch_err = batch("blake3", SHA256_EMPTY)
            .prepare_digest_domain()
            .expect_err("blake3 batch must not enter LFS business path");
        assert!(
            batch_err.to_string().contains("DEFER-B3-LFS-01"),
            "{batch_err}"
        );
    }
}
