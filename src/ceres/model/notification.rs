use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NotificationEventTypeInfo {
    pub code: String,
    pub category: String,
    pub description: String,
    pub system_required: bool,
    pub default_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserNotificationPreferenceItem {
    pub event_type_code: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserNotificationConfig {
    pub enabled: bool,
    pub preferences: Vec<UserNotificationPreferenceItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateUserNotificationConfig {
    pub enabled: Option<bool>,
    pub preferences: Option<Vec<UserNotificationPreferenceItem>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_config_round_trips_enabled_and_preferences_only() {
        let config: UserNotificationConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "preferences": [{
                "event_type_code": "cl.comment.created",
                "enabled": false
            }]
        }))
        .expect("UserNotificationConfig");
        assert!(config.enabled);
        assert_eq!(config.preferences.len(), 1);
        assert_eq!(config.preferences[0].event_type_code, "cl.comment.created");
        assert!(!config.preferences[0].enabled);

        let update: UpdateUserNotificationConfig = serde_json::from_value(serde_json::json!({
            "enabled": false
        }))
        .expect("UpdateUserNotificationConfig");
        assert_eq!(update.enabled, Some(false));
        assert!(update.preferences.is_none());
    }
}
