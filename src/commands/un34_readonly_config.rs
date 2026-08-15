//! UN-34: what a read-only command is allowed to do to find its config, and
//! what it then says about where the config came from.
//!
//! The loader's normal entry point writes a default `config.toml` when nothing
//! resolves, and announces it on stderr as though that were a courtesy. For a
//! read-only command it is a side effect on the very system it was asked to
//! observe — and worse, everything it then reports describes a configuration
//! this process invented rather than the one the server runs on. Both halves
//! matter, so both are refused.
//!
//! The other half of the card is provenance. A report is only worth as much as
//! the reader's ability to tell which configuration it describes, so the
//! loader's own answer is carried forward rather than re-derived downstream —
//! re-deriving from the command line would give the intent, not the resolution.

use std::{fs, path::PathBuf};

use crate::{
    commands::{LoadedConfigPaths, LoadedConfigSummary},
    config::loader::{ConfigInput, ConfigLoader, ConfigSource},
};

/// A loader input with only the named sources filled in.
///
/// `cwd_config_path` and `global_config_path` read the process's current
/// directory and `MEGA_BASE_DIR`, neither of which a test may assume anything
/// about — and changing the environment under a parallel test binary is the
/// kind of fixture that passes alone and fails in a full run. The cases that
/// need a particular ambient answer pass it in through
/// `load_readonly_with_ambient` instead.
fn input(cli_path: Option<PathBuf>) -> ConfigInput {
    ConfigInput {
        cli_path,
        env_path: None,
        cli_profile: None,
        env_profile: None,
    }
}

fn write_config(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, "[log]\nlevel = \"info\"\n").expect("write config");
    path
}

#[test]
fn un34_a_named_config_that_exists_is_loaded() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_config(temp.path(), "config.toml");

    let loaded = ConfigLoader::new(input(Some(path.clone())))
        .load_readonly()
        .expect("an existing --config must load");

    assert_eq!(loaded.path, path);
    assert_eq!(loaded.source, ConfigSource::Cli);
    assert!(loaded.profile.is_none());
}

/// A named config that is not there is an error, said at the point of
/// resolution.
///
/// `load` hands the nonexistent path straight back — nothing on that path
/// checks it — so the failure surfaces later as a read or parse error that
/// describes the wrong problem. For a command whose whole output is a claim
/// about a particular configuration, "that config is not there" is the thing
/// worth saying.
#[test]
fn un34_a_named_config_that_is_missing_is_refused() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("nowhere.toml");

    let error = ConfigLoader::new(input(Some(missing.clone())))
        .load_readonly()
        .expect_err("a missing --config must not be papered over");
    let message = error.to_string();
    assert!(
        message.contains("does not exist"),
        "the error must say what is wrong: {message}"
    );
    assert!(
        message.contains("will not generate"),
        "and why it is not being fixed: {message}"
    );

    assert!(
        !missing.exists(),
        "the refusal must not have created the file"
    );
    assert!(
        !temp.path().join("etc/config.toml").exists(),
        "nor a default anywhere else"
    );
}

/// The environment-named config is checked the same way.
///
/// `cli_path` and `env_path` are both *named* sources — chosen because someone
/// asked for them, not because a file was found — so both need the check. The
/// `cwd` and `global` sources are only ever selected after their file has been
/// seen to exist.
#[test]
fn un34_a_missing_config_from_the_environment_is_refused_too() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("nowhere.toml");

    let error = ConfigLoader::new(ConfigInput {
        cli_path: None,
        env_path: Some(missing),
        cli_profile: None,
        env_profile: None,
    })
    .load_readonly()
    .expect_err("a missing MEGA_CONFIG must not be papered over");
    assert!(error.to_string().contains("does not exist"), "{error}");
}

