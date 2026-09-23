use idgenerator::IdInstance;
use regex::Regex;
use serde_json::{Value, json};

use crate::{common::errors::MegaError, jupiter::utils::id_generator};

pub const ZERO_ID: &str = match std::str::from_utf8(&[b'0'; 40]) {
    Ok(s) => s,
    Err(_) => panic!("can't get ZERO_ID"),
};

/// All-zero object id in either SHA-1 (40) or SHA-256/BLAKE3 (64) wire width.
pub fn is_protocol_zero_id(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.as_bytes().iter().all(|&b| b == b'0')
}

/// Returns true if `oid` looks like a full Git object id in hex form.
///
/// We intentionally only accept full-hex ids here (no short ids), because short ids
/// require repository-specific disambiguation.
///
/// Accepted lengths are 40 or 64 hex characters. **Length is not hash kind**:
/// 40 hex is not automatically SHA-1, and 64 hex is not SHA-256 (BLAKE3-256 is
/// also 64 hex). Callers that construct an [`git_internal::hash::ObjectHash`]
/// must use `ObjectHash::from_hex_for_kind` with an explicit kind.
pub fn is_full_hex_object_id(oid: &str) -> bool {
    let is_valid_len = oid.len() == 40 || oid.len() == 64;
    is_valid_len && oid.as_bytes().iter().all(|b| b.is_ascii_hexdigit())
}

pub fn generate_id() -> i64 {
    id_generator::ensure_initialized();
    IdInstance::next_id()
}

pub const MEGA_BRANCH_NAME: &str = "refs/heads/main";

pub fn generate_rich_text(content: &str) -> String {
    let json_str = r#"
    {
        "root": {
            "children": [{
                "children": [{ "detail": 0, "format": 0, "mode": "normal", "style": "", "text": "", "type": "text", "version": 1 }],
                "direction": "ltr", "format": "", "indent": 0, "type": "paragraph", "version": 1, "textFormat": 0, "textStyle": ""
            }], "direction": "ltr", "format": "", "indent": 0, "type": "root", "version": 1
        }
    }"#;
    let mut data: Value = serde_json::from_str(json_str).expect("Invalid JSON");

    if let Some(text_value) = data["root"]["children"][0]["children"][0].get_mut("text") {
        *text_value = json!(content);
    }
    serde_json::to_string_pretty(&data).expect("Failed to serialize JSON")
}

pub fn cl_ref_name(cl_link: &str) -> String {
    format!("refs/cl/{cl_link}")
}

/// Format commit message with GPG signature<br>
/// There must be a `blank line`(\n) before `message`, or remote unpack failed.<br>
/// If there is `GPG signature`,
/// `blank line` should be placed between `signature` and `message`
pub fn format_commit_msg(msg: &str, gpg_sig: Option<&str>) -> String {
    match gpg_sig {
        None => {
            format!("\n{msg}")
        }
        Some(gpg) => {
            format!("{gpg}\n\n{msg}")
        }
    }
}

/// A git-internal `Commit::message` split into its extra header fields and
/// its message body (plan-20260923 ADR-FU-01).
///
/// git-internal stores everything after the `committer` line in `message`:
/// extra headers such as `gpgsig` / `gpgsig-sha256` (with space-prefixed
/// continuation lines), the header/body blank line, and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMessageParts<'a> {
    /// Extra header fields in order; continuation lines are joined with `\n`
    /// (without their leading space).
    pub headers: Vec<(&'a str, String)>,
    /// Message body bytes exactly as stored (never trimmed).
    pub body: &'a str,
}

impl CommitMessageParts<'_> {
    /// Whether any extra header is a commit signature.
    pub fn has_signature(&self) -> bool {
        self.headers.iter().any(|(key, _)| is_signature_header(key))
    }
}

/// Commit header keys that carry a detached signature.
pub fn is_signature_header(key: &str) -> bool {
    matches!(key, "gpgsig" | "gpgsig-sha256")
}

