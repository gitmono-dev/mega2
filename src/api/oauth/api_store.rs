use crate::api::oauth::{model::LoginUser, website_session_store::WebsiteSessionStore};

#[derive(Debug, Clone)]
pub enum BrowserSessionStore {
    Website(WebsiteSessionStore),
    /// Test-only session double, `cfg(test)`-gated so it is never compiled
    /// into the production `service http` binary (website-auth.md contract).
    #[cfg(test)]
    Fixed(FixedUserSessionStore),
    /// Test-only double that counts lookups and can return a different user on
    /// each call, so a test can prove a request resolves its subject exactly
    /// once (UN-22) rather than re-reading a drifting store.
    #[cfg(test)]
    Counting(CountingSessionStore),
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FixedUserSessionStore {
    pub user: LoginUser,
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct CountingSessionStore {
    pub calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Answers returned in order; the last one repeats once exhausted. `None`
    /// stands for "no session", `Err` for a store failure.
    pub answers: std::sync::Arc<Vec<Result<Option<LoginUser>, crate::common::errors::MegaError>>>,
}

#[cfg(test)]
impl CountingSessionStore {
    pub fn new(answers: Vec<Result<Option<LoginUser>, crate::common::errors::MegaError>>) -> Self {
        Self {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            answers: std::sync::Arc::new(answers),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
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
            #[cfg(test)]
            Self::Fixed(store) => Ok(Some(store.user.clone())),
            #[cfg(test)]
            Self::Counting(store) => {
                let nth = store
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let index = nth.min(store.answers.len().saturating_sub(1));
                match store.answers.get(index) {
                    Some(Ok(user)) => Ok(user.clone()),
                    Some(Err(error)) => {
                        Err(crate::common::errors::MegaError::Other(error.to_string()))
                    }
                    None => Ok(None),
                }
            }
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