/// With nothing named and nothing ambient, the answer is an error — not a
/// freshly written default.
///
/// This is the branch the card is really about, and the only one where `load`
/// generates: it reaches `create_default_config`, writes a `config.toml` under
/// the mega base, and prints a note to stderr. A read-only command doing that
/// has modified the system it was asked to observe, and everything it reports
/// afterwards describes a config it invented.
#[test]
fn un34_no_config_at_all_is_an_error_rather_than_a_generated_default() {
    let error = ConfigLoader::new(input(None))
        .load_readonly_with_ambient_for_test(None, None)
        .expect_err("a read-only load with no config must fail");
    let message = error.to_string();
    assert!(
        message.contains("no config file was found"),
        "the error must say what is missing: {message}"
    );
    assert!(
        message.contains("will not generate a default"),
        "and that generating one is a decision, not an oversight: {message}"
    );
    assert!(
        message.contains("--config"),
        "and what the operator can do about it: {message}"
    );
}

/// An ambient config is used when one is there — the refusal above is about
/// there being nothing, not about read-only mode rejecting ambient sources.
#[test]
fn un34_an_ambient_config_is_still_used() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_config(temp.path(), "config.toml");

    let loaded = ConfigLoader::new(input(None))
        .load_readonly_with_ambient_for_test(Some(path.clone()), None)
        .expect("an ambient config must load");
    assert_eq!(loaded.source, ConfigSource::Cwd);
    assert_eq!(loaded.path, path);

    let loaded = ConfigLoader::new(input(None))
        .load_readonly_with_ambient_for_test(None, Some(path.clone()))
        .expect("a global config must load");
    assert_eq!(loaded.source, ConfigSource::Global);
    assert_eq!(loaded.path, path);
}

/// A named source that is missing loses to nothing — least of all to an ambient
/// config that happens to be lying around.
///
/// This is the precedence that would be easy to regress into: falling back to
/// `cwd` or `global` when the named file is absent looks helpful and is the
/// worst of the options. The operator asked for a specific config; silently
/// reporting on a different one is exactly the confusion the precheck exists to
/// prevent, and unlike the generated default it leaves no trace at all.
#[test]
fn un34_a_missing_named_config_is_not_rescued_by_an_ambient_one() {
    let temp = tempfile::tempdir().expect("temp dir");
    let ambient = write_config(temp.path(), "config.toml");
    let missing = temp.path().join("nowhere.toml");

    for named in [
        ConfigInput {
            cli_path: Some(missing.clone()),
            env_path: None,
            cli_profile: None,
            env_profile: None,
        },
        ConfigInput {
            cli_path: None,
            env_path: Some(missing.clone()),
            cli_profile: None,
            env_profile: None,
        },
    ] {
        let error = ConfigLoader::new(named)
            .load_readonly_with_ambient_for_test(Some(ambient.clone()), Some(ambient.clone()))
            .expect_err("a missing named config must not fall back to an ambient one");
        assert!(
            error.to_string().contains("does not exist"),
            "the error must be about the named config, not the ambient one: {error}"
        );
    }
}

/// The `source` reported is the one the loader actually resolved.
#[test]
fn un34_the_environment_source_is_reported_as_such() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_config(temp.path(), "config.toml");

    let loaded = ConfigLoader::new(ConfigInput {
        cli_path: None,
        env_path: Some(path.clone()),
        cli_profile: None,
        env_profile: None,
    })
    .load_readonly()
    .expect("an existing MEGA_CONFIG must load");

    assert_eq!(loaded.source, ConfigSource::Env);
    assert_eq!(loaded.path, path);
}

/// A named profile that does not exist is still rejected, as on the normal
/// path — the read-only entry point relaxes nothing.
#[test]
fn un34_a_missing_profile_is_still_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_config(temp.path(), "config.toml");

    let error = ConfigLoader::new(ConfigInput {
        cli_path: Some(path),
        env_path: None,
        cli_profile: Some("nosuchprofile".to_string()),
        env_profile: None,
    })
    .load_readonly()
    .expect_err("a missing profile must be rejected");
    assert!(error.to_string().contains("does not exist"), "{error}");
}

