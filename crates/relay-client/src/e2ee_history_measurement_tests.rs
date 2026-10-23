//! Opt-in archive component measurements. Bulk records are synthetic fixture
//! data; real MLS messages bracket archival/reopen. No UI or server benchmark.
use super::*;
use serde_json::{json, Value};
use std::{fs, time::Instant};

fn memory() -> Value {
    use windows::Win32::System::{
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
        Threading::GetCurrentProcess,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: current-process pseudo-handle and one correctly sized writable
    // struct. No other application or process is inspected.
    assert!(unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        )
    }
    .as_bool());
    json!({"working_set_bytes": counters.WorkingSetSize,
        "private_bytes": counters.PrivateUsage,
        "peak_working_set_bytes_since_process_start": counters.PeakWorkingSetSize})
}

fn usage(pair: &Pair) -> Value {
    let bob = pair.bob.as_ref().unwrap();
    let usage = bob.history_usage();
    json!({"records": usage.records, "segments": usage.segments,
        "archive_bytes": usage.archived_bytes,
        "checkpoint_bytes": fs::metadata(&bob.path).unwrap().len(),
        "memory": memory()})
}

fn measure(count: usize) {
    if cfg!(debug_assertions) {
        panic!("Run archive measurements with --release");
    }
    let owned_root = PathBuf::from(
        std::env::var_os("RELAY_TEST_WORK_ROOT")
            .expect("Explicitly owned RELAY_TEST_WORK_ROOT is required"),
    );
    let start = Instant::now();
    let initial_memory = memory();
    let mut pair = Pair::new();
    assert!(pair.root.starts_with(&owned_root));
    let identity = pair.bob.as_ref().unwrap().device.identity().clone();
    let first = pair.envelope(
        "real-before-archive",
        "Actual MLS before archive measurement",
    );
    let wire = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(first)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(
        bob.decrypt_content(&pair.scope, &wire)
            .unwrap()
            .envelope
            .content,
        "Actual MLS before archive measurement"
    );
    bob.bind_delivery(&wire, "real-before-archive", 1).unwrap();
    let actor_setup_us = start.elapsed().as_micros();
    let bulk = Instant::now();
    // Reuse the existing explicitly synthetic protected-history regression
    // fixture. This does not authenticate 64,000 messages via MLS or HTTP.
    fill_history(pair.bob.as_mut().unwrap(), &pair.scope, count);
    let bulk_store_us = bulk.elapsed().as_micros();
    let stored = usage(&pair);
    let bob = pair.bob.as_ref().unwrap();
    assert_eq!(bob.history_usage().records, count + 1);
    if count > history::ACTIVE_LIMIT {
        assert!(!bob.state.cached.contains_key(&cipher_hash(&wire)));
        assert!(bob.history_usage().segments > 0);
    }
    let reopen = Instant::now();
    pair.reopen_bob();
    let reopen_us = reopen.elapsed().as_micros();
    let reopened = usage(&pair);
    assert_eq!(pair.bob.as_ref().unwrap().device.identity(), &identity);
    assert_eq!(
        pair.bob
            .as_mut()
            .unwrap()
            .decrypt_content(&pair.scope, &wire)
            .unwrap()
            .envelope
            .content,
        "Actual MLS before archive measurement"
    );
    assert!(pair
        .bob
        .as_mut()
        .unwrap()
        .bind_delivery(&wire, "wrong-binding", 999999)
        .is_err());
    let mut scans = Vec::new();
    let mut filters = Vec::new();
    let mut lookups = Vec::new();
    let expected_matches = (0..count)
        .filter(|n| format!("Synthetic protected history text {n}").contains("history text 123"))
        .count();
    for _ in 0..5 {
        let now = Instant::now();
        let bob = pair.bob.as_ref().unwrap();
        let entries = bob
            .cached_envelopes(&pair.scope)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), count + 1);
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.message_id.as_deref() == Some("real-before-archive"))
                .count(),
            1
        );
        scans.push(now.elapsed().as_micros());
        drop(entries);
        let now = Instant::now();
        let hits = bob
            .cached_envelopes(&pair.scope)
            .map(|e| e.unwrap())
            .filter(|e| e.envelope.content.contains("history text 123"))
            .count();
        assert_eq!(hits, expected_matches);
        filters.push(now.elapsed().as_micros());
        assert_eq!(bob.cached_envelopes("absent-fixture-scope").count(), 0);
        for index in [0, count / 2, count - 1] {
            let now = Instant::now();
            let cached = pair
                .bob
                .as_mut()
                .unwrap()
                .decrypt_content(
                    &pair.scope,
                    &format!("synthetic-authenticated-history-{index}"),
                )
                .unwrap();
            assert_eq!(
                cached.envelope.content,
                format!("Synthetic protected history text {index}")
            );
            lookups.push(json!({"record_index": index, "us": now.elapsed().as_micros()}));
        }
        let bob = pair.bob.as_ref().unwrap();
        let now = Instant::now();
        assert!(history::lookup(
            &bob.state,
            &bob.path,
            &bob.binding(),
            &cipher_hash("absent-owned-history-record")
        )
        .unwrap()
        .is_none());
        lookups.push(json!({"missing": true, "us": now.elapsed().as_micros()}));
    }
    let after_queries = usage(&pair);
    let outgoing = pair.envelope(
        "real-after-reopen",
        "Actual MLS still works after long archive reopen",
    );
    let next = pair
        .alice
        .as_mut()
        .unwrap()
        .encrypted_content(outgoing)
        .unwrap();
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(
        bob.decrypt_content(&pair.scope, &next)
            .unwrap()
            .envelope
            .content,
        "Actual MLS still works after long archive reopen"
    );
    bob.bind_delivery(&next, "real-after-reopen", count as u64 + 2)
        .unwrap();
    assert_eq!(bob.history_usage().records, count + 2);
    let mut archive_damage_checked = false;
    if let Some(path) = history::archive_paths(&bob.state, &bob.path).first() {
        let bytes = fs::read(path).unwrap();
        let mut damaged = bytes.clone();
        damaged[0] ^= 1;
        fs::write(path, damaged).unwrap();
        assert!(bob
            .cached_envelopes(&pair.scope)
            .collect::<Result<Vec<_>, _>>()
            .is_err());
        fs::write(path, bytes).unwrap();
        let saved = fs::read(path).unwrap();
        fs::remove_file(path).unwrap();
        assert!(bob
            .cached_envelopes(&pair.scope)
            .collect::<Result<Vec<_>, _>>()
            .is_err());
        fs::write(path, saved).unwrap();
        archive_damage_checked = true;
    }
    pair.reopen_bob();
    let bob = pair.bob.as_mut().unwrap();
    assert_eq!(bob.device.identity(), &identity);
    assert_eq!(
        bob.decrypt_content(&pair.scope, &next)
            .unwrap()
            .envelope
            .content,
        "Actual MLS still works after long archive reopen"
    );
    let report = json!({"synthetic_bulk_records": count, "actual_MLS_messages": 2,
        "process_id": std::process::id(), "actor_setup_us": actor_setup_us,
        "bulk_store_us": bulk_store_us, "reopen_us": reopen_us,
        "full_scan_us": scans, "iterator_text_filter_us": filters,
        "lookup_samples": lookups, "expected_text_matches": expected_matches,
        "initial_memory": initial_memory, "stored": stored, "reopened": reopened,
        "after_queries": after_queries, "final": usage(&pair),
        "archive_damage_and_missing_checked": archive_damage_checked,
        "real_MLS_continuity": true, "wrong_delivery_binding_rejected": true,
        "total_us": start.elapsed().as_micros(), "fixture": pair.root,
        "scope": "Archive component only: synthetic delivered cache records in real production Windows-protected checkpoints/segments. Two real MLS messages and actual actor reopen. Five full scans/text filters, individual lookups and named current-process RAM samples. Not full worker search/reactions/unread/files, network catch-up, whole-app responsiveness, cleanup, UI, hardware or WAN acceptance.",
        "private_keys_exported": false, "physical_devices_opened": false});
    let path = pair.root.join("history-measurement.json");
    fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("History measurement: {}", path.display());
}

#[test]
#[ignore = "Opt-in optimized Windows archive measurement; owned fixture required"]
fn protected_archive_1000_records() {
    measure(1000);
}

#[test]
#[ignore = "Opt-in optimized Windows archive measurement; owned fixture required"]
fn protected_archive_16000_records() {
    measure(16000);
}

#[test]
#[ignore = "Opt-in optimized Windows archive measurement; owned fixture required"]
fn protected_archive_64000_records() {
    measure(64000);
}
