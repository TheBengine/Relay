//! File-size policy is expressed in bytes on the wire, including encryption.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Legacy peers advertise no policy and accept up to ten MiB.
pub const MAX_SUPPORTED_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_PENDING_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct UploadLimits {
    pub max_file_bytes: u64,
    pub effective_file_bytes: u64,
    pub pending_bytes: u64,
}
impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: crate::MAX_UPLOAD_BYTES as u64,
            effective_file_bytes: crate::MAX_UPLOAD_BYTES as u64,
            pending_bytes: MAX_PENDING_UPLOAD_BYTES,
        }
    }
}
impl UploadLimits {
    pub fn effective(&self) -> u64 {
        self.effective_file_bytes
            .min(self.max_file_bytes)
            .min(MAX_SUPPORTED_UPLOAD_BYTES as u64)
    }
}

/// Server administrators own this policy. Missing role entries inherit the
/// default; explicit entries are additive grants (the largest wins).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct UploadPolicy {
    /// Submitted as the expected revision; successful writes increment it.
    pub revision: u64,
    pub default_file_bytes: u64,
    pub max_file_bytes: u64,
    pub role_limits: BTreeMap<String, u64>,
}
impl Default for UploadPolicy {
    fn default() -> Self {
        Self {
            revision: 0,
            default_file_bytes: crate::MAX_UPLOAD_BYTES as u64,
            max_file_bytes: crate::MAX_UPLOAD_BYTES as u64,
            role_limits: BTreeMap::new(),
        }
    }
}
impl UploadPolicy {
    pub fn valid(&self) -> bool {
        self.default_file_bytes <= self.max_file_bytes
            && self.max_file_bytes <= MAX_SUPPORTED_UPLOAD_BYTES as u64
            && self.role_limits.len() <= 128
            && self.role_limits.iter().all(|(id, size)| {
                !id.is_empty()
                    && id.len() <= 100
                    && !id.chars().any(char::is_control)
                    && *size <= MAX_SUPPORTED_UPLOAD_BYTES as u64
            })
    }
    pub fn for_roles<'a>(
        &self,
        roles: impl IntoIterator<Item = &'a str>,
        owner: bool,
    ) -> UploadLimits {
        let explicit = roles
            .into_iter()
            .filter_map(|id| self.role_limits.get(id))
            .copied()
            .max();
        let effective = if owner {
            self.max_file_bytes
        } else {
            explicit.unwrap_or(self.default_file_bytes)
        };
        UploadLimits {
            max_file_bytes: self.max_file_bytes.min(MAX_SUPPORTED_UPLOAD_BYTES as u64),
            effective_file_bytes: effective
                .min(self.max_file_bytes)
                .min(MAX_SUPPORTED_UPLOAD_BYTES as u64),
            pending_bytes: MAX_PENDING_UPLOAD_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_defaults_and_new_role_grants_keep_wire_limits_bounded() {
        assert_eq!(
            serde_json::from_str::<UploadLimits>("{}").unwrap(),
            UploadLimits::default()
        );
        let mut policy = UploadPolicy {
            max_file_bytes: MAX_SUPPORTED_UPLOAD_BYTES as u64,
            ..Default::default()
        };
        policy.role_limits.insert("restricted".into(), 0);
        policy.role_limits.insert("large".into(), 20 * 1024 * 1024);
        assert!(policy.valid());
        assert_eq!(
            policy.for_roles(["unknown"], false).effective(),
            crate::MAX_UPLOAD_BYTES as u64
        );
        assert_eq!(policy.for_roles(["restricted"], false).effective(), 0);
        assert_eq!(
            policy.for_roles(["restricted", "large"], false).effective(),
            20 * 1024 * 1024
        );
        assert_eq!(
            policy.for_roles(["restricted"], true).effective(),
            MAX_SUPPORTED_UPLOAD_BYTES as u64
        );
        policy.max_file_bytes = 5 * 1024 * 1024;
        policy.default_file_bytes = policy.max_file_bytes;
        assert_eq!(
            policy.for_roles(["large"], false).effective(),
            policy.max_file_bytes
        );
        policy.max_file_bytes = MAX_SUPPORTED_UPLOAD_BYTES as u64 + 1;
        assert!(!policy.valid());
    }
}