/// A resolved profile appears in the summary by name.
#[test]
fn un34_a_resolved_profile_is_carried_into_the_summary() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = write_config(temp.path(), "config.toml");
    let profile_path = write_config(temp.path(), "config.staging.toml");

    let loaded = ConfigLoader::new(ConfigInput {
        cli_path: Some(path.clone()),
        env_path: None,
        cli_profile: Some("staging".to_string()),
        env_profile: None,
    })
    .load_readonly()
    .expect("an existing profile must load");

    let profile = loaded.profile.as_ref().expect("profile");
    assert_eq!(profile.name, "staging");
    assert_eq!(profile.path, profile_path);

    let summary = LoadedConfigSummary {
        source: loaded.source,
        profile_name: Some(profile.name.clone()),
        paths: LoadedConfigPaths {
            config: loaded.path.clone(),
            profile: Some(profile.path.clone()),
        },
    };
    assert_eq!(summary.sanitized().profile.as_deref(), Some("staging"));
}

/// The sanitized summary's JSON shape, asserted literally.
///
/// It is frozen because reports embed it and consumers key off it; asserting
/// the serialized text rather than the struct is what makes a rename or an
/// added field fail here rather than in whatever reads the report next year.
#[test]
fn un34_the_sanitized_summary_has_exactly_the_frozen_shape() {
    let summary = LoadedConfigSummary {
        source: ConfigSource::Cli,
        profile_name: Some("staging".to_string()),
        paths: LoadedConfigPaths {
            config: PathBuf::from("/etc/mega/config.toml"),
            profile: Some(PathBuf::from("/etc/mega/config.staging.toml")),
        },
    };

    assert_eq!(
        serde_json::to_string(&summary.sanitized()).expect("serialize"),
        r#"{"source":"cli","profile":"staging"}"#
    );

    let anonymous = LoadedConfigSummary {
        profile_name: None,
        ..summary.clone()
    };
    assert_eq!(
        serde_json::to_string(&anonymous.sanitized()).expect("serialize"),
        r#"{"source":"cli","profile":null}"#,
        "no profile is an explicit null, not an absent key — a consumer \
         comparing two reports must not have to handle both shapes"
    );
}

/// No path reaches the sanitized summary (ER-11).
///
/// The paths are kept for operator diagnostics and deliberately dropped here: a
/// config path names a filesystem layout and a profile path can name a
/// deployment, and neither belongs in an artifact that travels. Asserting on
/// the serialized text is what catches a path arriving by way of a new field.
#[test]
fn un34_the_sanitized_summary_carries_no_path() {
    let summary = LoadedConfigSummary {
        source: ConfigSource::Global,
        profile_name: None,
        paths: LoadedConfigPaths {
            config: PathBuf::from("/very/telling/deployment/path/config.toml"),
            profile: Some(PathBuf::from("/very/telling/deployment/path/prod.toml")),
        },
    };

    let json = serde_json::to_string(&summary.sanitized()).expect("serialize");
    assert!(
        !json.contains("/very/telling"),
        "a path leaked into the sanitized summary: {json}"
    );
    assert!(!json.contains("config.toml"), "{json}");
    assert!(!json.contains("prod.toml"), "{json}");
    assert_eq!(json, r#"{"source":"global","profile":null}"#);
}

/// Every source has a wire name, and they are the frozen ones.
#[test]
fn un34_every_source_has_its_frozen_wire_name() {
    for (source, expected) in [
        (ConfigSource::Cli, "cli"),
        (ConfigSource::Env, "env"),
        (ConfigSource::Cwd, "cwd"),
        (ConfigSource::Global, "global"),
        (ConfigSource::DefaultGenerated, "default_generated"),
    ] {
        assert_eq!(source.as_str(), expected);
    }
}