/// Split a git-internal commit `message` into extra headers and body using
/// Git's header grammar: `key SP value` lines (key `[A-Za-z0-9-]+`) and
/// space-prefixed continuation lines, terminated by an empty line.
///
/// A message that starts with `\n` has no extra headers. Anything that does
/// not parse as a complete header block (including historical unframed
/// messages such as `create new directory demo`) is returned whole as the
/// body. Bytes are never trimmed and CRLF is preserved.
pub fn split_commit_message(message: &str) -> CommitMessageParts<'_> {
    let whole = || CommitMessageParts {
        headers: Vec::new(),
        body: message,
    };
    if let Some(body) = message.strip_prefix('\n') {
        return CommitMessageParts {
            headers: Vec::new(),
            body,
        };
    }
    let mut headers: Vec<(&str, String)> = Vec::new();
    let mut rest = message;
    loop {
        let Some(newline) = rest.find('\n') else {
            return whole();
        };
        let line = &rest[..newline];
        let after = &rest[newline + 1..];
        if line.is_empty() {
            return CommitMessageParts {
                headers,
                body: after,
            };
        }
        if let Some(continuation) = line.strip_prefix(' ') {
            let Some((_, value)) = headers.last_mut() else {
                return whole();
            };
            value.push('\n');
            value.push_str(continuation);
        } else {
            match line.split_once(' ') {
                Some((key, value))
                    if !key.is_empty()
                        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') =>
                {
                    headers.push((key, value.to_owned()));
                }
                _ => return whole(),
            }
        }
        rest = after;
    }
}

