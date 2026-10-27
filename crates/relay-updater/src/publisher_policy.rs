//! Trust is compiled from reviewed public assets, never discovered at runtime.
use super::{digest, Publisher, Result};
use serde::Deserialize;

// A stable build opts in explicitly: `--features relay-updater/stable-channel`.
// Both channels trust the same reviewed root; only the HTTPS bases and the
// channel name differ, and a stable publisher refuses prerelease versions.
#[cfg(not(feature = "stable-channel"))]
const POLICY: &[u8] = include_bytes!("../publisher/policy.json");
#[cfg(feature = "stable-channel")]
const POLICY: &[u8] = include_bytes!("../publisher/policy-stable.json");
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

#[cfg(test)]
mod tests {
    use super::*;

    const BETA: &[u8] = include_bytes!("../publisher/policy.json");
    const STABLE: &[u8] = include_bytes!("../publisher/policy-stable.json");

    #[test]
    fn beta_and_stable_policies_share_the_reviewed_root_and_differ_only_in_channel() {
        let beta = parse(BETA, ROOT).unwrap().unwrap();
        let stable = parse(STABLE, ROOT).unwrap().unwrap();
        assert_eq!(beta.channel, "beta");
        assert_eq!(stable.channel, "stable");
        assert_ne!(beta.policy_identity(), stable.policy_identity());
        let as_json = |raw: &[u8]| serde_json::from_slice::<serde_json::Value>(raw).unwrap();
        let (beta, stable) = (as_json(BETA), as_json(STABLE));
        assert_eq!(beta["root_sha256"], stable["root_sha256"]);
        for key in ["metadata", "targets"] {
            assert_eq!(
                beta[key].as_str().unwrap().replace("/beta/", "/stable/"),
                stable[key].as_str().unwrap()
            );
        }
    }

    #[test]
    fn compiled_policy_matches_the_selected_channel_feature() {
        let expected = if cfg!(feature = "stable-channel") { "stable" } else { "beta" };
        assert_eq!(bundled_publisher().unwrap().unwrap().channel, expected);
    }
}
