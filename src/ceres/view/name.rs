#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ViewNameError {
    #[error("view name segments must be nonempty")]
    EmptySegment,
    #[error("view name segments must not be dot or dot-dot")]
    DotSegment,
    #[error(
        "view name segments may contain only ASCII letters, digits, dot, underscore, or hyphen"
    )]
    InvalidCharacter,
    #[error("the final view name segment must not end in .git")]
    GitSuffix,
}

pub(crate) fn validate_view_name(name: &str) -> Result<(), ViewNameError> {
    let mut last = None;
    for segment in name.split('/') {
        if segment.is_empty() {
            return Err(ViewNameError::EmptySegment);
        }
        if matches!(segment, "." | "..") {
            return Err(ViewNameError::DotSegment);
        }
        if !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ViewNameError::InvalidCharacter);
        }
        last = Some(segment);
    }
    if last.is_some_and(|segment| segment.ends_with(".git")) {
        return Err(ViewNameError::GitSuffix);
    }
    Ok(())
}
