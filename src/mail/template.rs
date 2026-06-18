use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::common::errors::MegaError;

pub const DEFAULT_MAIL_LOCALE: &str = "en-US";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailTemplate {
    subject: String,
    html: String,
    text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedMail {
    pub subject: String,
    pub html: String,
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MailTemplateKey(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalizedMailTemplate {
    key: MailTemplateKey,
    locale: String,
    template: MailTemplate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalizedMailTemplateSource {
    template: LocalizedMailTemplate,
    source_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct MailTemplateRegistry {
    default_locale: String,
    templates: Vec<LocalizedMailTemplate>,
}

impl MailTemplate {
    pub fn new(subject: impl Into<String>, html: impl Into<String>, text: Option<&str>) -> Self {
        Self {
            subject: subject.into(),
            html: html.into(),
            text: text.map(str::to_string),
        }
    }

    pub fn render(&self, vars: &[(&str, &str)]) -> Result<RenderedMail, MegaError> {
        Ok(RenderedMail {
            subject: render_template(&self.subject, vars, ValueEscaping::Raw)?,
            html: render_template(&self.html, vars, ValueEscaping::Html)?,
            text: self
                .text
                .as_deref()
                .map(|text| render_template(text, vars, ValueEscaping::Raw))
                .transpose()?,
        })
    }

    fn validate_syntax(&self) -> Result<(), MegaError> {
        validate_template_syntax(&self.subject)?;
        validate_template_syntax(&self.html)?;
        if let Some(text) = &self.text {
            validate_template_syntax(text)?;
        }

        Ok(())
    }

    pub fn subject_template(&self) -> &str {
        &self.subject
    }

    pub fn html_template(&self) -> &str {
        &self.html
    }

    pub fn text_template(&self) -> Option<&str> {
        self.text.as_deref()
    }
}

impl MailTemplateKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl LocalizedMailTemplate {
    pub fn new(key: MailTemplateKey, locale: impl Into<String>, template: MailTemplate) -> Self {
        Self {
            key,
            locale: locale.into(),
            template,
        }
    }

    pub fn key(&self) -> &MailTemplateKey {
        &self.key
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }

    pub fn template(&self) -> &MailTemplate {
        &self.template
    }
}

impl LocalizedMailTemplateSource {
    pub fn new(template: LocalizedMailTemplate, source_path: PathBuf) -> Self {
        Self {
            template,
            source_path,
        }
    }

    pub fn template(&self) -> &LocalizedMailTemplate {
        &self.template
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn into_template(self) -> LocalizedMailTemplate {
        self.template
    }
}

impl MailTemplateRegistry {
    pub fn new(default_locale: impl Into<String>, templates: Vec<LocalizedMailTemplate>) -> Self {
        Self {
            default_locale: default_locale.into(),
            templates,
        }
    }

    pub fn append_templates<I>(&mut self, templates: I)
    where
        I: IntoIterator<Item = LocalizedMailTemplate>,
    {
        self.templates.extend(templates);
    }

    pub fn default_locale(&self) -> &str {
        &self.default_locale
    }

    pub fn templates(&self) -> &[LocalizedMailTemplate] {
        &self.templates
    }

    pub fn render(
        &self,
        key: &MailTemplateKey,
        locale: Option<&str>,
        vars: &[(&str, &str)],
    ) -> Result<RenderedMail, MegaError> {
        let template = self
            .template_for(key, locale)
            .ok_or_else(|| self.missing_template_error(key, locale))?;

        template.render(vars)
    }

    pub fn template_for(
        &self,
        key: &MailTemplateKey,
        locale: Option<&str>,
    ) -> Option<&MailTemplate> {
        let requested_locale = locale
            .map(str::trim)
            .filter(|locale| !locale.is_empty())
            .unwrap_or(&self.default_locale);

        self.find_exact(key, requested_locale)
            .or_else(|| {
                requested_locale
                    .split_once('-')
                    .and_then(|(language, _)| self.find_exact(key, language))
            })
            .or_else(|| self.find_exact(key, &self.default_locale))
    }

    fn find_exact(&self, key: &MailTemplateKey, locale: &str) -> Option<&MailTemplate> {
        self.templates
            .iter()
            .rev()
            .find(|template| &template.key == key && template.locale == locale)
            .map(|template| &template.template)
    }

    fn missing_template_error(&self, key: &MailTemplateKey, locale: Option<&str>) -> MegaError {
        let requested_locale = locale
            .map(str::trim)
            .filter(|locale| !locale.is_empty())
            .unwrap_or(&self.default_locale);

        MegaError::Other(format!(
            "mail template `{}` is missing for locale `{requested_locale}` with default locale `{}`",
            key.as_str(),
            self.default_locale
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailTemplateFile {
    key: String,
    locale: String,
    subject: String,
    html: String,
    #[serde(default)]
    text: Option<String>,
}

impl MailTemplateFile {
    fn into_localized_template(
        self,
        source_path: &Path,
    ) -> Result<LocalizedMailTemplate, MegaError> {
        validate_template_file_key("key", &self.key, source_path)?;
        validate_template_file_key("locale", &self.locale, source_path)?;
        if self.subject.trim().is_empty() {
            return Err(invalid_template_file(
                source_path,
                "mail template subject must not be empty",
            ));
        }
        if self.html.trim().is_empty() {
            return Err(invalid_template_file(
                source_path,
                "mail template html must not be empty",
            ));
        }

        let template = MailTemplate::new(self.subject, self.html, self.text.as_deref());
        template.validate_syntax().map_err(|err| {
            invalid_template_file(
                source_path,
                &format!("mail template syntax is invalid: {err}"),
            )
        })?;

        Ok(LocalizedMailTemplate::new(
            MailTemplateKey::new(self.key),
            self.locale,
            template,
        ))
    }
}

pub fn load_localized_templates_from_dir(
    template_dir: &Path,
) -> Result<Vec<LocalizedMailTemplate>, MegaError> {
    Ok(load_localized_template_sources_from_dir(template_dir)?
        .into_iter()
        .map(LocalizedMailTemplateSource::into_template)
        .collect())
}

pub fn load_localized_template_sources_from_dir(
    template_dir: &Path,
) -> Result<Vec<LocalizedMailTemplateSource>, MegaError> {
    let mut paths = mail_template_file_paths(template_dir)?;
    paths.sort();

    let mut templates = Vec::with_capacity(paths.len());
    let mut seen = BTreeSet::new();
    for path in paths {
        let raw = fs::read_to_string(&path).map_err(|err| {
            MegaError::Other(format!(
                "failed to read mail template file `{}`: {err}",
                path.display()
            ))
        })?;
        let template = toml::from_str::<MailTemplateFile>(&raw).map_err(|err| {
            MegaError::Other(format!(
                "failed to parse mail template file `{}`: {err}",
                path.display()
            ))
        })?;
        let template = template.into_localized_template(&path)?;
        let identity = (
            template.key.as_str().to_string(),
            template.locale.trim().to_string(),
        );
        if !seen.insert(identity) {
            return Err(invalid_template_file(
                &path,
                "mail template directory contains duplicate key/locale entries",
            ));
        }
        templates.push(LocalizedMailTemplateSource::new(template, path));
    }

    Ok(templates)
}

fn mail_template_file_paths(template_dir: &Path) -> Result<Vec<PathBuf>, MegaError> {
    let entries = fs::read_dir(template_dir).map_err(|err| {
        MegaError::Other(format!(
            "failed to read mail template directory `{}`: {err}",
            template_dir.display()
        ))
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            MegaError::Other(format!(
                "failed to read mail template directory entry in `{}`: {err}",
                template_dir.display()
            ))
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            paths.push(path);
        }
    }

    Ok(paths)
}

fn validate_template_file_key(
    field: &str,
    value: &str,
    source_path: &Path,
) -> Result<(), MegaError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_template_file(
            source_path,
            &format!("mail template {field} must not be empty"),
        ));
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        Ok(())
    } else {
        Err(invalid_template_file(
            source_path,
            &format!("mail template {field} contains unsupported characters"),
        ))
    }
}

fn invalid_template_file(source_path: &Path, message: &str) -> MegaError {
    MegaError::Other(format!(
        "mail template file `{}` is invalid: {message}",
        source_path.display()
    ))
}

fn validate_template_syntax(template: &str) -> Result<(), MegaError> {
    let mut remaining = template;
    while let Some(start) = remaining.find("{{") {
        let rest = &remaining[start + 2..];
        let Some(end) = rest.find("}}") else {
            return Err(MegaError::Other(
                "mail template contains an unclosed variable".to_string(),
            ));
        };

        let key = rest[..end].trim();
        validate_template_key(key)?;
        remaining = &rest[end + 2..];
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum ValueEscaping {
    Raw,
    Html,
}

fn render_template(
    template: &str,
    vars: &[(&str, &str)],
    escaping: ValueEscaping,
) -> Result<String, MegaError> {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;

    while let Some(start) = remaining.find("{{") {
        let (prefix, rest) = remaining.split_at(start);
        rendered.push_str(prefix);

        let rest = &rest[2..];
        let Some(end) = rest.find("}}") else {
            return Err(MegaError::Other(
                "mail template contains an unclosed variable".to_string(),
            ));
        };

        let key = rest[..end].trim();
        validate_template_key(key)?;

        let value = vars
            .iter()
            .find_map(|(candidate, value)| (*candidate == key).then_some(*value))
            .ok_or_else(|| {
                MegaError::Other(format!("mail template variable `{key}` is missing"))
            })?;

        match escaping {
            ValueEscaping::Raw => rendered.push_str(value),
            ValueEscaping::Html => rendered.push_str(&escape_html(value)),
        }

        remaining = &rest[end + 2..];
    }

    rendered.push_str(remaining);
    Ok(rendered)
}

fn validate_template_key(key: &str) -> Result<(), MegaError> {
    if key.is_empty() {
        return Err(MegaError::Other(
            "mail template contains an empty variable".to_string(),
        ));
    }

    if key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        Ok(())
    } else {
        Err(MegaError::Other(format!(
            "mail template variable `{key}` contains unsupported characters"
        )))
    }
}

pub fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_template_renders_subject_html_and_text() {
        let template = MailTemplate::new(
            "New comment on {{ cl_link }}",
            "<p>{{actor}}</p><p>{{comment}}</p>",
            Some("{{actor}} said: {{comment}}"),
        );

        let rendered = template
            .render(&[
                ("actor", "alice"),
                ("cl_link", "CL1"),
                ("comment", "<hello & goodbye>"),
            ])
            .unwrap();

        assert_eq!(rendered.subject, "New comment on CL1");
        assert_eq!(
            rendered.html,
            "<p>alice</p><p>&lt;hello &amp; goodbye&gt;</p>"
        );
        assert_eq!(
            rendered.text.as_deref(),
            Some("alice said: <hello & goodbye>")
        );
    }

    #[test]
    fn mail_template_reports_missing_variables_without_values() {
        let template = MailTemplate::new("{{missing}}", "{{present}}", None);
        let error = template
            .render(&[("present", "sensitive-value")])
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("missing"));
        assert!(!message.contains("sensitive-value"));
    }

    #[test]
    fn mail_template_rejects_unclosed_variables() {
        let template = MailTemplate::new("hello {{name", "{{name}}", None);
        let error = template.render(&[("name", "alice")]).unwrap_err();
        assert!(error.to_string().contains("unclosed variable"));
    }

    #[test]
    fn mail_template_registry_renders_requested_locale() {
        let key = MailTemplateKey::new("test.event");
        let registry = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            vec![
                LocalizedMailTemplate::new(
                    key.clone(),
                    DEFAULT_MAIL_LOCALE,
                    MailTemplate::new("Hello {{name}}", "<p>Hello {{name}}</p>", None),
                ),
                LocalizedMailTemplate::new(
                    key.clone(),
                    "zh-CN",
                    MailTemplate::new("Ni hao {{name}}", "<p>Ni hao {{name}}</p>", None),
                ),
            ],
        );

        let rendered = registry
            .render(&key, Some("zh-CN"), &[("name", "alice")])
            .unwrap();

        assert_eq!(rendered.subject, "Ni hao alice");
    }

    #[test]
    fn mail_template_registry_falls_back_to_language_then_default_locale() {
        let key = MailTemplateKey::new("test.event");
        let registry = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            vec![
                LocalizedMailTemplate::new(
                    key.clone(),
                    DEFAULT_MAIL_LOCALE,
                    MailTemplate::new("Hello {{name}}", "<p>Hello {{name}}</p>", None),
                ),
                LocalizedMailTemplate::new(
                    key.clone(),
                    "fr",
                    MailTemplate::new("Bonjour {{name}}", "<p>Bonjour {{name}}</p>", None),
                ),
            ],
        );

        let language = registry
            .render(&key, Some("fr-CA"), &[("name", "alice")])
            .unwrap();
        let default = registry
            .render(&key, Some("de-DE"), &[("name", "alice")])
            .unwrap();

        assert_eq!(language.subject, "Bonjour alice");
        assert_eq!(default.subject, "Hello alice");
    }

    #[test]
    fn mail_template_registry_missing_template_error_does_not_include_values() {
        let key = MailTemplateKey::new("test.event");
        let missing_key = MailTemplateKey::new("missing.event");
        let registry = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            vec![LocalizedMailTemplate::new(
                key,
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new("Hello {{name}}", "<p>Hello {{name}}</p>", None),
            )],
        );

        let error = registry
            .render(&missing_key, Some("en-US"), &[("name", "sensitive-value")])
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("missing.event"));
        assert!(!message.contains("sensitive-value"));
    }

    #[test]
    fn load_localized_templates_from_dir_reads_toml_overrides() {
        let dir = tempfile::tempdir().expect("temp dir");
        let template_path = dir.path().join("cl-comment.toml");
        fs::write(
            &template_path,
            r#"
key = "test.event"
locale = "en-US"
subject = "Override {{name}}"
html = "<p>Override {{name}}</p>"
text = "Override {{name}}"
"#,
        )
        .expect("write template");

        let key = MailTemplateKey::new("test.event");
        let mut registry = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            vec![LocalizedMailTemplate::new(
                key.clone(),
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new("Built in {{name}}", "<p>Built in {{name}}</p>", None),
            )],
        );
        registry.append_templates(load_localized_templates_from_dir(dir.path()).unwrap());

        let rendered = registry
            .render(&key, Some("en-US"), &[("name", "alice")])
            .unwrap();

        assert_eq!(rendered.subject, "Override alice");
        assert_eq!(rendered.text.as_deref(), Some("Override alice"));
    }

    #[test]
    fn load_localized_template_sources_from_dir_reports_source_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let template_path = dir.path().join("cl-comment.toml");
        fs::write(
            &template_path,
            r#"
key = "test.event"
locale = "en-US"
subject = "Subject {{name}}"
html = "<p>{{name}}</p>"
"#,
        )
        .expect("write template");

        let sources = load_localized_template_sources_from_dir(dir.path()).unwrap();

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_path(), template_path.as_path());
        assert_eq!(sources[0].template().key().as_str(), "test.event");
    }

    #[test]
    fn load_localized_templates_from_dir_rejects_duplicate_key_locale() {
        let dir = tempfile::tempdir().expect("temp dir");
        for filename in ["a.toml", "b.toml"] {
            fs::write(
                dir.path().join(filename),
                r#"
key = "test.event"
locale = "en-US"
subject = "Subject {{name}}"
html = "<p>{{name}}</p>"
"#,
            )
            .expect("write template");
        }

        let error = load_localized_templates_from_dir(dir.path()).unwrap_err();
        assert!(error.to_string().contains("duplicate key/locale"));
    }
}
