use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use toml::Value;

use crate::{
    common::utils::get_current_bin_name,
    config::{mega_base, template::default_config_template},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Cli,
    Env,
    Cwd,
    Global,
    DefaultGenerated,
}

impl ConfigSource {
    /// The wire name of this source.
    ///
    /// Frozen (UN-34): it is the `source` field of the sanitized provenance
    /// summary that audit reports embed, so a consumer can key off it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Env => "env",
            Self::Cwd => "cwd",
            Self::Global => "global",
            Self::DefaultGenerated => "default_generated",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub source: ConfigSource,
    pub profile: Option<LoadedProfileConfig>,
}

#[derive(Debug, Clone)]
pub struct LoadedProfileConfig {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Default)]
pub struct ConfigInput {
    /// CLI --config
    pub cli_path: Option<PathBuf>,

    /// ENV: MEGA_CONFIG
    pub env_path: Option<PathBuf>,

    /// CLI --profile
    pub cli_profile: Option<String>,

    /// ENV: MEGA_PROFILE
    pub env_profile: Option<String>,
}

pub struct ConfigLoader {
    input: ConfigInput,
}

impl ConfigLoader {
    pub fn new(input: ConfigInput) -> Self {
        Self { input }
    }

    /// Resolve the config without creating anything (UN-34).
    ///
    /// [`Self::load`] writes a default `config.toml` when no source resolves,
    /// and reports it on stderr as if that were a courtesy. For a read-only
    /// command it is a side effect on the very system it was asked to observe,
    /// and the config it would then read is one this process just invented
    /// rather than the one the server runs on. Both cases are refused here:
    ///
    /// * an explicit `--config` (or `MEGA_CONFIG`) naming a file that does not
    ///   exist — `load` hands the path back regardless and the failure surfaces
    ///   later as a read or parse error, which describes the wrong problem;
    /// * no source at all, which is where the generation happens.
    ///
    /// A profile that does not exist is still rejected by `loaded_config`, as
    /// on the normal path.
    pub fn load_readonly(&self) -> Result<LoadedConfig> {
        self.load_readonly_with_ambient(Self::cwd_config_path()?, Self::global_config_path()?)
    }

    /// The body of [`Self::load_readonly`], with the two ambient lookups passed
    /// in.
    ///
    /// The ambient sources read the process's current directory and
    /// `MEGA_BASE_DIR`. Neither is something a test may assume or safely change
    /// — mutating the environment under a parallel test binary is exactly the
    /// kind of order-dependent fixture that passes alone and fails in a full
    /// run. Taking them as arguments is what lets the "nothing resolves at all"
    /// branch be exercised for what it is.
    ///
    /// Private on purpose: the only production entry point is
    /// [`Self::load_readonly`], which supplies the real lookups. A caller that
    /// could pass its own ambient paths could pass one that does not exist and
    /// slip past the existence check the named sources get.
    fn load_readonly_with_ambient(
        &self,
        cwd_path: Option<PathBuf>,
        global_path: Option<PathBuf>,
    ) -> Result<LoadedConfig> {
        let profile_name = self.profile_name()?;

        if let Some(path) = &self.input.cli_path {
            return self.readonly_loaded_config(path.clone(), ConfigSource::Cli, profile_name);
        }

        if let Some(path) = &self.input.env_path {
            return self.readonly_loaded_config(path.clone(), ConfigSource::Env, profile_name);
        }

        if let Some(path) = cwd_path {
            return self.loaded_config(path, ConfigSource::Cwd, profile_name);
        }

        if let Some(path) = global_path {
            return self.loaded_config(path, ConfigSource::Global, profile_name);
        }

        anyhow::bail!(
            "no config file was found, and a read-only command will not generate a default one; \
             pass --config <path> or set MEGA_CONFIG"
        )
    }

    /// The `cwd` and `global` sources are only chosen because the file was
    /// found; `cli` and `env` are chosen because they were *named*, and nothing
    /// on the normal path checks that the named file is there. Saying so at the
    /// point of resolution beats a parse error two layers down.
    fn readonly_loaded_config(
        &self,
        path: PathBuf,
        source: ConfigSource,
        profile_name: Option<String>,
    ) -> Result<LoadedConfig> {
        if !path.exists() {
            anyhow::bail!(
                "config file `{}` (from {}) does not exist; a read-only command will not \
                 generate a default one",
                path.display(),
                source.as_str()
            );
        }

        self.loaded_config(path, source, profile_name)
    }

    /// [`Self::load_readonly_with_ambient`], reachable from this crate's tests
    /// only.
    #[cfg(test)]
    pub(crate) fn load_readonly_with_ambient_for_test(
        &self,
        cwd_path: Option<PathBuf>,
        global_path: Option<PathBuf>,
    ) -> Result<LoadedConfig> {
        self.load_readonly_with_ambient(cwd_path, global_path)
    }

    /// Load config path, create default config if not exists
    pub fn load(&self) -> Result<LoadedConfig> {
        let profile_name = self.profile_name()?;

        if let Some(path) = &self.input.cli_path {
            return self.loaded_config(path.clone(), ConfigSource::Cli, profile_name);
        }

        if let Some(path) = &self.input.env_path {
            return self.loaded_config(path.clone(), ConfigSource::Env, profile_name);
        }

        if let Some(path) = Self::cwd_config_path()? {
            return self.loaded_config(path, ConfigSource::Cwd, profile_name);
        }

        if let Some(path) = Self::global_config_path()? {
            return self.loaded_config(path, ConfigSource::Global, profile_name);
        }

        let path = self.create_default_config()?;
        self.loaded_config(path, ConfigSource::DefaultGenerated, profile_name)
    }

