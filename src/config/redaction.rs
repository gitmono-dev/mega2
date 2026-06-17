use url::Url;

const REDACTED: &str = "***";

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
}
