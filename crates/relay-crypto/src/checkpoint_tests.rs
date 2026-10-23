//! Same-state schema compatibility and optimized serialization measurements.
use super::*;
use std::{hint::black_box, time::Instant};

// Original accepted capacity serializer, retained only in the test binary.
impl Device {
    fn legacy_checkpoint(&self) -> Result<Zeroizing<Vec<u8>>> {
        let values = self
            .provider
            .storage()
            .values
            .read()
            .map_err(|e| error("read device state", e))?;
        let signer = serde_json::from_slice(
            &serde_json::to_vec(&self.signer).map_err(|e| error("serialize signing key", e))?,
        )
        .map_err(|e| error("snapshot signing key", e))?;
        let snapshot = Snapshot {
            // Older clients must reject a pending snapshot rather than ignore
            // its key-release barrier. Completed snapshots retain version 1.
            version: if self.pending_rotations.is_empty() {
                1
            } else if self
                .pending_rotations
                .values()
                .any(|r| !r.proofs.is_empty())
            {
                3
            } else {
                2
            },
            identity: self.identity.clone(),
            signer,
            values: values.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            approved: self.approved.clone(),
            groups: self.groups.clone(),
            pending_rotations: self.pending_rotations.clone(),
        };
        let bytes = Zeroizing::new(
            serde_json::to_vec(&snapshot).map_err(|e| error("serialize device state", e))?,
        );
        if bytes.len() > MAX_STATE_BYTES {
            return Err("Encryption device storage is full.".into());
        }
        Ok(bytes)
    }
}

fn actor(name: &str) -> Device {
    Device::new(Identity {
        server_id: "checkpoint-fixture-community".into(),
        user_id: name.into(),
        device_id: format!("{name}-checkpoint-device"),
    })
    .unwrap()
}

fn durable<T>(device: &mut Device, op: impl FnOnce(&mut Device) -> Result<T>) -> T {
    device.transact(op, |_| Ok(())).unwrap()
}

fn pair() -> (Device, Device, PublicDevice, PublicDevice) {
    let mut owner = actor("owner");
    let mut peer = actor("peer");
    let own = durable(&mut owner, Device::key_package);
    let public = durable(&mut peer, Device::key_package);
    durable(&mut owner, |d| d.approve(&public, &public.fingerprint));
    durable(&mut peer, |d| d.approve(&own, &own.fingerprint));
    durable(&mut owner, |d| {
        d.create_group("channel:checkpoint", "checkpoint-group")
    });
    let membership = durable(&mut owner, |d| {
        d.add_members("channel:checkpoint", std::slice::from_ref(&public))
    });
    durable(&mut peer, |d| {
        d.join(
            "channel:checkpoint",
            "checkpoint-group",
            own.identity.clone(),
            membership.welcome.as_ref().unwrap(),
        )
    });
    (owner, peer, own, public)
}

fn identical(device: &Device, version: u32) -> Zeroizing<Vec<u8>> {
    let old = device.legacy_checkpoint().unwrap();
    let new = device.checkpoint().unwrap();
    // Never print secret snapshot bytes on assertion failure.
    assert!(
        old.as_slice() == new.as_slice(),
        "checkpoint schema bytes changed"
    );
    let parsed: Snapshot = serde_json::from_slice(&new).unwrap();
    assert_eq!(parsed.version, version);
    assert_eq!(parsed.groups.len(), device.groups.len());
    new
}

#[test]
fn borrowed_snapshot_preserves_legacy_bytes_versions_and_reopen() {
    for version in 1..=3 {
        let (mut owner, mut peer, own, public) = pair();
        let pending = match version {
            1 => None,
            2 => Some(durable(&mut owner, |d| {
                d.stage_key_rotation("channel:checkpoint")
            })),
            _ => Some(durable(&mut owner, |d| {
                d.stage_key_rotation_with_proofs(
                    "channel:checkpoint",
                    &[CallFreshnessProof {
                        identity: public.identity.clone(),
                        nonce: [91; 32],
                    }],
                )
            })),
        };
        let saved = identical(&owner, version);
        let legacy = owner.legacy_checkpoint().unwrap();
        owner = Device::restore(&saved, &own.identity).unwrap();
        let old_reader = Device::restore(&legacy, &own.identity).unwrap();
        assert_eq!(
            old_reader.fingerprint().unwrap(),
            owner.fingerprint().unwrap()
        );
        if let Some(change) = pending {
            assert!(owner
                .media_key("channel:checkpoint", "opus", &own.identity)
                .is_err());
            assert_eq!(
                owner
                    .pending_key_rotation("channel:checkpoint")
                    .unwrap()
                    .unwrap()
                    .commit,
                change.commit
            );
            if version == 3 {
                assert_eq!(
                    owner.pending_call_proofs("channel:checkpoint").unwrap(),
                    vec![CallFreshnessProof {
                        identity: public.identity.clone(),
                        nonce: [91; 32]
                    }]
                );
            }
            durable(&mut owner, |d| {
                d.confirm_key_rotation("channel:checkpoint", &change.commit)
            });
            durable(&mut peer, |d| {
                d.receive("channel:checkpoint", &change.commit)
            });
            identical(&owner, 1);
        }
        let message = durable(&mut peer, |d| {
            d.encrypt("channel:checkpoint", b"schema-compatible synthetic message")
        });
        assert!(
            matches!(durable(&mut owner, |d| d.receive("channel:checkpoint", &message)), Received::Application { bytes, .. } if bytes == b"schema-compatible synthetic message")
        );
        assert!(owner
            .transact(|d| d.receive("channel:checkpoint", &message), |_| Ok(()))
            .is_err());
    }
}

