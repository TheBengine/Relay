//! R4.5, measurement part: what the production voice encryption costs on this
//! PC, per packet, without sockets, audio devices or a GUI. Opt-in only:
//!
//! `cargo test -p relay-client --release -- --ignored --test-threads=1 voice_crypto_cost`
//!
//! It calls the real `CallCipher::seal`/`open` (inner voice E2EE, including the
//! normal replay check) and, separately, `seal_packet`/`open_packet` (the outer
//! UDP transport protection between client and server, which is not extra
//! E2EE). Keys, identities and payloads are artificial; no key or packet bytes
//! are written. Results go to stdout and, when `RELAY_CRYPTO_COST_OUT` names a
//! file, to that JSON file. There is no speed limit: timings depend on the PC.
use super::*;
use std::time::Instant;

const WARMUP: usize = 2_000;
const SAMPLES: usize = 20_000;
const RAW_KEPT: usize = 200;
/// 32 and 64 kbit/s Opus at 20 ms, Relay's largest measured Opus frame
/// (521 bytes, iroh voice lab), and close to the largest payload `seal` allows.
const PAYLOADS: [usize; 4] = [80, 160, 521, 1_200];

fn bench_keys(local: &str) -> Arc<CallKeys> {
    let sender_keys = [("bench-a", 0x5a_u8), ("bench-b", 0xa5_u8)]
        .into_iter()
        .map(|(user, byte)| {
            (
                user.to_owned(),
                crate::e2ee::SenderKeys {
                    device_id: format!("device-{user}"),
                    opus: zeroize::Zeroizing::new([byte; 32]),
                    rtc: zeroize::Zeroizing::new([byte ^ 0xff; 32]),
                },
            )
        })
        .collect();
    Arc::new(CallKeys {
        group_id: "bench-group".into(),
        scope: "bench-room".into(),
        epoch: 1,
        local_user_id: local.into(),
        local_device_id: format!("device-{local}"),
        sender_keys,
        voice_replay: Mutex::default(),
    })
}

fn payload(size: usize, salt: u64) -> Vec<u8> {
    (0..size)
        .map(|n| (n as u64).wrapping_mul(31).wrapping_add(salt) as u8)
        .collect()
}

struct Series {
    name: String,
    payload: usize,
    wire: usize,
    nanos: Vec<u64>,
}

impl Series {
    fn summary(&self) -> serde_json::Value {
        let mut sorted = self.nanos.clone();
        sorted.sort_unstable();
        let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
        let mean = sorted.iter().sum::<u64>() as f64 / sorted.len() as f64;
        let variance = sorted
            .iter()
            .map(|n| (*n as f64 - mean).powi(2))
            .sum::<f64>()
            / sorted.len() as f64;
        serde_json::json!({
            "operation": self.name,
            "payload_bytes": self.payload,
            "output_bytes": self.wire,
            "samples": sorted.len(),
            "unit": "ns per single packet",
            "min": sorted[0],
            "median": at(0.5),
            "p95": at(0.95),
            "p99": at(0.99),
            "max": sorted[sorted.len() - 1],
            "mean": mean.round(),
            "stddev": variance.sqrt().round(),
            // The first measured samples, in order, after warm-up.
            "raw_first": &self.nanos[..RAW_KEPT.min(self.nanos.len())],
        })
    }
}

/// Times one call at a time; every packet is checked for a byte-exact result.
fn measure_inner(size: usize) -> (Series, Series) {
    let sender = CallCipher::new(bench_keys("bench-a")).unwrap();
    let mut receiver = CallCipher::new(bench_keys("bench-b")).unwrap();
    let total = WARMUP + SAMPLES;
    let opus = payload(size, size as u64);
    let mut seal = Vec::with_capacity(SAMPLES);
    let mut sealed = Vec::with_capacity(total);
    for sequence in 1..=total as u64 {
        let started = Instant::now();
        let packet = sender.seal(9, sequence, sequence * 960, sequence == 1, &opus);
        let elapsed = started.elapsed().as_nanos() as u64;
        let packet = packet.expect("seal accepts the payload");
        if sequence as usize > WARMUP {
            seal.push(elapsed);
        }
        sealed.push(packet);
    }
    let wire = sealed[0].len();
    let mut open = Vec::with_capacity(SAMPLES);
    for (index, packet) in sealed.iter().enumerate() {
        let started = Instant::now();
        let frame = receiver.open(packet);
        let elapsed = started.elapsed().as_nanos() as u64;
        let frame = frame.expect("a fresh sequence passes the normal replay check");
        assert_eq!(frame.opus, opus, "decrypted payload is byte-identical");
        assert_eq!(frame.sequence, index as u64 + 1);
        if index >= WARMUP {
            open.push(elapsed);
        }
    }
    // The normal replay check still rejects a repeated packet.
    assert!(receiver.open(&sealed[total - 1]).is_none());
    (
        Series {
            name: "inner E2EE seal (CallCipher::seal)".into(),
            payload: size,
            wire,
            nanos: seal,
        },
        Series {
            name: "inner E2EE open + replay check (CallCipher::open)".into(),
            payload: size,
            wire,
            nanos: open,
        },
    )
}

