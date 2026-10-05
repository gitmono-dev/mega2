#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(super) enum InitializationTargetError {
    #[error(
        "MST2_NATIVE_INIT_UNSUPPORTED_OBJECT_FORMAT: {0}; this maintenance entry supports sha1 only"
    )]
    UnsupportedObjectFormat(String),
    #[error("MST2_NATIVE_INIT_INVALID_INSTANCE: both deployment instances must be non-nil UUIDs")]
    InvalidInstance,
    #[error("MST2_NATIVE_INIT_INSTANCE_MISMATCH: --instance differs from mst2.instance_uuid")]
    InstanceMismatch,
    #[error(
        "MST2_NATIVE_INIT_INVALID_ROOT: expected commit and tree must be canonical SHA-1 object IDs"
    )]
    InvalidRoot,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct InitializationTarget {
    pub(super) instance: String,
    pub(super) commit: String,
    pub(super) tree: String,
}

impl InitializationTarget {
    pub(super) fn parse(
        object_format: &str,
        configured_instance: Option<&str>,
        requested_instance: &str,
        commit: &str,
        tree: &str,
    ) -> Result<Self, InitializationTargetError> {
        if object_format != "sha1" {
            return Err(InitializationTargetError::UnsupportedObjectFormat(
                object_format.into(),
            ));
        }
        let parse_instance = |value: &str| {
            uuid::Uuid::parse_str(value)
                .ok()
                .filter(|id| !id.is_nil())
                .ok_or(InitializationTargetError::InvalidInstance)
        };
        let configured =
            parse_instance(configured_instance.ok_or(InitializationTargetError::InvalidInstance)?)?;
        let requested = parse_instance(requested_instance)?;
        if configured != requested {
            return Err(InitializationTargetError::InstanceMismatch);
        }
        if [commit, tree].iter().any(|value| {
            value.len() != 40
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(InitializationTargetError::InvalidRoot);
        }
        Ok(Self {
            instance: configured.to_string(),
            commit: commit.into(),
            tree: tree.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTANCE: &str = "6ab219b0-4275-45ba-9d7b-7b0b633018cd";

    #[test]
    fn target_is_canonical_and_bound_to_the_configured_instance() {
        let target = InitializationTarget::parse(
            "sha1",
            Some(INSTANCE),
            &INSTANCE.to_uppercase(),
            &"a".repeat(40),
            &"b".repeat(40),
        )
        .unwrap();
        assert_eq!(target.instance, INSTANCE);
        assert_eq!(target.commit, "a".repeat(40));
        assert_eq!(target.tree, "b".repeat(40));
    }

    #[test]
    fn target_rejects_other_formats_without_inferring_from_equal_width() {
        for format in ["sha256", "blake3", "unknown"] {
            assert_eq!(
                InitializationTarget::parse(
                    format,
                    Some(INSTANCE),
                    INSTANCE,
                    &"a".repeat(64),
                    &"b".repeat(64),
                ),
                Err(InitializationTargetError::UnsupportedObjectFormat(
                    format.into()
                ))
            );
        }
    }

    #[test]
    fn target_rejects_nil_missing_invalid_or_different_instances() {
        for instance in [
            None,
            Some("invalid"),
            Some("00000000-0000-0000-0000-000000000000"),
        ] {
            assert_eq!(
                InitializationTarget::parse(
                    "sha1",
                    instance,
                    INSTANCE,
                    &"a".repeat(40),
                    &"b".repeat(40)
                ),
                Err(InitializationTargetError::InvalidInstance)
            );
        }
        for instance in ["invalid", "00000000-0000-0000-0000-000000000000"] {
            assert_eq!(
                InitializationTarget::parse(
                    "sha1",
                    Some(INSTANCE),
                    instance,
                    &"a".repeat(40),
                    &"b".repeat(40)
                ),
                Err(InitializationTargetError::InvalidInstance)
            );
        }
        assert_eq!(
            InitializationTarget::parse(
                "sha1",
                Some(INSTANCE),
                "11111111-2222-4333-8444-555555555555",
                &"a".repeat(40),
                &"b".repeat(40)
            ),
            Err(InitializationTargetError::InstanceMismatch)
        );
    }

    #[test]
    fn target_rejects_noncanonical_or_wrong_width_object_ids() {
        for value in [
            "a".repeat(39),
            "a".repeat(64),
            "A".repeat(40),
            "z".repeat(40),
        ] {
            for (commit, tree) in [(&value, &"b".repeat(40)), (&"a".repeat(40), &value)] {
                assert_eq!(
                    InitializationTarget::parse("sha1", Some(INSTANCE), INSTANCE, commit, tree),
                    Err(InitializationTargetError::InvalidRoot)
                );
            }
        }
    }
}
