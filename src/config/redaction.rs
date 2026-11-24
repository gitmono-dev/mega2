use url::Url;

const REDACTED: &str = "***";

/// Scrubs sensitive substrings from a string before it reaches logs/errors.
///
/// Implementations must be cheap and infallible so callers can wrap any
/// log/error payload unconditionally (vault.md log-redaction tooling). The
/// default [`UrlRedactor`] strips URL userinfo, which covers the connection
/// strings (DB/Redis/object storage) and provider errors that most commonly
/// embed credentials. Secret *values* held in `SecretString` self-redact via
/// their `Debug`/`Serialize` impls; `SecretRef` redacts via `Debug`/`Display`
/// but serializes its URI for config round-trip. Raw credential values that
/// must appear in diagnostics (e.g. an S3 access key id logged on a build
/// failure) are masked by the standalone [`redact_secret_value`] helper rather
/// than by this trait.
pub trait Redactor: Send + Sync {
    fn redact(&self, input: &str) -> String;
}

/// The default redactor: strips URL userinfo from connection-string-like inputs.
#[derive(Debug, Default, Clone, Copy)]
pub struct UrlRedactor;

impl Redactor for UrlRedactor {
    fn redact(&self, input: &str) -> String {
        redact_url(input)
    }
}

/// Process-wide default [`Redactor`].
pub fn global_redactor() -> &'static dyn Redactor {
    static REDACTOR: UrlRedactor = UrlRedactor;
    &REDACTOR
}

/// Redact a database connection URL (alias of [`redact_url`] for call-site clarity).
pub fn redact_db_url(input: &str) -> String {
    redact_url(input)
}

/// Redact a Redis connection URL (alias of [`redact_url`] for call-site clarity).
pub fn redact_redis_url(input: &str) -> String {
    redact_url(input)
}

pub fn redact_object_storage_endpoint(input: &str) -> String {
    redact_url(input)
}

/// Completely masks `ci_checks.hmac_secret` for logs and diagnostics without revealing any prefix.
pub fn redact_ci_checks_hmac_secret(secret: Option<&super::secret::SecretString>) -> &'static str {
    match secret {
        Some(_) => REDACTED,
        None => "<unset>",
    }
}

pub fn redact_secret_value(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.chars().count() <= 4 {
        return REDACTED.to_string();
    }
    let prefix_end = trimmed
        .char_indices()
        .nth(4)
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    format!("{}{}", &trimmed[..prefix_end], REDACTED)
}

pub fn redact_url(input: &str) -> String {
    match Url::parse(input) {
        Ok(mut url) => {
            redact_url_userinfo(&mut url);
            url.to_string()
        }
        Err(_) => redact_unparseable_url(input),
    }
}

fn redact_url_userinfo(url: &mut Url) {
    if !url.username().is_empty() {
        let _ = url.set_username(REDACTED);
    }

    if url.password().is_some() {
        let _ = url.set_password(Some(REDACTED));
    }
}

fn redact_unparseable_url(input: &str) -> String {
    let Some(scheme_end) = input.find("://") else {
        return input.to_owned();
    };
    let authority_start = scheme_end + "://".len();
    let Some(at_offset) = input[authority_start..].find('@') else {
        return input.to_owned();
    };

    let tail_start = authority_start + at_offset + '@'.len_utf8();
    format!(
        "{}://{}@{}",
        &input[..scheme_end],
        REDACTED,
        &input[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_url_username_and_password() {
        let redacted = redact_url("postgres://mono:mono@localhost:5432/mono");

        assert_eq!(redacted, "postgres://***:***@localhost:5432/mono");
        assert!(!redacted.contains("mono:mono"));
    }

    #[test]
    fn redacts_url_password_with_empty_username() {
        let redacted = redact_url("redis://:secret@127.0.0.1:6379/0");

        assert_eq!(redacted, "redis://:***@127.0.0.1:6379/0");
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn preserves_url_without_userinfo() {
        let redacted = redact_url("redis://127.0.0.1:6379/0");

        assert_eq!(redacted, "redis://127.0.0.1:6379/0");
    }

    #[test]
    fn redacts_unparseable_url_userinfo_conservatively() {
        let redacted = redact_url("postgres://mono:secret@[::1");

        assert_eq!(redacted, "postgres://***@[::1");
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn db_and_redis_url_aliases_match_redact_url() {
        let db = "postgres://mono:pw@localhost:5432/mono";
        let redis = "redis://:pw@127.0.0.1:6379/0";
        assert_eq!(redact_db_url(db), redact_url(db));
        assert_eq!(redact_redis_url(redis), redact_url(redis));
        assert!(!redact_db_url(db).contains("pw"));
    }

    #[test]
    fn global_redactor_strips_userinfo() {
        let redacted = global_redactor().redact("postgres://mono:pw@localhost/mono");
        assert_eq!(redacted, "postgres://***:***@localhost/mono");
    }

    #[test]
    fn object_storage_endpoint_alias_strips_userinfo() {
        let redacted = redact_object_storage_endpoint("https://key:secret@s3.example.com:9000");
        assert_eq!(redacted, "https://***:***@s3.example.com:9000/");
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn redact_secret_value_keeps_short_prefix() {
        assert_eq!(redact_secret_value("AKIAIOSFODNN7EXAMPLE"), "AKIA***");
        assert!(!redact_secret_value("AKIAIOSFODNN7EXAMPLE").contains("EXAMPLE"));
    }

    #[test]
    fn redact_secret_value_masks_short_inputs_fully() {
        assert_eq!(redact_secret_value("pw"), REDACTED);
        assert_eq!(redact_secret_value("abcd"), REDACTED);
        assert_eq!(redact_secret_value(""), REDACTED);
        assert_eq!(redact_secret_value("  ab  "), REDACTED);
    }

    #[test]
    fn redact_secret_value_counts_chars_not_bytes_for_non_ascii() {
        assert_eq!(redact_secret_value("你好"), REDACTED);
        assert_eq!(redact_secret_value("你好世界xyz"), "你好世界***");
    }

    #[test]
    fn ci_checks_hmac_secret_is_fully_redacted_in_debug_serialize_and_logs() {
        let secret_plain = "super-secret-ci-hmac-key-0123456789abcdef";
        let cfg = crate::config::CiChecksConfig {
            enabled: true,
            hmac_secret: Some(crate::config::secret::SecretString::new(secret_plain)),
            max_skew_secs: 300,
            nonce_retention_secs: 604_800,
        };

        let dbg_out = format!("{cfg:?}");
        assert!(
            !dbg_out.contains(secret_plain),
            "Debug leaked secret: {dbg_out}"
        );
        assert!(
            !dbg_out.contains("super-secret"),
            "Debug leaked prefix: {dbg_out}"
        );
        assert!(dbg_out.contains(REDACTED), "{dbg_out}");

        let json_out = serde_json::to_string(&cfg).expect("serialize CiChecksConfig");
        assert!(
            !json_out.contains(secret_plain),
            "JSON leaked secret: {json_out}"
        );
        assert!(
            !json_out.contains("super-secret"),
            "JSON leaked prefix: {json_out}"
        );
        assert!(json_out.contains(REDACTED), "{json_out}");

        assert_eq!(
            redact_ci_checks_hmac_secret(cfg.hmac_secret.as_ref()),
            REDACTED
        );
        assert_eq!(redact_ci_checks_hmac_secret(None), "<unset>");
    }
}
