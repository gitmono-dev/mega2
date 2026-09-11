use sha2::{Digest, Sha256};

use crate::ceres::oci::error::OciError;

const SHA256_PREFIX: &str = "sha256:";
const SHA256_HEX_LENGTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OciDigest(String);

impl OciDigest {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, OciError> {
        let value = value.as_ref();
        let Some(hex) = value.strip_prefix(SHA256_PREFIX) else {
            return Err(OciError::DigestInvalid);
        };
        if hex.len() != SHA256_HEX_LENGTH || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(OciError::DigestInvalid);
        }
        Ok(Self(format!("{SHA256_PREFIX}{}", hex.to_ascii_lowercase())))
    }

    pub fn compute(bytes: impl AsRef<[u8]>) -> Self {
        Self(format!(
            "{SHA256_PREFIX}{}",
            hex::encode(Sha256::digest(bytes.as_ref()))
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn hex(&self) -> &str {
        &self.0[SHA256_PREFIX.len()..]
    }
}

impl AsRef<str> for OciDigest {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for OciDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub fn parse_digest(value: impl AsRef<str>) -> Result<OciDigest, OciError> {
    OciDigest::parse(value)
}

pub fn validate_digest(value: impl AsRef<str>) -> Result<(), OciError> {
    parse_digest(value).map(|_| ())
}

pub fn compute_digest(bytes: impl AsRef<[u8]>) -> OciDigest {
    OciDigest::compute(bytes)
}

#[cfg(test)]
mod tests {
    use super::{compute_digest, parse_digest};
    use crate::ceres::oci::error::OciError;

    #[test]
    fn rejects_non_sha256_prefix() {
        assert_eq!(
            parse_digest(format!("sha512:{}", "a".repeat(64))),
            Err(OciError::DigestInvalid)
        );
    }

    #[test]
    fn rejects_short_hex() {
        assert_eq!(
            parse_digest(format!("sha256:{}", "a".repeat(63))),
            Err(OciError::DigestInvalid)
        );
    }

    #[test]
    fn computes_and_parses_sha256() {
        let digest = compute_digest(b"hello");
        assert_eq!(
            digest.as_str(),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(parse_digest(&digest), Ok(digest));
    }
}
