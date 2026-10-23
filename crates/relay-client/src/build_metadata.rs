//! Information embedded by build.rs; it never reads the user's profile or current clock.
#[path = "../build_support/build_stamp.rs"]
mod build_stamp;
#[cfg(test)]
#[path = "../build_support/source_identity.rs"]
mod source_identity;

use build_stamp::BuildStamp;
use std::sync::OnceLock;

pub struct BuildMetadata {
    pub version: &'static str,
    pub stamp: BuildStamp,
    pub source_sha256: &'static str,
    pub source_scope: &'static str,
    pub git_revision: &'static str,
    pub git_dirty: Option<bool>,
}

impl BuildMetadata {
    pub fn build_id(&self) -> String {
        format!("{}-{}", self.stamp.sequence, &self.source_sha256[..12])
    }

    pub fn label(&self) -> String {
        format!("Relay {} · {}", self.version, self.stamp.display)
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "invitation_protocol": if cfg!(windows) {1} else {0},
            "version": self.version,
            "build_id": self.build_id(),
            "sequence": self.stamp.sequence,
            "built_at": self.stamp.utc,
            "build_date": self.stamp.date,
            "source_sha256": self.source_sha256,
            "source_scope": self.source_scope,
            "git_revision": self.git_revision,
            "git_dirty": self.git_dirty,
        })
    }

    pub fn details(&self) -> String {
        let provenance = match self.git_dirty {
            Some(true) => "local changes included",
            Some(false) => "clean working tree at compilation",
            None => "working-tree status unavailable at compilation",
        };
        format!(
            "Build: {}\nBuilt: {}\nSource revision sampled at compilation: {} ({provenance})\nSource SHA-256: {}\nSource scope: {}",
            self.build_id(), self.stamp.utc, self.git_revision, self.source_sha256, self.source_scope,
        )
    }
}

pub fn current() -> &'static BuildMetadata {
    static METADATA: OnceLock<BuildMetadata> = OnceLock::new();
    METADATA.get_or_init(|| BuildMetadata {
        version: env!("CARGO_PKG_VERSION"),
        stamp: BuildStamp::resolve(Some(env!("RELAY_BUILD_SEQUENCE")), None, 0)
            .expect("build.rs must emit a valid UTC build sequence"),
        source_sha256: env!("RELAY_SOURCE_SHA256"),
        source_scope: env!("RELAY_SOURCE_SCOPE"),
        git_revision: env!("RELAY_GIT_SHA"),
        git_dirty: match env!("RELAY_GIT_DIRTY") {
            "true" => Some(true),
            "false" => Some(false),
            "unknown" => None,
            _ => panic!("build.rs emitted an invalid Git state"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata(sequence: &str, digest: &'static str) -> BuildMetadata {
        BuildMetadata {
            version: "0.10.0",
            stamp: BuildStamp::parse(sequence).unwrap(),
            source_sha256: digest,
            source_scope: "relay-client-inputs-v2",
            git_revision: "0123456789ab",
            git_dirty: Some(true),
        }
    }

    #[test]
    fn same_day_builds_and_different_dirty_inputs_have_distinct_support_identity() {
        let first = metadata(
            "20261015090102",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let later = metadata("20261015090103", first.source_sha256);
        assert_eq!(first.label(), later.label());
        assert_ne!(first.details(), later.details());
        assert_ne!(first.build_id(), later.build_id());
        let different_dirty_source = metadata(
            "20261015090102",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        assert_eq!(first.git_revision, different_dirty_source.git_revision);
        assert_eq!(first.git_dirty, different_dirty_source.git_dirty);
        assert_ne!(first.build_id(), different_dirty_source.build_id());
        assert_ne!(
            first.json()["source_sha256"],
            different_dirty_source.json()["source_sha256"]
        );
    }

    #[test]
    fn release_sequence_is_the_same_identifier_exposed_to_installer_binding() {
        let info = metadata(
            "20261015235959",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let json = info.json();
        assert_eq!(json["schema"], 1);
        assert_eq!(
            json["invitation_protocol"],
            if cfg!(windows) { 1 } else { 0 }
        );
        assert_eq!(json["version"], "0.10.0");
        assert_eq!(json["sequence"], info.stamp.sequence);
        assert_eq!(json["built_at"], "2026-10-15T23:59:59Z");
        assert_eq!(json["git_dirty"], true);
        assert_eq!(json["source_sha256"].as_str().unwrap().len(), 64);
        assert!(!info.label().contains("dirty"));
        assert!(!info.label().contains(info.git_revision));
    }

    #[test]
    fn no_git_details_are_explicit_and_do_not_claim_clean_source() {
        let mut info = metadata(
            "20261015000000",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        info.git_revision = "nogit";
        info.git_dirty = None;
        assert_eq!(info.json()["git_revision"], "nogit");
        assert!(info.json()["git_dirty"].is_null());
        assert!(info
            .details()
            .contains("working-tree status unavailable at compilation"));
        assert!(!info.details().contains("clean working tree"));
    }
}
