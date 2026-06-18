use crate::common::errors::MegaError;

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
}
