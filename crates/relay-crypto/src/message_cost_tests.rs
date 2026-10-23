//! R4.5, text-message part: what Relay's actual MLS message encryption costs
//! on this PC, per message, in one process. Opt-in only:
//!
//! `cargo test -p relay-crypto --release -- --ignored --exact message_cost_tests::message_crypto_cost --test-threads=1 --nocapture`
//!
//! Two fresh in-memory devices compare and approve each other's fingerprints
//! and form a real MLS group through the production API. That setup is not
//! timed. The timed steady state then calls the production `Device::encrypt`
//! and `Device::receive` once per message, with a new message generation
//! every time and the normal replay protection left on.
//!
//! The payloads are synthetic bytes handed directly to `Device::encrypt`: raw
//! MLS application payload bytes, not characters a person types and not the
//! client's JSON envelope. Not included: the client's envelope encoding, base64,
//! `Device::transact` (checkpoint, restore and durable persistence), history,
//! the server and the network. Nothing is written to disk: the result is one
//! JSON line on stdout. There is no speed limit; timings depend on the PC.
use super::*;
use std::time::Instant;

const WARMUP: usize = 200;
const SAMPLES: usize = 2_000;
const RAW_KEPT: usize = 200;
const PAYLOADS: [usize; 3] = [32, 256, 4_096];
const SCOPE: &str = "channel:message-cost";

fn device(name: &str) -> Device {
    Device::new(Identity {
        server_id: "synthetic-community".into(),
        user_id: name.into(),
        device_id: format!("{name}-cost-device"),
    })
    .unwrap()
}

/// Setup through `transact` with an in-memory no-op persist, as the existing
/// fixtures do. Fingerprints are compared and approved; nothing is bypassed.
fn durable<T>(device: &mut Device, op: impl FnOnce(&mut Device) -> Result<T>) -> T {
    device.transact(op, |_| Ok(())).unwrap()
}

fn group() -> (Device, Device) {
    let mut alice = device("alice");
    let mut bob = device("bob");
    let a = durable(&mut alice, |d| d.key_package());
    let b = durable(&mut bob, |d| d.key_package());
    durable(&mut alice, |d| d.approve(&b, &b.fingerprint));
    durable(&mut bob, |d| d.approve(&a, &a.fingerprint));
    durable(&mut alice, |d| d.create_group(SCOPE, "message-cost-group"));
    let change = durable(&mut alice, |d| {
        d.add_members(SCOPE, std::slice::from_ref(&b))
    });
    durable(&mut bob, |d| {
        d.join(
            SCOPE,
            "message-cost-group",
            a.identity.clone(),
            change.welcome.as_ref().unwrap(),
        )
    });
    (alice, bob)
}

/// Deterministic synthetic bytes. The pattern repeats every 256 messages
/// (the byte value wraps), so not every plaintext in a series is unique;
/// each message is still encrypted as a new MLS generation.
fn payload(size: usize, message: usize) -> Vec<u8> {
    (0..size)
        .map(|n| (n as u64 * 31 + message as u64 * 7 + size as u64) as u8)
        .collect()
}

fn summary(operation: &str, size: usize, wire: (usize, usize), nanos: &[u64]) -> serde_json::Value {
    let mut sorted = nanos.to_vec();
    sorted.sort_unstable();
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    let mean = sorted.iter().sum::<u64>() as f64 / sorted.len() as f64;
    let variance = sorted
        .iter()
        .map(|n| (*n as f64 - mean).powi(2))
        .sum::<f64>()
        / sorted.len() as f64;
    serde_json::json!({
        "operation": operation,
        "payload_bytes": size,
        "payload_kind": "raw MLS application payload passed to Device::encrypt",
        "ciphertext_bytes_min": wire.0,
        "ciphertext_bytes_max": wire.1,
        "warmup": WARMUP,
        "samples": sorted.len(),
        "unit": "ns per single message",
        "min": sorted[0],
        "median": at(0.5),
        "p95": at(0.95),
        "p99": at(0.99),
        "max": sorted[sorted.len() - 1],
        "mean": mean.round(),
        "stddev": variance.sqrt().round(),
        // The first measured samples, in order, after warm-up.
        "raw_first": &nanos[..RAW_KEPT.min(nanos.len())],
    })
}