/// First non-empty line of a commit message body, trimmed (display only).
pub fn commit_body_subject(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

// check if the commit message is conventional commit
// ref: https://www.conventionalcommits.org/en/v1.0.0/
pub fn check_conventional_commits_message(msg: &str) -> bool {
    let first_line = msg.lines().next().unwrap_or_default();
    #[allow(unused_variables)]
    let body_footer = msg.lines().skip(1).collect::<Vec<_>>().join("\n");

    let unicode_pattern = r"\p{L}\p{N}\p{P}\p{S}\p{Z}";
    // type only support characters&numbers, others fields support all unicode characters
    let regex_str = format!(
        r"^(?P<type>[\p{{L}}\p{{N}}_-]+)(?:\((?P<scope>[{unicode_pattern}]+)\))?!?: (?P<description>[{unicode_pattern}]+)$",
    );

    let re = Regex::new(&regex_str).unwrap();
    const RECOMMENDED_TYPES: [&str; 8] = [
        "build", "chore", "ci", "docs", "feat", "fix", "perf", "refactor",
    ];

    if let Some(captures) = re.captures(first_line) {
        let commit_type = captures.name("type").map(|m| m.as_str().to_string());
        #[allow(unused_variables)]
        let scope = captures.name("scope").map(|m| m.as_str().to_string());
        let description = captures.name("description").map(|m| m.as_str().to_string());
        if commit_type.is_none() || description.is_none() {
            return false;
        }

        let commit_type = commit_type.unwrap();
        if !RECOMMENDED_TYPES.contains(&commit_type.to_lowercase().as_str()) {
            println!(
                "`{commit_type}` is not a recommended commit type, refer to https://www.conventionalcommits.org/en/v1.0.0/ for more information"
            );
        }

        // println!("{}({}): {}\n{}", commit_type, scope.unwrap_or("None".to_string()), description.unwrap(), body_footer);

        return true;
    }
    false
}

pub fn get_current_bin_name() -> String {
    let bin_path = std::env::args().next().unwrap_or_default();
    std::path::Path::new(&bin_path)
        .file_name()
        .and_then(|os_str| os_str.to_str())
        .unwrap_or("unknown")
        .to_owned()
}

/// Escape `%`, `_`, and `\` for SQL `LIKE` patterns that use `\` as the escape
/// character (`ESCAPE '\'` / sea-orm `LikeExpr` with backslash escape).
///
/// Order matters: backslash must be escaped first so newly inserted escape
/// markers are not double-processed.
pub fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

/// Canonicalize a monorepo ref path for I3 / attach prechecks.
///
/// Collapses repeated `/`, strips `.` segments and trailing `/`, and requires a
/// leading `/`. Rejects empty, `..`, and backslash paths so aliases cannot
/// bypass path-equality checks against `mega_refs.path`.
pub fn canonicalize_mono_ref_path(path: &str) -> Result<String, MegaError> {
    let s = path.trim();
    if s.is_empty() {
        return Err(MegaError::Other("path cannot be empty".into()));
    }
    if s.split('/').any(|p| p == "..") {
        return Err(MegaError::Other(format!("path traversal not allowed: {s}")));
    }
    if s.contains('\\') {
        return Err(MegaError::Other(format!(
            "path must use '/' separator: {s}"
        )));
    }
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        return Ok("/".to_owned());
    }
    let parts: Vec<&str> = s
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() {
        return Err(MegaError::Other(
            "path cannot be empty or consist only of '.' segments".into(),
        ));
    }
    Ok(format!("/{}", parts.join("/")))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_is_full_hex_object_id() {
        // Shape-only: 40 hex (SHA-1 width). Length does not imply kind.
        assert!(is_full_hex_object_id(&"0".repeat(40)));
        assert!(is_full_hex_object_id(&"a".repeat(40)));
        assert!(is_full_hex_object_id(&"A".repeat(40)));
        assert!(is_full_hex_object_id(&"f".repeat(40)));
        assert!(is_full_hex_object_id(&"F".repeat(40)));

        // Shape-only: 64 hex (SHA-256 or BLAKE3-256 width). Length ≠ kind.
        assert!(is_full_hex_object_id(&"0".repeat(64)));
        let sha256 = "abcdef".repeat(10) + "abcd"; // 60 + 4 = 64
        assert!(is_full_hex_object_id(&sha256));

        // Invalid lengths (we don't accept short ids)
        assert!(!is_full_hex_object_id(""));
        assert!(!is_full_hex_object_id(&"0".repeat(39)));
        assert!(!is_full_hex_object_id(&"0".repeat(41)));
        assert!(!is_full_hex_object_id(&"0".repeat(63)));
        assert!(!is_full_hex_object_id(&"0".repeat(65)));

        // Invalid characters
        assert!(!is_full_hex_object_id(
            &(String::from("g") + &"0".repeat(39))
        ));
        assert!(!is_full_hex_object_id(
            &(String::from("-") + &"0".repeat(39))
        ));
        assert!(!is_full_hex_object_id(
            &(String::from(" ") + &"0".repeat(39))
        ));
        assert!(!is_full_hex_object_id(
            &(String::from("é") + &"0".repeat(39))
        ));
    }

    #[test]
    fn test_check_conventional_commits() {
        // successfull cases
        let msg = "feat: add new feature";
        assert!(check_conventional_commits_message(msg));

        let msg = "fix(common crate): bug fix";
        assert!(check_conventional_commits_message(msg));

        let msg = "chore(范围)!: 依存関係を更新する";
        assert!(check_conventional_commits_message(msg));

        let msg = "se_lf-ty9pe(scope)!: Description\n\n여기 시체가 있어요\n\nвот нога";
        assert!(check_conventional_commits_message(msg));

        let msg = "feat(scope)!: Description\n\n\nbody one\n\nbody two\n\nfooter";
        assert!(check_conventional_commits_message(msg));

        // failed casesmsg
        let msg = "feat:add new feature"; // missing ' ' before ':'
        assert!(!check_conventional_commits_message(msg));

        let msg = "fix(common crate)bug fix"; // missing ':'
        assert!(!check_conventional_commits_message(msg));

        let msg = "類@型(common): add new feature"; // unssupported characters in type
        assert!(!check_conventional_commits_message(msg));

        let msg = "()(common): add new feature"; // unssupported characters in type
        assert!(!check_conventional_commits_message(msg));
    }

    #[test]
    fn escape_like_escapes_percent_underscore_and_backslash() {
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("a%b"), r"a\%b");
        assert_eq!(escape_like("a_b"), r"a\_b");
        assert_eq!(escape_like(r"a\b"), r"a\\b");
        assert_eq!(escape_like(r"%_\"), r"\%\_\\");
        assert_eq!(escape_like(r"foo\%bar"), r"foo\\\%bar");
    }

    const PGP_ARMOR: &str = "gpgsig -----BEGIN PGP SIGNATURE-----\n \n wsBcBAABCAAQBQJ\n =abcd\n -----END PGP SIGNATURE-----\n";

    #[test]
    fn split_commit_message_grammar() {
        // PGP `gpgsig` with multi-line continuations, then body.
        let msg = format!("{PGP_ARMOR}\nfeat: subject\n\nbody\n");
        let parts = split_commit_message(&msg);
        assert_eq!(parts.body, "feat: subject\n\nbody\n");
        assert!(parts.has_signature());
        assert_eq!(parts.headers.len(), 1);
        assert!(parts.headers[0].1.ends_with("-----END PGP SIGNATURE-----"));

        // `gpgsig-sha256` is a signature header too.
        let msg = "gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n x\n -----END PGP SIGNATURE-----\n\nsha256 subject\n";
        let parts = split_commit_message(msg);
        assert_eq!(parts.body, "sha256 subject\n");
        assert!(parts.has_signature());

        // SSH armor.
        let msg = "gpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lH\n -----END SSH SIGNATURE-----\n\nssh subject\n";
        assert_eq!(split_commit_message(msg).body, "ssh subject\n");

        // Truncated armor still parses deterministically by the continuation grammar.
        let msg = "gpgsig -----BEGIN PGP SIGNATURE-----\n wsBc\n\ntruncated subject\n";
        let parts = split_commit_message(msg);
        assert_eq!(parts.body, "truncated subject\n");
        assert!(parts.has_signature());

        // No extra headers: git-internal keeps the separator as a leading newline.
        let parts = split_commit_message("\nplain subject\n");
        assert_eq!(parts.body, "plain subject\n");
        assert!(parts.headers.is_empty());

        // A body whose first line starts with `gpgsig` stays body.
        let parts = split_commit_message("\ngpgsig is a word here\n");
        assert_eq!(parts.body, "gpgsig is a word here\n");
        assert!(!parts.has_signature());

        // Unknown header plus signature.
        let msg = format!("encoding ISO-8859-1\n{PGP_ARMOR}\nmixed subject\n");
        let parts = split_commit_message(&msg);
        assert_eq!(parts.body, "mixed subject\n");
        assert_eq!(parts.headers[0], ("encoding", "ISO-8859-1".to_owned()));
        assert!(parts.has_signature());

        // Historical unframed message (no header/body separator) is all body.
        let parts = split_commit_message("create new directory demo");
        assert_eq!(parts.body, "create new directory demo");
        assert!(parts.headers.is_empty());

        // CRLF body bytes are preserved exactly.
        let msg = format!("{PGP_ARMOR}\ncrlf subject\r\n\r\nbody\r\n");
        assert_eq!(
            split_commit_message(&msg).body,
            "crlf subject\r\n\r\nbody\r\n"
        );

        // Empty body.
        let msg = format!("{PGP_ARMOR}\n");
        assert_eq!(split_commit_message(&msg).body, "");

        assert_eq!(
            commit_body_subject("\n\n  first line  \nsecond\n"),
            "first line"
        );
        assert_eq!(commit_body_subject(""), "");
    }

    #[test]
    fn format_commit_msg_frames_body() {
        use std::str::FromStr;

        use git_internal::internal::object::{ObjectTrait, commit::Commit};

        assert_eq!(
            format_commit_msg("create new directory demo", None),
            "\ncreate new directory demo"
        );
        let tree =
            git_internal::hash::ObjectHash::from_str("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
                .unwrap();
        for msg in [
            "create new directory demo",
            "subject\n\nbody\n",
            // A user message that looks like a header block stays body.
            "gpgsig forged\n\nreal subject",
            "",
        ] {
            let framed = format_commit_msg(msg, None);
            let parts = split_commit_message(&framed);
            assert!(parts.headers.is_empty(), "{msg:?}");
            assert_eq!(parts.body, msg);
            let commit = Commit::from_tree_id(tree, vec![], &framed);
            let raw = String::from_utf8(commit.to_data().unwrap()).unwrap();
            let (header, body) = raw.split_once("\n\n").expect("header/body blank line");
            assert!(header.lines().last().unwrap().starts_with("committer "));
            assert_eq!(body, msg);
        }
    }
}
