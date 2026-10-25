//! Complete transactions carried by several retained, signed log entries.
//!
//! The last entry seals the exact ordered bytes. An orphan prefix is never a
//! transaction. This layer proves retention and integrity, NOT quorum commit:
//! the caller must separately obtain `commit_gate` approval for the seal before
//! publishing or applying anything. Real WAL admission/recovery is not wired here.
#![allow(dead_code)]

use crate::takeover_voter::{HostSigner, LogStore, Refused, Voter};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::{
    handover::HostIdentity,
    takeover::{EntryBody, Payload, Tip},
};
use sha2::{Digest, Sha256};
use std::io::Write;
use uuid::Uuid;

pub(crate) const PART_BYTES: usize = 256 * 1024;
const MAGIC: &[u8; 8] = b"RLTX\x01\0\0\0";
const HEADER: usize = 8 + 16 + 4 + 1;
const MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_PARTS: u32 = 262_144;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Invalid,
    Retained(Refused),
    Output,
}

/// A bounded encoder. The declared size must be reached exactly before sealing;
/// a read/capture failure must discard this value, leaving only an orphan prefix.
pub(crate) struct Parts {
    id: [u8; 16],
    expected: u64,
    bytes: u64,
    count: u32,
    hash: Sha256,
    failed: bool,
}
impl Parts {
    pub(crate) fn new(expected: u64) -> Result<Self, Error> {
        if expected == 0 || expected > MAX_BYTES {
            return Err(Error::Invalid);
        }
        Ok(Self {
            id: *Uuid::new_v4().as_bytes(),
            expected,
            bytes: 0,
            count: 0,
            hash: Sha256::new(),
            failed: false,
        })
    }
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Payload, Error> {
        if self.failed
            || data.is_empty()
            || data.len() > PART_BYTES
            || self.count >= MAX_PARTS
            || self.bytes + data.len() as u64 > self.expected
        {
            self.failed = true;
            return Err(Error::Invalid);
        }
        let payload = encode(self.id, self.count, 0, data)?;
        self.hash.update(data);
        self.bytes += data.len() as u64;
        self.count += 1;
        Ok(payload)
    }
    pub(crate) fn seal(self) -> Result<Payload, Error> {
        if self.failed || self.count == 0 || self.bytes != self.expected {
            return Err(Error::Invalid);
        }
        let mut body = Vec::with_capacity(40);
        body.extend_from_slice(&self.bytes.to_be_bytes());
        body.extend_from_slice(&self.hash.finalize());
        encode(self.id, self.count, 1, &body)
    }
}
fn encode(id: [u8; 16], sequence: u32, kind: u8, data: &[u8]) -> Result<Payload, Error> {
    let mut bytes = Vec::with_capacity(HEADER + data.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&id);
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(data);
    Payload::new(URL_SAFE_NO_PAD.encode(bytes)).map_err(|_| Error::Invalid)
}

