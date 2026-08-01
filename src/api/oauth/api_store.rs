use crate::api::oauth::{model::LoginUser, website_session_store::WebsiteSessionStore};

#[derive(Debug, Clone)]
pub enum BrowserSessionStore {
    Website(WebsiteSessionStore),
    /// Test-only session double. The production HTTP server always constructs
    /// [`BrowserSessionStore::Website`].
    Fixed(FixedUserSessionStore),
}

#[derive(Debug, Clone)]
pub struct FixedUserSessionStore {
    pub user: LoginUser,
}

impl BrowserSessionStore {
    pub async fn load_user(
        &self,
        cookie_header: Option<&str>,
    ) -> Result<Option<LoginUser>, crate::common::errors::MegaError> {
        match self {
            Self::Website(store) => {
                let Some((cookie_name, cookie_value)) =
                    cookie_header.and_then(|header| matching_cookie(header, store.cookie_names()))
                else {
                    return Ok(None);
                };
                store
                    .load_user_from_cookie_header_pair(cookie_name, cookie_value)
                    .await
            }
            Self::Fixed(store) => Ok(Some(store.user.clone())),
        }
    }
}

fn matching_cookie<'a>(
    cookie_header: &'a str,
    cookie_names: &[String],
) -> Option<(&'a str, &'a str)> {
    cookie_names.iter().find_map(|cookie_name| {
        cookie_header.split(';').find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == cookie_name).then_some((name, value))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{BrowserSessionStore, matching_cookie};
    use crate::api::oauth::website_session_store::WebsiteSessionStore;

    #[test]
    fn prefers_the_first_configured_cookie_name() {
        let names = vec![
            "better-auth.session_token".to_string(),
            "__Secure-better-auth.session_token".to_string(),
        ];
        assert_eq!(
            matching_cookie(
                "__Secure-better-auth.session_token=second; better-auth.session_token=first",
                &names
            ),
            Some(("better-auth.session_token", "first"))
        );
    }

    #[tokio::test]
    async fn website_session_without_cookie_is_rejected() {
        let store = BrowserSessionStore::Website(
            WebsiteSessionStore::new(
                "http://127.0.0.1:7001".to_string(),
                vec!["better-auth.session_token".to_string()],
            )
            .unwrap(),
        );

        assert!(store.load_user(None).await.unwrap().is_none());
    }
}
