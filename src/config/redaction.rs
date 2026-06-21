use url::Url;

const REDACTED: &str = "***";

/// Scrubs sensitive substrings from a string before it reaches logs/errors.
///
/// Implementations must be cheap and infallible so callers can wrap any
/// log/error payload unconditionally (vault.md log-redaction tooling). The
/// default [`UrlRedactor`] strips URL userinfo, which covers the connection
/// strings (DB/Redis/object storage) and provider errors that most commonly
/// embed credentials. Secret *values* themselves are handled by `SecretRef` /
/// `SecretString`, which self-redact, so no value-masking redactor is needed
/// here.
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
}
