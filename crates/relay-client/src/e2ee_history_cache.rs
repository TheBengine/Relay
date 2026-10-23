//! Actor-owned reuse of immutable, authenticated history segments. No disk index.
use super::*;
use std::{
    collections::VecDeque,
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
};

const ACTOR_BYTES: usize = 96 * 1024 * 1024;
const PROCESS_BYTES: usize = 128 * 1024 * 1024;
const MAX_SEGMENTS: usize = 512;
static BUDGET: AtomicUsize = AtomicUsize::new(0);

struct Lease(usize);
impl Lease {
    fn reserve(bytes: usize) -> Option<Self> {
        reserve(&BUDGET, bytes, PROCESS_BYTES).then(|| Self(bytes))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        BUDGET.fetch_sub(self.0, Ordering::Relaxed);
    }
}
fn reserve(budget: &AtomicUsize, bytes: usize, limit: usize) -> bool {
    budget
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|next| *next <= limit)
        })
        .is_ok()
}
pub(super) struct Records {
    entries: HashMap<String, CachedEnvelope>,
    _lease: Option<Lease>,
}
impl Deref for Records {
    type Target = HashMap<String, CachedEnvelope>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}
#[derive(Default)]
struct State {
    entries: VecDeque<(String, usize, Arc<Records>)>,
    bytes: usize,
}
#[derive(Default)]
pub(super) struct Cache(Mutex<State>);
impl Cache {
    pub(super) fn get(&self, key: &str) -> Option<Arc<Records>> {
        let mut state = self.0.lock().ok()?;
        let position = state.entries.iter().position(|(k, _, _)| k == key)?;
        let entry = state.entries.remove(position)?;
        let records = entry.2.clone();
        state.entries.push_back(entry);
        Some(records)
    }
    pub(super) fn insert(
        &self,
        key: String,
        entries: HashMap<String, CachedEnvelope>,
    ) -> Arc<Records> {
        // Charge retained heap capacities and conservative per-allocation/hash
        // overhead. This bounds cache retention, not the application's total RSS.
        let bytes = charge(&entries)
            .saturating_add(key.capacity())
            .saturating_add(512);
        self.insert_charged(key, entries, bytes)
    }
    fn insert_charged(
        &self,
        key: String,
        entries: HashMap<String, CachedEnvelope>,
        bytes: usize,
    ) -> Arc<Records> {
        let Ok(mut state) = self.0.lock() else {
            return Arc::new(Records {
                entries,
                _lease: None,
            });
        };
        if bytes > ACTOR_BYTES {
            return Arc::new(Records {
                entries,
                _lease: None,
            });
        }
        while state.bytes.saturating_add(bytes) > ACTOR_BYTES || state.entries.len() >= MAX_SEGMENTS
        {
            let Some((_, old_bytes, _)) = state.entries.pop_front() else {
                break;
            };
            state.bytes -= old_bytes;
        }
        let lease = Lease::reserve(bytes);
        let retained = lease.is_some();
        let records = Arc::new(Records {
            entries,
            _lease: lease,
        });
        if retained {
            state.bytes += bytes;
            state.entries.push_back((key, bytes, records.clone()));
        }
        // A full budget falls back to the complete freshly decoded segment.
        records
    }
}
fn text_charge(value: &String) -> usize {
    value.capacity().saturating_add(32)
}
fn optional_charge(value: &Option<String>) -> usize {
    value.as_ref().map(text_charge).unwrap_or(0)
}
fn charge(entries: &HashMap<String, CachedEnvelope>) -> usize {
    let mut bytes = entries
        .capacity()
        .saturating_mul(std::mem::size_of::<(String, CachedEnvelope)>() + 32);
    for (key, entry) in entries {
        let envelope = &entry.envelope;
        for value in [
            key,
            &entry.author.server_id,
            &entry.author.user_id,
            &entry.author.device_id,
            &envelope.scope,
            &envelope.client_id,
            &envelope.content,
        ] {
            bytes = bytes.saturating_add(text_charge(value));
        }
        bytes = bytes
            .saturating_add(optional_charge(&entry.message_id))
            .saturating_add(optional_charge(&envelope.reply_to_id));
        bytes = bytes.saturating_add(
            envelope
                .mentions
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
        );
        for value in &envelope.mentions {
            bytes = bytes.saturating_add(text_charge(value));
        }
        bytes = bytes.saturating_add(
            envelope
                .attachments
                .capacity()
                .saturating_mul(std::mem::size_of::<EncryptedAttachment>()),
        );
        for file in &envelope.attachments {
            for value in [
                &file.id,
                &file.name,
                &file.mime,
                &file.file.ciphertext_sha256,
                &file.file.binding,
            ] {
                bytes = bytes.saturating_add(text_charge(value));
            }
        }
        if let Some(MessageAction::Reaction {
            message_id,
            target_client_id,
            emoji,
            ..
        }) = &envelope.action
        {
            bytes = bytes
                .saturating_add(text_charge(message_id))
                .saturating_add(optional_charge(target_client_id))
                .saturating_add(text_charge(emoji));
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_reservation_is_atomic_bounded_and_rejects_overflow() {
        let budget = Arc::new(AtomicUsize::new(0));
        let jobs: Vec<_> = (0..32)
            .map(|_| {
                let budget = budget.clone();
                std::thread::spawn(move || reserve(&budget, 10, 70))
            })
            .collect();
        assert_eq!(
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .filter(|passed| *passed)
                .count(),
            7
        );
        assert_eq!(budget.load(Ordering::Relaxed), 70);
        assert!(!reserve(&budget, usize::MAX, usize::MAX));
    }
    #[test]
    fn actor_entries_are_isolated_and_release_budget_after_last_reader() {
        let first = Cache::default();
        let second = Cache::default();
        let retained = first.insert("one".into(), HashMap::new());
        assert!(Arc::ptr_eq(&retained, &first.get("one").unwrap()));
        assert!(second.get("one").is_none());
        assert!(retained._lease.is_some());
        drop(first);
        assert_eq!(Arc::strong_count(&retained), 1);
    }
    #[test]
    fn over_budget_segments_return_complete_records_without_retention() {
        let cache = Cache::default();
        let entries = HashMap::from([(
            "fixture".into(),
            CachedEnvelope {
                author: Identity {
                    server_id: "s".into(),
                    user_id: "u".into(),
                    device_id: "d".into(),
                },
                envelope: Envelope {
                    version: 1,
                    scope: "scope".into(),
                    client_id: "client".into(),
                    content: "complete".into(),
                    reply_to_id: None,
                    mentions: vec![],
                    attachments: vec![],
                    action: None,
                },
                message_id: Some("m".into()),
                sequence: 1,
                delivery_revision: 1,
            },
        )]);
        let records = cache.insert_charged("oversized".into(), entries, ACTOR_BYTES + 1);
        assert_eq!(records["fixture"].envelope.content, "complete");
        assert!(records._lease.is_none());
        assert!(cache.get("oversized").is_none());
    }
    #[test]
    fn segment_eviction_keeps_live_reader_and_does_not_leak_old_actor_entries() {
        let cache = Cache::default();
        let held = cache.insert("first".into(), HashMap::new());
        for n in 0..MAX_SEGMENTS {
            cache.insert(format!("key-{n}"), HashMap::new());
        }
        assert!(cache.get("first").is_none());
        assert!(held._lease.is_some());
        assert_eq!(Arc::strong_count(&held), 1);
        assert_eq!(cache.0.lock().unwrap().entries.len(), MAX_SEGMENTS);
    }
}
