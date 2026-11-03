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
        archived_bytes: state
            .history
            .iter()
            .chain(&state.recovered_history)
            .map(|s| s.bytes)
            .sum(),
        byte_limit: MAX_HISTORY_BYTES,
        segments: state.history.len() + state.recovered_history.len(),
        segment_limit: MAX_SEGMENTS,
        records: state.cached.len()
            + state
                .history
                .iter()
                .chain(&state.recovered_history)
                .map(|s| s.count)
                .sum::<usize>(),
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
        if state.history.len() + state.recovered_history.len() >= MAX_SEGMENTS {
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
        let segment = write_segment(
            path,
            binding,
            &state.identity,
            entries,
            state
                .history
                .iter()
                .chain(&state.recovered_history)
                .map(|s| s.bytes)
                .sum(),
        )?;
        state.history.push(segment);
        for (hash, _, _) in candidates {
            state.cached.remove(&hash);
        }
    }
    Ok(())
}

fn write_segment(
    path: &Path,
    binding: &str,
    identity: &Identity,
    entries: HashMap<String, CachedEnvelope>,
    already_bytes: u64,
) -> Result<Segment, String> {
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
                .filter(|e| e.author == *identity)
                .map(|e| e.envelope.client_id.clone()),
        ),
    };
    let contents = Contents {
        version: 1,
        identity: identity.clone(),
        entries,
    };
    let plain = Zeroizing::new(serde_json::to_vec(&contents).map_err(|_| invalid())?);
    if plain.len() > MAX_SEGMENT_PLAIN {
        return Err("Protected history segment exceeds its storage limit.".into());
    }
    let bytes = protected::protect(&plain, &entropy(binding, identity, &segment))?;
    segment.bytes = bytes.len() as u64;
    segment.sha256 = format!("{:x}", Sha256::digest(&bytes));
    if segment.bytes > MAX_SEGMENT_BYTES || already_bytes + segment.bytes > MAX_HISTORY_BYTES {
        return Err("Protected history reached its 1 GiB archive quota. Existing history and keys were preserved.".into());
    }
    let directory = root(path, identity);
    std::fs::create_dir_all(&directory).map_err(|_| "Cannot create protected history storage.")?;
    regular(&directory, true)?;
    let destination = directory.join(&segment.file);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&destination)
        .map_err(|_| "Cannot create protected history segment.")?;
    let written = file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Cannot save protected history; encryption is paused.".to_string());
    drop(file);
    // Failed staging never leaves an uncharged partial file behind.
    if let Err(error) = written.and_then(|_| read(path, binding, identity, &segment).map(|_| ())) {
        if regular(&destination, false).is_ok() {
            let _ = std::fs::remove_file(&destination);
        }
        return Err(error);
    }
    Ok(segment)
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

pub(super) fn recovered_records<'a>(
    state: &'a PrivateState,
    path: &'a Path,
    binding: String,
) -> impl Iterator<Item = Result<(String, CachedEnvelope), String>> + 'a {
    state.recovered_history.iter().flat_map(move |segment| {
        match read(path, &binding, &state.identity, segment) {
            Ok(entries) => entries.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(error) => vec![Err(error)],
        }
    })
}
pub(super) fn recovered_lookup(
    state: &PrivateState,
    path: &Path,
    binding: &str,
    hash: &str,
) -> Result<Option<CachedEnvelope>, String> {
    for segment in state.recovered_history.iter().rev() {
        if may_contain(&segment.wires, hash)? {
            if let Some(row) = read(path, binding, &state.identity, segment)?.remove(hash) {
                return Ok(Some(row));
            }
        }
    }
    Ok(None)
}

