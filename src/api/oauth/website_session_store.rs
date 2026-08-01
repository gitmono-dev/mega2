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