struct Part {
    id: [u8; 16],
    sequence: u32,
    kind: u8,
    body: Vec<u8>,
}
impl Part {
    fn decode(payload: &Payload) -> Result<Self, Error> {
        if payload.decoded_len() < HEADER || payload.decoded_len() > HEADER + PART_BYTES {
            return Err(Error::Invalid);
        }
        payload.validate().map_err(|_| Error::Invalid)?;
        let mut raw = URL_SAFE_NO_PAD
            .decode(payload.as_str())
            .map_err(|_| Error::Invalid)?;
        if &raw[..8] != MAGIC {
            return Err(Error::Invalid);
        }
        let id = raw[8..24].try_into().map_err(|_| Error::Invalid)?;
        let sequence = u32::from_be_bytes(raw[24..28].try_into().map_err(|_| Error::Invalid)?);
        let kind = raw[28];
        if !matches!(kind, 0 | 1)
            || sequence > MAX_PARTS
            || (kind == 0 && raw.len() == HEADER)
            || (kind == 1 && raw.len() != HEADER + 40)
        {
            return Err(Error::Invalid);
        }
        let body = raw.split_off(HEADER);
        Ok(Self {
            id,
            sequence,
            kind,
            body,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    server_id: String,
    set: String,
    epoch: u64,
    term: u64,
    leader: HostIdentity,
}
impl From<&EntryBody> for Binding {
    fn from(body: &EntryBody) -> Self {
        Self {
            server_id: body.server_id.clone(),
            set: body.set_sha256.clone(),
            epoch: body.epoch,
            term: body.term,
            leader: body.leader.clone(),
        }
    }
}

/// Verified retained bytes, with no authority to apply them. Parent and end MUST
/// come from the caller's trusted snapshot/applied/commit state, not peer hints.
/// No byte payload or transaction identifier is exposed in Debug.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RetainedTransaction {
    parent: Tip,
    end: Tip,
    binding: Binding,
    id: [u8; 16],
    parts: u32,
    bytes: u64,
    digest: [u8; 32],
}
impl std::fmt::Debug for RetainedTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedTransaction")
            .field("parts", &self.parts)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}
impl RetainedTransaction {
    pub(crate) fn inspect<J: LogStore, S: HostSigner>(
        voter: &mut Voter<J, S>,
        parent: &Tip,
        end: &Tip,
    ) -> Result<Self, Error> {
        read(voter, parent, end, |_| Ok(()))
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
    pub(crate) fn parent(&self) -> &Tip {
        &self.parent
    }
    pub(crate) fn end(&self) -> &Tip {
        &self.end
    }

    /// Re-read and validate the retained chain while copying into PRIVATE STAGING.
    /// A write/storage fault can leave a partial staging output: the caller must
    /// discard it on Err, and may atomically apply only after this returns Ok AND
    /// separate quorum/admission checks succeed. Never point this at a live DB.
    pub(crate) fn copy_to_staging<J: LogStore, S: HostSigner>(
        &self,
        voter: &mut Voter<J, S>,
        output: &mut impl Write,
    ) -> Result<(), Error> {
        let again = read(voter, &self.parent, &self.end, |bytes| {
            output.write_all(bytes).map_err(|_| Error::Output)
        })?;
        if again != *self {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

fn read<J: LogStore, S: HostSigner>(
    voter: &mut Voter<J, S>,
    parent: &Tip,
    end: &Tip,
    mut data: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<RetainedTransaction, Error> {
    parent.validate().map_err(|_| Error::Invalid)?;
    end.validate().map_err(|_| Error::Invalid)?;
    let distance = end.index.checked_sub(parent.index).ok_or(Error::Invalid)?;
    if !(2..=u64::from(MAX_PARTS) + 1).contains(&distance) || end.term < parent.term {
        return Err(Error::Invalid);
    }
    let mut previous = parent.digest.clone();
    let mut binding = None;
    let mut id = None;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    for offset in 0..distance {
        let index = parent.index.checked_add(offset + 1).ok_or(Error::Invalid)?;
        let entry = voter.entry(index).map_err(Error::Retained)?;
        if entry.body.index != index
            || entry.body.previous != previous
            || entry.body.term != end.term
        {
            return Err(Error::Invalid);
        }
        let current_binding = Binding::from(&entry.body);
        if binding.as_ref().is_some_and(|b| *b != current_binding) {
            return Err(Error::Invalid);
        }
        binding = Some(current_binding);
        previous = format!("{:x}", Sha256::digest(entry.body.digest_message()));
        let part = Part::decode(&entry.body.payload)?;
        if part.sequence as u64 != offset || id.is_some_and(|id| id != part.id) {
            return Err(Error::Invalid);
        }
        id = Some(part.id);
        if index != end.index {
            if part.kind != 0 {
                return Err(Error::Invalid);
            }
            bytes = bytes
                .checked_add(part.body.len() as u64)
                .ok_or(Error::Invalid)?;
            if bytes > MAX_BYTES {
                return Err(Error::Invalid);
            }
            hash.update(&part.body);
            data(&part.body)?;
        } else {
            if part.kind != 1 || previous != end.digest {
                return Err(Error::Invalid);
            }
            let declared =
                u64::from_be_bytes(part.body[..8].try_into().map_err(|_| Error::Invalid)?);
            let digest: [u8; 32] = hash.finalize().into();
            if bytes != declared || bytes == 0 || part.body[8..] != digest {
                return Err(Error::Invalid);
            }
            return Ok(RetainedTransaction {
                parent: parent.clone(),
                end: end.clone(),
                binding: binding.ok_or(Error::Invalid)?,
                id: part.id,
                parts: part.sequence,
                bytes,
                digest,
            });
        }
    }
    Err(Error::Invalid)
}
