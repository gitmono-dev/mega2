use serde::{Deserialize, Serialize};

/// Website Better Auth `GET /api/auth/get-session` response body.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct WebsiteGetSessionResponse {
    pub session: Option<WebsiteSessionJson>,
    pub user: Option<WebsiteAuthUserJson>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebsiteSessionJson {
    pub id: String,
    pub user_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebsiteAuthUserJson {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub image: Option<String>,
    pub banned: Option<bool>,
}

impl WebsiteAuthUserJson {
    pub fn into_login_user(self) -> Option<LoginUser> {
        if self.banned == Some(true) {
            tracing::warn!(website_user_id = %self.id, "rejecting banned website user");
            return None;
        }

        let email = self.email.unwrap_or_default();
        let username = if self.name.trim().is_empty() {
            email
                .split_once('@')
                .map(|(local_part, _)| local_part.trim())
                .filter(|local_part| !local_part.is_empty())
                .unwrap_or_default()
                .to_string()
        } else {
            self.name
        };

        if username.trim().is_empty() {
            tracing::warn!(
                website_user_id = %self.id,
                "rejecting website user without a usable name or email local-part"
            );
            return None;
        }

        Some(LoginUser {
            website_user_id: self.id,
            username,
            email,
            avatar_url: self.image.unwrap_or_default(),
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LoginUser {
    pub website_user_id: String,
    pub username: String,
    pub avatar_url: String,
    pub email: String,
}

#[cfg(test)]
mod tests {
    use super::WebsiteAuthUserJson;

    #[test]
    fn rejects_banned_users() {
        assert!(
            WebsiteAuthUserJson {
                id: "user-1".to_string(),
                name: "Ada".to_string(),
                email: Some("ada@example.com".to_string()),
                image: None,
                banned: Some(true),
            }
            .into_login_user()
            .is_none()
        );
    }

    #[test]
    fn uses_email_local_part_when_name_is_empty() {
        let user = WebsiteAuthUserJson {
            id: "user-1".to_string(),
            name: "  ".to_string(),
            email: Some("ada@example.com".to_string()),
            image: None,
            banned: None,
        }
        .into_login_user()
        .expect("email local-part should provide a username");

        assert_eq!(user.username, "ada");
    }

    #[test]
    fn rejects_empty_name_without_usable_email() {
        for email in [
            None,
            Some("@example.com".to_string()),
            Some("   @example.com".to_string()),
        ] {
            assert!(
                WebsiteAuthUserJson {
                    id: "user-1".to_string(),
                    name: String::new(),
                    email,
                    image: None,
                    banned: None,
                }
                .into_login_user()
                .is_none()
            );
        }
    }
}
