//! Explicit public export policy. Credentials never enter these wire types.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordBridgeSettings {
    pub enabled: bool,
    pub status_enabled: bool,
    pub status_channel_id: Option<String>,
    pub application_id: String,
    pub guild_id: String,
    pub channel_id: Option<String>,
    pub public_label: String,
    pub join_instructions: String,
}

pub fn snowflake(value: &str) -> Option<u64> {
    (!value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse::<u64>().ok().filter(|v| *v > 0))
        .flatten()
}

impl DiscordBridgeSettings {
    pub fn valid(&self) -> bool {
        (!self.enabled
            || !self.status_enabled
            || self
                .status_channel_id
                .as_deref()
                .and_then(snowflake)
                .is_some())
            && self
                .status_channel_id
                .as_ref()
                .is_none_or(|v| v.len() <= 20)
            && self.application_id.len() <= 20
            && self.guild_id.len() <= 20
            && self.channel_id.as_ref().is_none_or(|c| c.len() <= 128)
            && self.public_label.chars().count() <= 80
            && self.join_instructions.chars().count() <= 700
            && !self.public_label.chars().any(char::is_control)
            && !self
                .join_instructions
                .chars()
                .any(|c| c.is_control() && c != '\n')
            && (!self.enabled
                || (snowflake(&self.application_id).is_some()
                    && snowflake(&self.guild_id).is_some()
                    && self.channel_id.as_ref().is_some_and(|c| !c.is_empty())
                    && !self.public_label.trim().is_empty()
                    && !self.join_instructions.trim().is_empty()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordBridgeView {
    pub settings: DiscordBridgeSettings,
    pub connection: String,
    #[serde(default)]
    pub channel_status: String,
    pub public_preview: Option<String>,
}