    fn profile_name(&self) -> Result<Option<String>> {
        let profile = self
            .input
            .cli_profile
            .as_deref()
            .or(self.input.env_profile.as_deref())
            .map(str::trim)
            .filter(|profile| !profile.is_empty());

        match profile {
            Some(profile) => {
                validate_profile_name(profile)?;
                Ok(Some(profile.to_string()))
            }
            None => Ok(None),
        }
    }

    fn loaded_config(
        &self,
        path: PathBuf,
        source: ConfigSource,
        profile_name: Option<String>,
    ) -> Result<LoadedConfig> {
        let profile = profile_name
            .map(|name| {
                let profile_path = profile_path_for(&path, &name)?;
                if !profile_path.exists() {
                    anyhow::bail!(
                        "profile config `{}` for profile `{}` does not exist",
                        profile_path.display(),
                        name
                    );
                }

                Ok(LoadedProfileConfig {
                    name,
                    path: profile_path,
                })
            })
            .transpose()?;

        Ok(LoadedConfig {
            path,
            source,
            profile,
        })
    }

    fn cwd_config_path() -> Result<Option<PathBuf>> {
        let cwd = env::current_dir().context("failed to get current dir")?;
        let path = cwd.join("config/config.toml");
        Ok(path.exists().then_some(path))
    }

    fn global_config_path() -> Result<Option<PathBuf>> {
        let path = mega_base().join("etc/config.toml");
        Ok(path.exists().then_some(path))
    }

    fn create_default_config(&self) -> Result<PathBuf> {
        let base_dir = mega_base();
        let etc_dir = base_dir.join("etc");
        fs::create_dir_all(&etc_dir).with_context(|| format!("failed to create {:?}", etc_dir))?;

        let bin_name = get_current_bin_name();
        let template = default_config_template(&bin_name)
            .with_context(|| format!("no default config template for binary `{}`", bin_name))?;

        let config = Self::render_template(template, &base_dir)?;
        let config_path = etc_dir.join("config.toml");

        fs::write(&config_path, config)
            .with_context(|| format!("failed to write {:?}", config_path))?;

        eprintln!(
            "config.toml not found, created default config at {:?}",
            config_path
        );

        Ok(config_path)
    }

    fn render_template(template: &str, base_dir: &Path) -> Result<String> {
        let mut value: Value =
            toml::from_str(template).context("failed to parse default config template")?;

        value["base_dir"] = Value::String(base_dir.to_string_lossy().into());

        toml::to_string_pretty(&value).context("failed to serialize default config")
    }
}

fn validate_profile_name(profile: &str) -> Result<()> {
    if profile
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Ok(());
    }

    anyhow::bail!(
        "profile name `{}` is invalid; use only ASCII letters, digits, '-' or '_'",
        profile
    )
}

fn profile_path_for(base_path: &Path, profile: &str) -> Result<PathBuf> {
    let file_stem = base_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot derive profile config path from `{}`",
                base_path.display()
            )
        })?;
    let file_name = match base_path.extension().and_then(|value| value.to_str()) {
        Some(extension) => format!("{file_stem}.{profile}.{extension}"),
        None => format!("{file_stem}.{profile}"),
    };

    Ok(base_path.with_file_name(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_path_uses_same_directory_and_profile_suffix() {
        assert_eq!(
            profile_path_for(Path::new("/tmp/config.toml"), "prod").unwrap(),
            PathBuf::from("/tmp/config.prod.toml")
        );
        assert_eq!(
            profile_path_for(Path::new("/tmp/app"), "prod").unwrap(),
            PathBuf::from("/tmp/app.prod")
        );
    }

    #[test]
    fn load_prefers_cli_profile_over_env_profile() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let cli_profile_path = temp_dir.path().join("config.prod.toml");
        let env_profile_path = temp_dir.path().join("config.dev.toml");
        fs::write(&config_path, "base_dir = \"/tmp\"").expect("base config");
        fs::write(&cli_profile_path, "base_dir = \"/tmp/prod\"").expect("cli profile config");
        fs::write(&env_profile_path, "base_dir = \"/tmp/dev\"").expect("env profile config");

        let loaded = ConfigLoader::new(ConfigInput {
            cli_path: Some(config_path.clone()),
            cli_profile: Some("prod".to_string()),
            env_profile: Some("dev".to_string()),
            ..Default::default()
        })
        .load()
        .expect("config should load");

        let profile = loaded.profile.expect("profile should load");
        assert_eq!(loaded.path, config_path);
        assert_eq!(profile.name, "prod");
        assert_eq!(profile.path, cli_profile_path);
    }

    #[test]
    fn load_rejects_missing_profile_config() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        fs::write(&config_path, "base_dir = \"/tmp\"").expect("base config");

        let err = ConfigLoader::new(ConfigInput {
            cli_path: Some(config_path),
            cli_profile: Some("prod".to_string()),
            ..Default::default()
        })
        .load()
        .expect_err("missing profile should fail");

        assert!(err.to_string().contains("profile config"));
    }

    #[test]
    fn load_rejects_path_like_profile_name() {
        let err = ConfigLoader::new(ConfigInput {
            cli_path: Some(PathBuf::from("config.toml")),
            cli_profile: Some("../prod".to_string()),
            ..Default::default()
        })
        .load()
        .expect_err("invalid profile should fail");

        assert!(err.to_string().contains("profile name"));
    }
}