#[test]
fn borrowed_snapshot_size_failure_blocks_output_and_keeps_last_durable_state() {
    let mut owner = actor("limit");
    durable(&mut owner, |d| {
        d.create_group("channel:limit", "limit-group")
    });
    let before = identical(&owner, 1);
    let called = std::cell::Cell::new(false);
    let result = owner.transact(
        |next| {
            next.provider.storage().values.write().unwrap().insert(
                b"owned oversized fixture".to_vec(),
                vec![0; MAX_STATE_BYTES / 2],
            );
            let Err(error) = next.legacy_checkpoint() else {
                panic!("legacy serializer accepted the oversized fixture");
            };
            assert!(error.contains("storage is full"));
            Ok("must not be released")
        },
        |_| {
            called.set(true);
            Ok(())
        },
    );
    assert!(result.unwrap_err().contains("storage is full"));
    assert!(!called.get());
    assert!(owner.is_blocked());
    assert!(owner.checkpoint().unwrap().as_slice() == before.as_slice());
    assert!(owner
        .transact(|d| d.encrypt("channel:limit", b"not released"), |_| Ok(()))
        .is_err());
    let mut reopened = Device::restore(&before, owner.identity()).unwrap();
    assert!(reopened
        .transact(
            |d| d.encrypt("channel:limit", b"valid durable state"),
            |_| Ok(())
        )
        .is_ok());
}

fn statistics(values: &[f64]) -> serde_json::Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let count = sorted.len();
    serde_json::json!({
        "iterations": count,
        "median_ms": (sorted[count / 2 - 1] + sorted[count / 2]) / 2.0,
        "p25_ms": sorted[count / 4], "p75_ms": sorted[count * 3 / 4],
        "min_ms": sorted[0], "max_ms": sorted[count - 1], "samples_ms": values,
    })
}

#[test]
#[ignore = "Optimized serializer measurement; actual one-member MLS groups, no devices/network/DPAPI"]
fn actual_checkpoint_serialization_comparison() {
    if cfg!(debug_assertions) {
        panic!("Run the checkpoint measurement with --release");
    }
    let path = std::path::PathBuf::from(
        std::env::var_os("RELAY_CHECKPOINT_REPORT")
            .expect("An explicit new report path is required"),
    );
    let mut device = actor("measurement");
    let mut rows = Vec::new();
    for groups in [1, 16, 32, 64, 128] {
        while device.groups.len() < groups {
            let index = device.groups.len();
            device
                .create_group(
                    &format!("channel:measurement-{index}"),
                    &format!("measurement-group-{index}"),
                )
                .unwrap();
        }
        let saved = identical(&device, 1);
        assert_eq!(
            Device::restore(&saved, device.identity())
                .unwrap()
                .scopes()
                .len(),
            groups
        );
        for _ in 0..3 {
            black_box(device.legacy_checkpoint().unwrap());
            black_box(device.checkpoint().unwrap());
        }
        let mut old_times = Vec::new();
        let mut new_times = Vec::new();
        for iteration in 0..24 {
            let mut timed = |old: bool| {
                let start = Instant::now();
                let bytes = if old {
                    device.legacy_checkpoint().unwrap()
                } else {
                    device.checkpoint().unwrap()
                };
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                assert!(
                    bytes.as_slice() == saved.as_slice(),
                    "measurement changed checkpoint bytes"
                );
                black_box(&bytes);
                if old {
                    old_times.push(elapsed);
                } else {
                    new_times.push(elapsed);
                }
            };
            timed(iteration % 2 == 0);
            timed(iteration % 2 != 0);
        }
        let values = device.provider.storage().values.read().unwrap();
        rows.push(serde_json::json!({"actual_groups": groups, "members_per_group": 1,
            "storage_entries": values.len(), "storage_key_bytes": values.keys().map(Vec::len).sum::<usize>(),
            "storage_value_bytes": values.values().map(Vec::len).sum::<usize>(),
            "checkpoint_bytes": saved.len(), "bytes_identical": true, "restore_verified": true,
            "legacy_owned": statistics(&old_times), "borrowed": statistics(&new_times)}));
    }
    let report = serde_json::json!({"cases": rows,
        "scope": "Alternating same-actor optimized checkpoint serialization only. Actual one-member MLS groups. No production protected writes, applications/catch-up, native UI, network, physical devices, RAM benchmark or whole-app/idle performance acceptance.",
        "order": "Three warm-ups per method; 24 pairs per group count, alternating first method each pair.",
        "private_keys_exported": false, "memory_measured": false,
        "legacy_serializer": "Exact original accepted capacity checkpoint body retained in a cfg(test) impl; source checked by verification runner."});
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    serde_json::to_writer_pretty(file, &report).unwrap();
    println!("Checkpoint comparison: five same-actor group counts; report saved without private key material.");
}
