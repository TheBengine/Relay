//! Immutable DPAPI history segments. Only a committed device checkpoint makes a
//! segment reachable; abandoned writes never authorize cached plaintext.
use super::*;
use std::{collections::HashSet, io::Write, path::Path};

pub(super) const ACTIVE_LIMIT: usize = 1024;
const SEGMENT_ENTRIES: usize = 256;
const MAX_SEGMENTS: usize = 4096;
const MAX_SEGMENT_PLAIN: usize = 12 * 1024 * 1024;
const MAX_SEGMENT_BYTES: u64 = (MAX_SEGMENT_PLAIN + 1024 * 1024) as u64;
const MAX_HISTORY_BYTES: u64 = 1024 * 1024 * 1024;
const FILTER_BYTES: usize = 512;

pub(super) fn usage(state: &PrivateState) -> HistoryUsage {
    HistoryUsage {
        archived_bytes: state.history.iter().map(|s| s.bytes).sum(),
        byte_limit: MAX_HISTORY_BYTES,
        segments: state.history.len(),
        segment_limit: MAX_SEGMENTS,
        records: state.cached.len() + state.history.iter().map(|s| s.count).sum::<usize>(),
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Segment {
    file: String,
    sha256: String,
    bytes: u64,
    count: usize,
    scopes: Vec<String>,
    wires: String,
    sends: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contents {
    version: u32,
    identity: Identity,
    entries: HashMap<String, CachedEnvelope>,
}
fn positions(value: &str) -> [usize; 4] {
    let digest = Sha256::digest(value.as_bytes());
    std::array::from_fn(|i| {
        u16::from_le_bytes([digest[i * 2], digest[i * 2 + 1]]) as usize % (FILTER_BYTES * 8)
    })
}
fn filter(values: impl Iterator<Item = String>) -> String {
    let mut bits = vec![0u8; FILTER_BYTES];
    for value in values {
        for position in positions(&value) {
            bits[position / 8] |= 1 << (position % 8);
        }
    }
    STANDARD.encode(bits)
}
fn may_contain(encoded: &str, value: &str) -> Result<bool, String> {
    let bits = STANDARD.decode(encoded).map_err(|_| invalid())?;
    if bits.len() != FILTER_BYTES {
        return Err(invalid());
    }
    Ok(positions(value)
        .iter()
        .all(|position| bits[position / 8] & (1 << (position % 8)) != 0))
}
fn invalid() -> String {
    "Protected history is missing or damaged. Restore this device's complete storage; its keys were preserved.".into()
}
fn root(path: &Path, identity: &Identity) -> PathBuf {
    path.with_extension(format!("{}.history", identity.device_id))
}
fn entropy(binding: &str, identity: &Identity, segment: &Segment) -> String {
    format!(
        "{binding}/{}/history/v1/{}",
        identity.device_id, segment.file
    )
}
fn regular(path: &Path, directory: bool) -> Result<std::fs::Metadata, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| invalid())?;
    if metadata.is_dir() != directory || metadata.file_type().is_symlink() {
        return Err(invalid());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid());
        }
    }
    Ok(metadata)
}
pub(super) fn validate(segments: &[Segment]) -> Result<(), String> {
    if segments.len() > MAX_SEGMENTS {
        return Err(invalid());
    }
    let mut bytes = 0u64;
    let mut names = HashSet::new();
    for segment in segments {
        let Some(stem) = segment.file.strip_suffix(".history") else {
            return Err(invalid());
        };
        if uuid::Uuid::parse_str(stem).is_err()
            || stem.len() != 36
            || !names.insert(&segment.file)
            || segment.sha256.len() != 64
            || !segment.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || segment.bytes == 0
            || segment.bytes > MAX_SEGMENT_BYTES
            || segment.count == 0
            || segment.count > SEGMENT_ENTRIES
            || segment.scopes.is_empty()
            || segment.scopes.len() > SEGMENT_ENTRIES
            || segment.scopes.iter().any(|s| s.is_empty() || s.len() > 512)
        {
            return Err(invalid());
        }
        // Validate both membership filters, including manifests migrated from disk.
        may_contain(&segment.wires, "")?;
        may_contain(&segment.sends, "")?;
        bytes = bytes.checked_add(segment.bytes).ok_or_else(invalid)?;
    }
    if bytes > MAX_HISTORY_BYTES {
        return Err(invalid());
    }
    Ok(())
}
fn blob(path: &Path, identity: &Identity, segment: &Segment) -> Result<Vec<u8>, String> {
    let directory = root(path, identity);
    regular(&directory, true)?;
    let file = directory.join(&segment.file);
    let metadata = regular(&file, false)?;
    if metadata.len() != segment.bytes {
        return Err(invalid());
    }
    let bytes = std::fs::read(file).map_err(|_| invalid())?;
    if bytes.len() as u64 != segment.bytes
        || format!("{:x}", Sha256::digest(&bytes)) != segment.sha256
    {
        return Err(invalid());
    }
    Ok(bytes)
}
pub(super) fn verify_files(
    path: &Path,
    identity: &Identity,
    segments: &[Segment],
) -> Result<(), String> {
    validate(segments)?;
    for segment in segments {
        blob(path, identity, segment)?;
    }
    Ok(())
}
fn read(
    path: &Path,
    binding: &str,
    identity: &Identity,
    segment: &Segment,
) -> Result<HashMap<String, CachedEnvelope>, String> {
    let bytes = blob(path, identity, segment)?;
    decode(&bytes, binding, identity, segment)
}
fn decode(
    bytes: &[u8],
    binding: &str,
    identity: &Identity,
    segment: &Segment,
) -> Result<HashMap<String, CachedEnvelope>, String> {
    let plain = protected::unprotect(bytes, &entropy(binding, identity, segment))?;
    if plain.len() > MAX_SEGMENT_PLAIN {
        return Err(invalid());
    }
    let contents: Contents = serde_json::from_slice(&plain).map_err(|_| invalid())?;
    if contents.version != 1
        || contents.identity != *identity
        || contents.entries.len() != segment.count
    {
        return Err(invalid());
    }
    for (hash, entry) in &contents.entries {
        if hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || entry.author.server_id != identity.server_id
            || !segment.scopes.contains(&entry.envelope.scope)
            || !may_contain(&segment.wires, hash)?
            || (entry.author == *identity
                && !may_contain(&segment.sends, &entry.envelope.client_id)?)
        {
            return Err(invalid());
        }
    }
    Ok(contents.entries)
}
pub(super) fn compact(state: &mut PrivateState, path: &Path, binding: &str) -> Result<(), String> {
    state.delivery_clock = state.delivery_clock.max(
        state
            .cached
            .values()
            .map(|entry| entry.delivery_revision)
            .max()
            .unwrap_or(0),
    );
    let pending: HashSet<_> = state
        .outbox
        .values()
        .map(|(_, wire)| cipher_hash(wire))
        .collect();
    while state.cached.len() > ACTIVE_LIMIT {
        if state.history.len() >= MAX_SEGMENTS {
            return Err("Protected history reached its archive quota. Existing history and keys were preserved.".into());
        }
        let mut candidates: Vec<_> = state
            .cached
            .iter()
            .filter(|(hash, _)| !pending.contains(*hash))
            .map(|(hash, entry)| (hash.clone(), entry.delivery_revision, entry.sequence))
            .collect();
        // Bound entries first; pending outgoing ciphertext always stays in the checkpoint.
        candidates
            .sort_by_key(|(hash, revision, sequence)| (*sequence == 0, *revision, hash.clone()));
        candidates.truncate(SEGMENT_ENTRIES);
        if candidates.is_empty() {
            return Err("Protected pending message storage is full.".into());
        }
        let entries: HashMap<_, _> = candidates
            .iter()
            .map(|(hash, _, _)| (hash.clone(), state.cached[hash].clone()))
            .collect();
        let mut scopes: Vec<_> = entries.values().map(|e| e.envelope.scope.clone()).collect();
        scopes.sort_unstable();
        scopes.dedup();
        let mut segment = Segment {
            file: format!("{}.history", uuid::Uuid::new_v4()),
            sha256: String::new(),
            bytes: 0,
            count: entries.len(),
            scopes,
            wires: filter(entries.keys().cloned()),
            sends: filter(
                entries
                    .values()
                    .filter(|e| e.author == state.identity)
                    .map(|e| e.envelope.client_id.clone()),
            ),
        };
        let contents = Contents {
            version: 1,
            identity: state.identity.clone(),
            entries,
        };
        let plain = Zeroizing::new(serde_json::to_vec(&contents).map_err(|_| invalid())?);
        if plain.len() > MAX_SEGMENT_PLAIN {
            return Err("Protected history segment exceeds its storage limit.".into());
        }
        let bytes = protected::protect(&plain, &entropy(binding, &state.identity, &segment))?;
        segment.bytes = bytes.len() as u64;
        segment.sha256 = format!("{:x}", Sha256::digest(&bytes));
        if segment.bytes > MAX_SEGMENT_BYTES
            || state.history.iter().map(|s| s.bytes).sum::<u64>() + segment.bytes
                > MAX_HISTORY_BYTES
        {
            return Err("Protected history reached its 1 GiB archive quota. Existing history and keys were preserved.".into());
        }
        let directory = root(path, &state.identity);
        std::fs::create_dir_all(&directory)
            .map_err(|_| "Cannot create protected history storage.")?;
        regular(&directory, true)?;
        let destination = directory.join(&segment.file);
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .map_err(|_| "Cannot create protected history segment.")?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot save protected history; encryption is paused.")?;
        drop(file);
        // Verify the written content before the checkpoint can reference it.
        read(path, binding, &state.identity, &segment)?;
        state.history.push(segment);
        for (hash, _, _) in candidates {
            state.cached.remove(&hash);
        }
    }
    Ok(())
}
pub(super) fn lookup(
    state: &PrivateState,
    path: &Path,
    binding: &str,
    hash: &str,
) -> Result<Option<CachedEnvelope>, String> {
    if let Some(entry) = state.cached.get(hash) {
        return Ok(Some(entry.clone()));
    }
    for segment in state.history.iter().rev() {
        if may_contain(&segment.wires, hash)? {
            if let Some(entry) = read(path, binding, &state.identity, segment)?.remove(hash) {
                return Ok(Some(entry));
            }
        }
    }
    Ok(None)
}
pub(super) fn contains_send(
    state: &PrivateState,
    path: &Path,
    binding: &str,
    client_id: &str,
) -> Result<bool, String> {
    if state
        .cached
        .values()
        .any(|e| e.author == state.identity && e.envelope.client_id == client_id)
    {
        return Ok(true);
    }
    for segment in &state.history {
        if may_contain(&segment.sends, client_id)?
            && read(path, binding, &state.identity, segment)?
                .values()
                .any(|e| e.author == state.identity && e.envelope.client_id == client_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
pub(super) fn records<'a>(
    state: &'a PrivateState,
    path: &'a Path,
    binding: String,
    scope: Option<&'a str>,
) -> impl Iterator<Item = Result<CachedEnvelope, String>> + 'a {
    let active = state
        .cached
        .values()
        .filter(move |e| scope.is_none_or(|s| e.envelope.scope == s))
        .cloned()
        .map(Ok);
    let archives = state
        .history
        .iter()
        .filter(move |segment| scope.is_none_or(|s| segment.scopes.iter().any(|item| item == s)))
        .flat_map(
            move |segment| match read(path, &binding, &state.identity, segment) {
                Ok(entries) => entries
                    .into_values()
                    .filter(move |e| scope.is_none_or(|s| e.envelope.scope == s))
                    .map(Ok)
                    .collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            },
        );
    active.chain(archives)
}

#[cfg(test)]
pub(super) fn archive_paths(state: &PrivateState, path: &Path) -> Vec<PathBuf> {
    state
        .history
        .iter()
        .map(|segment| root(path, &state.identity).join(&segment.file))
        .collect()
}

// Only the current committed manifest selects segments. Cache entries never
// authorize delivery, and every hit rechecks the actual ciphertext bytes.
pub(super) fn records_cached<'a>(
    state: &'a PrivateState,
    path: &'a Path,
    binding: String,
    scope: &'a str,
    cache: &'a history_cache::Cache,
) -> impl Iterator<Item = Result<CachedEnvelope, String>> + 'a {
    let active = state
        .cached
        .values()
        .filter(move |e| e.envelope.scope == scope)
        .cloned()
        .map(Ok);
    let archives = state
        .history
        .iter()
        .filter(move |segment| segment.scopes.iter().any(|item| item == scope))
        .flat_map(move |segment| {
            let records = blob(path, &state.identity, segment).and_then(|bytes| {
                let manifest = serde_json::to_vec(segment).map_err(|_| invalid())?;
                let key = format!("{:x}", Sha256::digest(&manifest));
                if let Some(entries) = cache.get(&key) {
                    return Ok(entries);
                }
                let entries = decode(&bytes, &binding, &state.identity, segment)?;
                Ok(cache.insert(key, entries))
            });
            match records {
                Ok(entries) => entries
                    .values()
                    .filter(|e| e.envelope.scope == scope)
                    .cloned()
                    .map(Ok)
                    .collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            }
        });
    active.chain(archives)
}
