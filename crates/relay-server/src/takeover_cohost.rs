//! Setting up takeover between real machines, milestone 1: the co-host side.
//!
//! Used by the app on an approved co-host's computer. Everything stays in one
//! private folder per community: this machine's own takeover key, the parts of
//! a copy being fetched, and the installed standby image with its retained log.
//! The app does the HTTP over its existing connection to the host; this module
//! never opens a connection itself.
use crate::{
    handover_target::{private_directory, TargetKey},
    takeover_setup::{enroll_message, EnrollRequest, Manifest},
    takeover_standby::{Limits, Standby, IMAGE_NAME},
    takeover_store::{Quota, TakeoverStore},
    takeover_voter::Voter,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::{
    handover::{HostIdentity, HostProof, RoutePin},
    takeover::{self as wire, AcceptBody, AuthoritySet, CandidateAccept},
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

type Error = Box<dyn std::error::Error + Send + Sync>;

const QUOTA: Quota = Quota {
    max_entries: 1 << 20,
    max_entry_bytes: 16 * 1024 * 1024,
};

/// This computer's takeover identity for one community.
pub struct CohostKey {
    key: TargetKey,
    dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Installed {
    snapshot_id: String,
    sha256: String,
    epoch: u64,
}

impl CohostKey {
    pub fn open(dir: &Path) -> Result<Self, Error> {
        private_directory(dir).map_err(|e| e.to_string())?;
        Ok(Self {
            key: TargetKey::open(dir).map_err(|e| e.to_string())?,
            dir: dir.to_owned(),
        })
    }

    /// The private folder for this community on this computer.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn identity(&self) -> HostIdentity {
        self.key.identity()
    }

    fn recipient(&self) -> Result<String, Error> {
        let keys = self.key.encryption_keys().map_err(|e| e.to_string())?;
        Ok(URL_SAFE_NO_PAD.encode(keys.public_key().as_slice()))
    }

    /// Proves to the host that this machine holds its key.
    pub fn enroll(&self, server_id: &str, user_id: &str) -> Result<EnrollRequest, Error> {
        let recipient_key = self.recipient()?;
        Ok(EnrollRequest {
            identity: self.identity(),
            proof: self.key.sign(&enroll_message(server_id, user_id, &recipient_key)),
            recipient_key,
        })
    }

    /// Signs consent to exactly this set, after checking that the host this app
    /// pinned signed it and that this machine is one of its two candidates.
    pub fn consent(&self, set: &AuthoritySet, pinned: &RoutePin) -> Result<CandidateAccept, Error> {
        let me = self.identity();
        if set.validate().is_err()
            || &set.body.pin != pinned
            || !set.body.is_candidate(&me)
            || !crate::handover_target::verify(
                &HostProof {
                    identity: pinned.host.clone(),
                    signature: set.signature.clone(),
                },
                &set.body.message(),
            )
        {
            return Err("The takeover agreement is not signed by this community's host".into());
        }
        let body = AcceptBody {
            schema: wire::SCHEMA,
            server_id: set.body.pin.server_id.clone(),
            set_sha256: format!("{:x}", Sha256::digest(set.body.message())),
            epoch: set.body.epoch,
            candidate: me,
            recipient_key: self.recipient()?,
            full_data: true,
            takeover: true,
        };
        Ok(CandidateAccept {
            signature: self.key.sign(&body.message()),
            body,
        })
    }

    fn incoming(&self, manifest: &Manifest) -> PathBuf {
        self.dir.join("incoming").join(&manifest.snapshot_id)
    }

    /// Fetches every part not already here and checked. An interruption leaves the
    /// good parts; the next call fetches only the rest. Returns the complete image.
    pub fn pull(
        &self,
        manifest: &Manifest,
        mut fetch: impl FnMut(usize) -> Result<Vec<u8>, String>,
    ) -> Result<PathBuf, Error> {
        if manifest.parts.is_empty() || uuid::Uuid::parse_str(&manifest.snapshot_id).is_err() {
            return Err("Invalid copy description".into());
        }
        // Parts of an older copy are of no use any more.
        if let Ok(entries) = std::fs::read_dir(self.dir.join("incoming")) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy() != manifest.snapshot_id {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let folder = self.incoming(manifest);
        private_directory(&folder).map_err(|e| e.to_string())?;
        for (index, expected) in manifest.parts.iter().enumerate() {
            let path = folder.join(format!("{index}.part"));
            let have = std::fs::read(&path)
                .ok()
                .is_some_and(|b| format!("{:x}", Sha256::digest(&b)) == *expected);
            if have {
                continue;
            }
            let bytes = fetch(index)?;
            if format!("{:x}", Sha256::digest(&bytes)) != *expected {
                return Err("A part of the copy arrived damaged; try again".into());
            }
            let pending = folder.join(format!("{index}.pending"));
            std::fs::write(&pending, &bytes)?;
            std::fs::rename(&pending, &path)?;
        }
        let image = folder.join("image.sqlite3");
        let mut whole = Sha256::new();
        let mut out = Vec::with_capacity(manifest.bytes as usize);
        for index in 0..manifest.parts.len() {
            let bytes = std::fs::read(folder.join(format!("{index}.part")))?;
            whole.update(&bytes);
            out.extend(bytes);
        }
        if out.len() as u64 != manifest.bytes || format!("{:x}", whole.finalize()) != manifest.sha256 {
            return Err("The copy does not match what the host prepared".into());
        }
        std::fs::write(&image, out)?;
        Ok(image)
    }

    /// Installs a complete, checked image as this machine's standby copy for the
    /// set it consented to, replacing an older copy of its own.
    pub fn install(
        &self,
        set: &AuthoritySet,
        accept: &CandidateAccept,
        manifest: &Manifest,
        image: &Path,
    ) -> Result<(), Error> {
        let check = rusqlite::Connection::open_with_flags(
            image,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let ok: String = check.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if ok != "ok" || crate::community::server_id(&check)? != set.body.pin.server_id {
            return Err("The copy failed its check".into());
        }
        drop(check);
        // Set the checked image aside before the older copy and parts are removed.
        let pending = self.dir.join("install.pending.sqlite3");
        let _ = std::fs::remove_file(&pending);
        std::fs::rename(image, &pending)?;
        self.forget_copy();
        let standby = self.dir.join("standby");
        private_directory(&standby).map_err(|e| e.to_string())?;
        std::fs::rename(&pending, standby.join(IMAGE_NAME))?;
        let identity = self.identity();
        Standby::create_new(&standby, set, &identity, &manifest.base, Limits::DEFAULT)
            .map_err(|e| format!("{e:?}"))?;
        Voter::initialize(
            TakeoverStore::create_new(&self.dir.join("log"), QUOTA).map_err(|e| format!("{e:?}"))?,
            self.key.takeover_signer(),
            set.clone(),
            Some(accept.clone()),
            manifest.base.clone(),
        )
        .map_err(|e| format!("{e:?}"))?;
        let record = Installed {
            snapshot_id: manifest.snapshot_id.clone(),
            sha256: manifest.sha256.clone(),
            epoch: set.body.epoch,
        };
        crate::handover_transfer::publish_new(&self.dir.join("installed.json"), &serde_json::to_vec(&record)?)?;
        crate::handover_transfer::publish_new(
            &self.dir.join("agreement.json"),
            &serde_json::to_vec(&(set, accept))?,
        )?;
        let _ = std::fs::remove_dir_all(self.dir.join("incoming"));
        Ok(())
    }

    /// The installed copy, if any: its snapshot ID.
    pub fn installed(&self) -> Option<String> {
        let raw = std::fs::read(self.dir.join("installed.json")).ok()?;
        serde_json::from_slice::<Installed>(&raw).ok().map(|i| i.snapshot_id)
    }

    /// Removes this machine's copy (after the role was revoked). The key stays.
    pub fn forget_copy(&self) {
        for name in ["standby", "log", "incoming"] {
            let _ = std::fs::remove_dir_all(self.dir.join(name));
        }
        let _ = std::fs::remove_file(self.dir.join("installed.json"));
        let _ = std::fs::remove_file(self.dir.join("agreement.json"));
    }

    /// Opens the installed copy to follow the host's live changes.
    pub fn follower(&self) -> Result<Follower, Error> {
        let raw = std::fs::read(self.dir.join("agreement.json"))?;
        let (set, _accept): (AuthoritySet, CandidateAccept) = serde_json::from_slice(&raw)?;
        let identity = self.identity();
        let voter = Voter::open(
            TakeoverStore::open_existing(&self.dir.join("log"), QUOTA).map_err(|e| format!("{e:?}"))?,
            self.key.takeover_signer(),
        )
        .map_err(|e| format!("{e:?}"))?;
        let standby = Standby::open_existing(&self.dir.join("standby"), &set, &identity, Limits::DEFAULT)
            .map_err(|e| format!("{e:?}"))?;
        Ok(Follower {
            replica: crate::takeover_runtime::Replica::new(voter, standby, &set)
                .map_err(|e| e.0.to_string())?,
        })
    }
}

/// This computer's copy, following the host one change at a time.
pub struct Follower {
    replica: crate::takeover_runtime::Replica,
}

impl Follower {
    /// The JSON body of the next feed request.
    pub fn request(&self, feed_key: &str) -> Vec<u8> {
        serde_json::to_vec(&crate::takeover_live::FeedRequest {
            key: feed_key.into(),
            after: self.replica.tip().index,
            applied: self.replica.applied().index,
        })
        .expect("bounded request")
    }

    /// Applies one feed reply and returns the JSON body of the answers to post.
    /// Entries are retained and acknowledged; commit notices apply whole
    /// transactions to the standby image, in order.
    pub fn apply(&mut self, reply: &[u8], feed_key: &str) -> Result<Vec<u8>, Error> {
        let reply: crate::takeover_live::FeedReply = serde_json::from_slice(reply)?;
        let mut answers = Vec::new();
        for entry in &reply.entries {
            if entry.body.index <= self.replica.tip().index {
                continue;
            }
            match self.replica.append(entry) {
                Ok(answer) => answers.push((entry.body.index, answer.into())),
                Err(_) => break,
            }
        }
        for notice in &reply.notices {
            if notice.end.index <= self.replica.applied().index {
                continue;
            }
            if self.replica.committed(notice).is_err() {
                break;
            }
        }
        Ok(serde_json::to_vec(&crate::takeover_live::Answers {
            key: feed_key.into(),
            answers,
        })?)
    }

    /// Highest applied position: everything up to here is in the standby image.
    pub fn applied_index(&self) -> u64 {
        self.replica.applied().index
    }
    pub fn retained_index(&self) -> u64 {
        self.replica.tip().index
    }

    /// Hands the copy to the takeover logic (used by the outage path and tests).
    #[cfg(test)]
    pub(crate) fn into_replica(self) -> crate::takeover_runtime::Replica {
        self.replica
    }
}
