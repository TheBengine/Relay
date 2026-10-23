//! Trust is compiled from reviewed public assets, never discovered at runtime.
use super::{digest, Publisher, Result};
use serde::Deserialize;

const POLICY: &[u8] = include_bytes!("../publisher/policy.json");
const ROOT: &[u8] = include_bytes!("../publisher/root.json");

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
enum Policy {
    Disabled {
        schema: u32,
    },
    Signed {
        schema: u32,
        metadata: String,
        targets: String,
        channel: String,
        root_sha256: String,
    },
}

/// No filesystem, preferences, environment, TLS, network or worker is used here.
/// A malformed enabled policy is an error, distinct from an intentionally
/// unconfigured build. Establish the real root identity before enabling it.
pub fn bundled_publisher() -> Result<Option<Publisher>> {
    parse(POLICY, ROOT)
}

pub(super) fn parse(policy: &[u8], root: &[u8]) -> Result<Option<Publisher>> {
    if policy.len() > 16 * 1024 {
        return Err("The bundled publisher policy exceeds its size limit.".into());
    }
    let policy: Policy =
        serde_json::from_slice(policy).map_err(|_| "The bundled publisher policy is invalid.")?;
    match policy {
        Policy::Disabled { schema: 1 } => Ok(None),
        Policy::Signed {
            schema: 1,
            metadata,
            targets,
            channel,
            root_sha256,
        } => {
            if root.is_empty() || root.len() as u64 > super::MAX_METADATA {
                return Err(
                    "The bundled publisher root exceeds its size limit or is empty.".into(),
                );
            }
            if root_sha256 != digest(root) {
                return Err(
                    "The bundled publisher root does not match its reviewed digest.".into(),
                );
            }
            Publisher::from_bundled_root(root.to_vec(), &metadata, &targets, &channel).map(Some)
        }
        _ => Err("The bundled publisher policy uses an unsupported schema.".into()),
    }
}
