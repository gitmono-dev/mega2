use crate::common::errors::MegaError;

pub const DEFAULT_MAIL_LOCALE: &str = "en-US";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailTemplate<'a> {
    subject: &'a str,
    html: &'a str,
    text: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedMail {
    pub subject: String,
    pub html: String,
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailTemplateKey(&'static str);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalizedMailTemplate<'a> {
    key: MailTemplateKey,
    locale: &'a str,
    template: MailTemplate<'a>,
}

#[derive(Clone, Copy, Debug)]
pub struct MailTemplateRegistry<'a> {
    default_locale: &'a str,
    templates: &'a [LocalizedMailTemplate<'a>],
}

impl<'a> MailTemplate<'a> {
    pub const fn new(subject: &'a str, html: &'a str, text: Option<&'a str>) -> Self {
        Self {
            subject,
            html,
            text,
        }
    }

    pub fn render(&self, vars: &[(&str, &str)]) -> Result<RenderedMail, MegaError> {
        Ok(RenderedMail {
            subject: render_template(self.subject, vars, ValueEscaping::Raw)?,
            html: render_template(self.html, vars, ValueEscaping::Html)?,
            text: self
                .text
                .map(|text| render_template(text, vars, ValueEscaping::Raw))
                .transpose()?,
        })
    }
}

impl MailTemplateKey {
    pub const fn new(key: &'static str) -> Self {
        Self(key)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl<'a> LocalizedMailTemplate<'a> {
    pub const fn new(key: MailTemplateKey, locale: &'a str, template: MailTemplate<'a>) -> Self {
        Self {
            key,
            locale,
            template,
        }
    }
}

impl<'a> MailTemplateRegistry<'a> {
    pub const fn new(default_locale: &'a str, templates: &'a [LocalizedMailTemplate<'a>]) -> Self {
        Self {
            default_locale,
            templates,
        }
    }

    pub fn render(
        &self,
        key: MailTemplateKey,
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
        key: MailTemplateKey,
        locale: Option<&str>,
    ) -> Option<MailTemplate<'a>> {
        let requested_locale = locale
            .map(str::trim)
            .filter(|locale| !locale.is_empty())
            .unwrap_or(self.default_locale);

        self.find_exact(key, requested_locale)
            .or_else(|| {
                requested_locale
                    .split_once('-')
                    .and_then(|(language, _)| self.find_exact(key, language))
            })
            .or_else(|| self.find_exact(key, self.default_locale))
    }

    fn find_exact(&self, key: MailTemplateKey, locale: &str) -> Option<MailTemplate<'a>> {
        self.templates
            .iter()
            .find(|template| template.key == key && template.locale == locale)
            .map(|template| template.template)
    }

    fn missing_template_error(&self, key: MailTemplateKey, locale: Option<&str>) -> MegaError {
        let requested_locale = locale
            .map(str::trim)
            .filter(|locale| !locale.is_empty())
            .unwrap_or(self.default_locale);

        MegaError::Other(format!(
            "mail template `{}` is missing for locale `{requested_locale}` with default locale `{}`",
            key.as_str(),
            self.default_locale
        ))
    }
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
        const KEY: MailTemplateKey = MailTemplateKey::new("test.event");
        const REGISTRY: MailTemplateRegistry<'static> = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            &[
                LocalizedMailTemplate::new(
                    KEY,
                    DEFAULT_MAIL_LOCALE,
                    MailTemplate::new("Hello {{name}}", "<p>Hello {{name}}</p>", None),
                ),
                LocalizedMailTemplate::new(
                    KEY,
                    "zh-CN",
                    MailTemplate::new("Ni hao {{name}}", "<p>Ni hao {{name}}</p>", None),
                ),
            ],
        );

        let rendered = REGISTRY
            .render(KEY, Some("zh-CN"), &[("name", "alice")])
            .unwrap();

        assert_eq!(rendered.subject, "Ni hao alice");
    }

    #[test]
    fn mail_template_registry_falls_back_to_language_then_default_locale() {
        const KEY: MailTemplateKey = MailTemplateKey::new("test.event");
        const REGISTRY: MailTemplateRegistry<'static> = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            &[
                LocalizedMailTemplate::new(
                    KEY,
                    DEFAULT_MAIL_LOCALE,
                    MailTemplate::new("Hello {{name}}", "<p>Hello {{name}}</p>", None),
                ),
                LocalizedMailTemplate::new(
                    KEY,
                    "fr",
                    MailTemplate::new("Bonjour {{name}}", "<p>Bonjour {{name}}</p>", None),
                ),
            ],
        );

        let language = REGISTRY
            .render(KEY, Some("fr-CA"), &[("name", "alice")])
            .unwrap();
        let default = REGISTRY
            .render(KEY, Some("de-DE"), &[("name", "alice")])
            .unwrap();

        assert_eq!(language.subject, "Bonjour alice");
        assert_eq!(default.subject, "Hello alice");
    }

    #[test]
    fn mail_template_registry_missing_template_error_does_not_include_values() {
        const KEY: MailTemplateKey = MailTemplateKey::new("test.event");
        const MISSING_KEY: MailTemplateKey = MailTemplateKey::new("missing.event");
        const REGISTRY: MailTemplateRegistry<'static> = MailTemplateRegistry::new(
            DEFAULT_MAIL_LOCALE,
            &[LocalizedMailTemplate::new(
                KEY,
                DEFAULT_MAIL_LOCALE,
                MailTemplate::new("Hello {{name}}", "<p>Hello {{name}}</p>", None),
            )],
        );

        let error = REGISTRY
            .render(MISSING_KEY, Some("en-US"), &[("name", "sensitive-value")])
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("missing.event"));
        assert!(!message.contains("sensitive-value"));
    }
}
