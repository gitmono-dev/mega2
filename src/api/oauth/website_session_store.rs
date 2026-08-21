use std::{sync::Arc, time::Duration};

use http::header::COOKIE;
use reqwest::Client;

use crate::{
    api::oauth::model::{LoginUser, WebsiteAuthUserJson, WebsiteGetSessionResponse},
    common::errors::MegaError,
};

#[derive(Debug, Clone)]
pub struct WebsiteSessionStore {
    client: Arc<Client>,
    api_base_url: String,
    cookie_names: Vec<String>,
}

impl WebsiteSessionStore {
    pub fn new(api_base_url: String, cookie_names: Vec<String>) -> Result<Self, MegaError> {
        let client = Client::builder()
            .no_proxy()
            // Refuse redirects, matching every other outbound client in this
            // repo (notification::website_mail, channels::slack,
            // channels::webhook). Following one would re-send the session
            // Cookie to wherever the front end pointed, so a misrouted deploy
            // could answer with someone else's session. Refusing keeps the
            // 3xx itself as the observed status: `load_user_from_cookie_header_pair`
            // then warns with that status and returns `Ok(None)` — the request
            // is treated as anonymous (fail-closed, HTTP 401 `Login first`),
            // NOT as an authenticated user of the redirect target. Note this is
            // a warn plus a 401, not a hard error; CI's
            // `curl -sf .../api/auth/get-session` probe does not pass -L, so a
            // 3xx passes CI while every browser session here fails closed.
            // `oauth.website_api_base_url` is allowed to be plain `http`
            // (config/validate.rs), so an origin that 301s http->https is a
            // realistic way to hit this.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|error| {
                MegaError::Other(format!(
                    "failed to build website session HTTP client: {error}"
                ))
            })?;

        Ok(Self {
            client: Arc::new(client),
            api_base_url,
            cookie_names,
        })
    }

    pub fn cookie_names(&self) -> &[String] {
        &self.cookie_names
    }

    pub async fn load_user_from_cookie_header_pair(
        &self,
        cookie_name: &str,
        cookie_value: &str,
    ) -> Result<Option<LoginUser>, MegaError> {
        let url = format!(
            "{}/api/auth/get-session",
            self.api_base_url.trim_end_matches('/')
        );
        let response = self
            .client
            .get(url)
            .header(COOKIE, format!("{cookie_name}={cookie_value}"))
            .send()
            .await
            .map_err(|error| {
                MegaError::Other(format!("website get-session request failed: {error}"))
            })?;

        if !response.status().is_success() {
            tracing::warn!(status = %response.status(), "website get-session returned non-success status");
            return Ok(None);
        }

        let body = response.text().await.map_err(|error| {
            MegaError::Other(format!(
                "failed to read website get-session response: {error}"
            ))
        })?;
        let body = body.trim();
        if body.is_empty() {
            return Ok(None);
        }

        let response: Option<WebsiteGetSessionResponse> = serde_json::from_str(body)?;
        Ok(response
            .and_then(|response| response.user)
            .and_then(WebsiteAuthUserJson::into_login_user))
    }
}

#[cfg(test)]
mod tests {
    use super::WebsiteSessionStore;

    #[test]
    fn builds_without_panicking() {
        assert!(
            WebsiteSessionStore::new(
                "http://127.0.0.1:7001".to_string(),
                vec!["better-auth.session_token".to_string()]
            )
            .is_ok()
        );
    }
}