/// The outer client-server UDP protection, over the already sealed packet.
fn measure_outer(inner: &[u8]) -> (Series, Series) {
    let cipher = ChaCha20Poly1305::new_from_slice(&[0x3c; 32]).unwrap();
    let total = WARMUP + SAMPLES;
    let mut seal = Vec::with_capacity(SAMPLES);
    let mut packets = Vec::with_capacity(total);
    for sequence in 1..=total as u64 {
        let started = Instant::now();
        let packet = seal_packet(&cipher, 77, sequence, 0, inner);
        let elapsed = started.elapsed().as_nanos() as u64;
        if sequence as usize > WARMUP {
            seal.push(elapsed);
        }
        packets.push(packet.expect("fits the UDP limit"));
    }
    let wire = packets[0].len();
    let mut open = Vec::with_capacity(SAMPLES);
    for (index, packet) in packets.iter().enumerate() {
        let started = Instant::now();
        let opened = open_packet(&cipher, 77, 0, packet);
        let elapsed = started.elapsed().as_nanos() as u64;
        let (sequence, plain) = opened.expect("opens");
        assert_eq!(sequence, index as u64 + 1);
        assert_eq!(plain, inner, "byte-identical");
        if index >= WARMUP {
            open.push(elapsed);
        }
    }
    (
        Series {
            name: "outer UDP transport seal (seal_packet)".into(),
            payload: inner.len(),
            wire,
            nanos: seal,
        },
        Series {
            name: "outer UDP transport open (open_packet)".into(),
            payload: inner.len(),
            wire,
            nanos: open,
        },
    )
}

#[test]
#[ignore = "opt-in local measurement; timings depend on the PC"]
fn voice_crypto_cost() {
    // Per-call setup, kept apart from the per-packet cost. MLS key agreement
    // happens before this and is not measured here.
    let mut setup = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let keys = bench_keys("bench-a");
        let started = Instant::now();
        let cipher = CallCipher::new(keys);
        setup.push(started.elapsed().as_nanos() as u64);
        assert!(cipher.is_ok());
    }
    let mut series = vec![Series {
        name: "per-call setup (CallCipher::new: key checks + random nonce prefix)".into(),
        payload: 0,
        wire: 0,
        nanos: setup,
    }];
    for size in PAYLOADS {
        let (seal, open) = measure_inner(size);
        let inner = CallCipher::new(bench_keys("bench-a"))
            .unwrap()
            .seal(9, 1, 0, true, &payload(size, 1))
            .unwrap();
        assert!(inner.len() + UDP_HEADER_LEN + 16 <= UDP_MAX_PACKET);
        let (outer_seal, outer_open) = measure_outer(&inner);
        series.extend([seal, open, outer_seal, outer_open]);
    }
    let report = serde_json::json!({
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "warmup_per_series": WARMUP,
        "samples_per_series": SAMPLES,
        "timer": "std::time::Instant around one call; includes the call's own allocation",
        "boundary": "CPU work of one packet on one thread; no sockets, codec, devices, network or playout",
        "series": series.iter().map(Series::summary).collect::<Vec<_>>(),
    });
    for item in report["series"].as_array().unwrap() {
        println!(
            "{:<70} {:>5} B  median {:>6} ns  p95 {:>6} ns  p99 {:>6} ns",
            item["operation"].as_str().unwrap(),
            item["payload_bytes"],
            item["median"],
            item["p95"],
            item["p99"],
        );
    }
    if let Some(path) = std::env::var_os("RELAY_CRYPTO_COST_OUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
