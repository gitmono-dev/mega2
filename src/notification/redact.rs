//! Redaction helpers for the notification subsystem.
//!
//! Notification delivery touches user PII (recipient addresses) and provider
//! errors. Error/diagnostic paths must not leak full recipient addresses
//! (docs/notification.md phase 0). These helpers centralise that scrubbing so
//! channels and the dispatcher log a consistent, redacted form.

/// Redact an email address for logging.
///
/// Keeps the first character of the local part and the full domain, masking the
/// rest (`alice@example.com` -> `a***@example.com`). Inputs that are empty or do
/// not look like an address are fully masked to `***`, so callers can log the
/// result unconditionally without leaking PII.
pub fn redact_email(email: &str) -> String {
    let email = email.trim();
    if email.is_empty() {
        return String::new();
    }

    match email.split_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => {
            let first = local.chars().next().unwrap_or('*');
            format!("{first}***@{domain}")
        }
        _ => "***".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_local_part_keeps_domain() {
        assert_eq!(redact_email("alice@example.com"), "a***@example.com");
        assert_eq!(redact_email("a@b"), "a***@b");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(redact_email(""), "");
        assert_eq!(redact_email("   "), "");
    }

    #[test]
    fn non_address_is_fully_masked() {
        assert_eq!(redact_email("not-an-email"), "***");
        assert_eq!(redact_email("@example.com"), "***");
        assert_eq!(redact_email("alice@"), "***");
    }

    #[test]
    fn trims_surrounding_whitespace_before_redacting() {
        assert_eq!(redact_email("  alice@example.com  "), "a***@example.com");
    }
}