/// One sender, one receiver, one message at a time: send is timed, then
/// receive is timed; the plaintext is compared outside both timers.
fn measure(size: usize) -> [serde_json::Value; 2] {
    let (mut alice, mut bob) = group();
    let author = alice.identity().clone();
    let mut send = Vec::with_capacity(SAMPLES);
    let mut receive = Vec::with_capacity(SAMPLES);
    let mut wire = (usize::MAX, 0);
    let mut last = Vec::new();
    for message in 0..WARMUP + SAMPLES {
        let plaintext = payload(size, message);
        let started = Instant::now();
        let ciphertext = alice.encrypt(SCOPE, &plaintext);
        let sent = started.elapsed().as_nanos() as u64;
        let ciphertext = ciphertext.expect("encrypt accepts the payload");
        let started = Instant::now();
        let received = bob.receive(SCOPE, &ciphertext);
        let opened = started.elapsed().as_nanos() as u64;
        let Ok(Received::Application {
            author: from,
            bytes,
        }) = received
        else {
            panic!("a fresh message is accepted as an application message");
        };
        assert_eq!(from, author, "authenticated author");
        assert_eq!(bytes, plaintext, "decrypted payload is byte-identical");
        assert!(
            !ciphertext
                .windows(size.min(16))
                .any(|w| w == &plaintext[..size.min(16)]),
            "plaintext does not appear in the ciphertext"
        );
        wire = (wire.0.min(ciphertext.len()), wire.1.max(ciphertext.len()));
        if message >= WARMUP {
            send.push(sent);
            receive.push(opened);
        }
        last = ciphertext;
    }
    // The normal replay protection still rejects a repeated message.
    assert!(
        bob.receive(SCOPE, &last).is_err(),
        "a repeated message is rejected after the series"
    );
    [
        summary("send (Device::encrypt)", size, wire, &send),
        summary("receive (Device::receive)", size, wire, &receive),
    ]
}

/// What the timer itself can show, kept visible rather than subtracted.
fn timer() -> serde_json::Value {
    let mut empty: Vec<u64> = (0..SAMPLES)
        .map(|_| Instant::now().elapsed().as_nanos() as u64)
        .collect();
    empty.sort_unstable();
    let mut step = u64::MAX;
    for _ in 0..1_000 {
        let start = Instant::now();
        let mut now = Instant::now();
        while now == start {
            now = Instant::now();
        }
        step = step.min((now - start).as_nanos() as u64);
    }
    serde_json::json!({
        "clock": "std::time::Instant",
        "smallest_observed_step_ns": step,
        "empty_timing_median_ns": empty[empty.len() / 2],
        "empty_timing_p95_ns": empty[(empty.len() - 1) * 95 / 100],
        "note": "not subtracted from the series",
    })
}

#[test]
#[ignore = "opt-in local measurement; run with --ignored"]
fn message_crypto_cost() {
    let timer = timer();
    let mut series = Vec::new();
    for size in PAYLOADS {
        for result in measure(size) {
            println!(
                "{:<28} {:>5} B -> {:>5} B: median {:>7} ns, p95 {:>7} ns",
                result["operation"].as_str().unwrap(),
                size,
                result["ciphertext_bytes_max"],
                result["median"],
                result["p95"]
            );
            series.push(result);
        }
    }
    let report = serde_json::json!({
        "measurement": "relay-crypto message_crypto_cost",
        "setup": "two fresh in-memory devices, approved fingerprints, one real MLS group per payload size; not timed",
        "timer": timer,
        "series": series,
    });
    println!("RELAY_MESSAGE_CRYPTO_COST {report}");
}