pub(crate) struct TransferSource {
    identity: Identity,
    path: PathBuf,
    binding: String,
    active: HashMap<String, CachedEnvelope>,
    history: Vec<Segment>,
    recovered: Vec<Segment>,
}
pub(super) fn transfer_source(
    state: &PrivateState,
    path: &Path,
    binding: String,
) -> TransferSource {
    TransferSource {
        identity: state.identity.clone(),
        path: path.into(),
        binding,
        active: state.cached.clone(),
        history: state.history.clone(),
        recovered: state.recovered_history.clone(),
    }
}
impl TransferSource {
    pub(crate) fn snapshot(
        &self,
        origin: &crate::network::CommandOrigin,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::history_transfer::Snapshot, String> {
        self.check(origin)?;
        let mut snapshot = crate::history_transfer::Snapshot {
            version: 1,
            server_id: origin.server_id.clone(),
            user_id: origin.user_id.clone(),
            source_device: self.identity.device_id.clone(),
            entries: HashMap::new(),
        };
        let mut bytes = 2048usize;
        let mut push = |hash: String, mut row: CachedEnvelope| -> Result<(), String> {
            cancelled(cancel)?;
            if row.message_id.is_none() || row.sequence == 0 || snapshot.entries.contains_key(&hash)
            {
                return Ok(());
            }
            row.delivery_revision = 0;
            bytes = bytes.saturating_add(
                Zeroizing::new(serde_json::to_vec(&row).map_err(|_| invalid())?).len() + 72,
            );
            if bytes > crate::history_transfer::MAX_BYTES - 48 || snapshot.entries.len() >= 100_000
            {
                return Err(
                    "History exceeds the 32 MiB / 100,000 record transfer limit. Nothing was sent."
                        .into(),
                );
            }
            snapshot.entries.insert(hash, row);
            Ok(())
        };
        for (hash, row) in &self.active {
            push(hash.clone(), row.clone())?;
        }
        for segment in self.history.iter().chain(&self.recovered) {
            cancelled(cancel)?;
            for (hash, row) in read(&self.path, &self.binding, &self.identity, segment)? {
                push(hash, row)?;
            }
        }
        snapshot.validate(origin)?;
        Ok(snapshot)
    }
    fn check(&self, origin: &crate::network::CommandOrigin) -> Result<(), String> {
        if self.identity.server_id != origin.server_id || self.identity.user_id != origin.user_id {
            return Err("History account changed.".into());
        }
        Ok(())
    }
    pub(crate) fn stage(
        &self,
        mut snapshot: crate::history_transfer::Snapshot,
        origin: &crate::network::CommandOrigin,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<PreparedTransfer, String> {
        self.check(origin)?;
        snapshot.validate(origin)?;
        cancelled(cancel)?;
        let mut incoming = std::mem::take(&mut snapshot.entries);
        for row in incoming.values_mut() {
            row.delivery_revision = 0;
        }
        // Read each immutable segment once, rather than once for every incoming row.
        for hash in self.active.keys() {
            incoming.remove(hash);
        }
        for segment in &self.history {
            cancelled(cancel)?;
            if incoming.is_empty() {
                break;
            }
            for hash in read(&self.path, &self.binding, &self.identity, segment)?.keys() {
                incoming.remove(hash);
            }
        }
        for segment in &self.recovered {
            cancelled(cancel)?;
            if incoming.is_empty() {
                break;
            }
            for (hash, mut previous) in read(&self.path, &self.binding, &self.identity, segment)? {
                if let Some(row) = incoming.remove(&hash) {
                    previous.delivery_revision = 0;
                    let a = Zeroizing::new(serde_json::to_vec(&previous).map_err(|_| invalid())?);
                    let b = Zeroizing::new(serde_json::to_vec(&row).map_err(|_| invalid())?);
                    if a != b {
                        return Err("Transferred history conflicts with an earlier transfer. Nothing was imported.".into());
                    }
                }
            }
        }
        let count = incoming.len();
        if self.history.len() + self.recovered.len() + count.div_ceil(SEGMENT_ENTRIES)
            > MAX_SEGMENTS
        {
            return Err("History storage is full.".into());
        }
        let mut prepared = PreparedTransfer {
            identity: self.identity.clone(),
            path: self.path.clone(),
            binding: self.binding.clone(),
            previous: self.recovered.iter().map(|s| s.file.clone()).collect(),
            segments: Vec::new(),
            count,
            committed: false,
        };
        let mut used = self
            .history
            .iter()
            .chain(&self.recovered)
            .map(|s| s.bytes)
            .sum::<u64>();
        let mut remaining = incoming.into_iter();
        loop {
            cancelled(cancel)?;
            let chunk: HashMap<_, _> = remaining.by_ref().take(SEGMENT_ENTRIES).collect();
            if chunk.is_empty() {
                return Ok(prepared);
            }
            let segment = write_segment(&self.path, &self.binding, &self.identity, chunk, used)?;
            used += segment.bytes;
            prepared.segments.push(segment);
        }
    }
}
fn cancelled(cancel: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        Err("History transfer was cancelled.".into())
    } else {
        Ok(())
    }
}
pub(crate) struct PreparedTransfer {
    identity: Identity,
    path: PathBuf,
    binding: String,
    previous: Vec<String>,
    segments: Vec<Segment>,
    pub(crate) count: usize,
    committed: bool,
}
impl PreparedTransfer {
    pub(super) fn attach(
        &self,
        state: &mut PrivateState,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), String> {
        cancelled(cancel)?;
        if state.identity != self.identity
            || state
                .recovered_history
                .iter()
                .map(|s| &s.file)
                .ne(self.previous.iter())
        {
            return Err("The device or its history changed during transfer. Try again.".into());
        }
        let used = state
            .history
            .iter()
            .chain(&state.recovered_history)
            .chain(&self.segments)
            .map(|s| s.bytes)
            .sum::<u64>();
        if used > MAX_HISTORY_BYTES
            || state.history.len() + state.recovered_history.len() + self.segments.len()
                > MAX_SEGMENTS
        {
            return Err("History storage is full. Existing messages were preserved.".into());
        }
        state
            .recovered_history
            .extend(self.segments.iter().cloned());
        Ok(())
    }
    pub(super) fn committed(&mut self) {
        self.committed = true;
    }
}
impl Drop for PreparedTransfer {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let directory = root(&self.path, &self.identity);
        if regular(&directory, true).is_err() {
            return;
        }
        // save() installs the checkpoint before updating its Windows marker. A marker
        // failure can therefore return an error AFTER the new manifest is durable.
        // Keep every referenced segment, and keep all staged files if the installed
        // checkpoint cannot be authenticated. Never turn an uncertain save into loss.
        let installed = (|| -> Result<PrivateState, String> {
            let meta = regular(&self.path, false)?;
            if meta.len() > (MAX_PRIVATE_BYTES + 1024 * 1024) as u64 {
                return Err(invalid());
            }
            let bytes = std::fs::read(&self.path).map_err(|_| invalid())?;
            let plain = protected::unprotect(&bytes, &self.binding)?;
            serde_json::from_slice(&plain).map_err(|_| invalid())
        })();
        let Ok(installed) = installed else {
            return;
        };
        for segment in &self.segments {
            if installed
                .history
                .iter()
                .chain(&installed.recovered_history)
                .any(|saved| saved.file == segment.file && saved.sha256 == segment.sha256)
            {
                continue;
            }
            // Only our newly-created UUID files, verified unchanged, may be removed.
            if blob(&self.path, &self.identity, segment).is_ok() {
                let _ = std::fs::remove_file(directory.join(&segment.file));
            }
        }
    }
}
