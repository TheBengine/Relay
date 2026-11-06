# Packet copies for bursty loss — 6 November 2026

Round 3, Task M. A voice packet can now carry an exact copy of the frame sent just before it. The receiver plays that copy when the earlier packet never arrives. Copies switch on by themselves when the losses you receive come in pairs or runs, and off after a clean minute. They are only sent once everyone else in the call has said their Relay understands them. Older Relay, and the relay server, see packets they already handle.

**The headline target is not met.** On `wifi_busy` with 10 ms packets, concealed frames drop from 66 to 61 (8 % fewer), not the 40 % the plan asks for (39 or fewer). The measurements below show why: 30 of the 66 are late packets in the first second, while the receive buffer grows, and the first frame of each lost pair can only be rebuilt from a packet that arrives after its deadline. Both are receive-buffer depth, not redundancy. A fixed 30 ms buffer alone gives 39. What copies do deliver is better sound under loss: speech distortion (LSD) drops from 2.01 to 1.64 dB at 64 kbit/s, because the codec no longer needs FEC's mode.

| Target (plan, Task M step 4) | Result |
| --- | --- |
| `wifi_busy` concealed at least 40 % below shipped (66 → 39 or fewer, 10 ms packets) | **Fail.** 66 → 61 at 32, 48 and 64 kbit/s. With 20 ms packets 34 → 33. |
| `mobile_hotspot` not worse than today (18) | Pass. 18, identical in every figure: its losses are single, so copies never switch on and FEC keeps doing that job. |
| No extra delay | Pass. Same receive buffer, same first sound. Codec delay is 4 ms *shorter* while copies are on (2.5 ms instead of FEC's 6.5 ms). No extra send delay: the copy is bytes already encoded. |
| Bandwidth at most 2×, only while copies are on | Pass. 1.57–1.87× what today's code sends on the same link, only while copies are on. 1.5× the bitrate setting. |
| Old peers keep today's packets, with a test | Pass. Copies are not sent until every other person has announced support. Even a packet with copies decodes bit-for-bit like the plain one on an older Relay: `an_older_relay_decodes_padded_packets_exactly_like_plain_ones`. |

## How it was measured

This uses the simulation from `docs/network-profiles-20261028.md`: the real Opus encoder and decoder, Relay's real receive buffer (factory setting, Automatic), the same five networks and seeds. Each run is 10 s of the audio bench's synthetic talker (`voice_test_signals::talker`). The sender runs FEC Automatic and decides recovery and copies from the loss the receiver measures. That is the symmetric-path assumption the call makes. `network_profiles::run` in `crates/relay-client/src/voice_jitter.rs` now also takes the packet size, the bitrate, the copy plan and an optional fixed receive buffer.

Speech quality is the audio bench's `metrics()` on what was played, against the clean talker, aligned by the encoder's lookahead per packet:

- **LSD** is log-spectral distance; lower is better.
- **segSNR** is speech segmental SNR; higher is better.

segSNR is not comparable across codec modes. On a clean LAN, forcing FEC on (which puts the encoder in its Voip mode) moves LSD from 0.88 to 1.54 dB and segSNR from 28.4 to 5.2 dB with no loss at all. Voip mode keeps the sound, not the waveform, so read LSD, and segSNR only within one mode.

*Payload* is the Opus bytes sent per second, framing and copies included; IP/UDP and Relay's sealing are not counted.

## Step 1: today's code

FEC Automatic, no copies. The `wifi_busy` row at 10 ms is the plan's 66.

| profile | packet | kbit/s | lost | concealed | rebuilt by FEC | late | LSD (dB) | segSNR (dB) | payload kbit/s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `lan_wired` | 10 ms | 32 | 0 | 0 | 0 | 0 | 1.13 | 23.77 | 30.3 |
| `lan_wired` | 10 ms | 48 | 0 | 0 | 0 | 0 | 0.98 | 26.48 | 45.0 |
| `lan_wired` | 10 ms | 64 | 0 | 0 | 0 | 0 | 0.88 | 28.40 | 59.6 |
| `lan_wired` | 20 ms | 32 | 0 | 0 | 0 | 0 | 1.17 | 22.64 | 30.1 |
| `lan_wired` | 20 ms | 48 | 0 | 0 | 0 | 0 | 0.97 | 24.78 | 45.0 |
| `lan_wired` | 20 ms | 64 | 0 | 0 | 0 | 0 | 0.86 | 27.52 | 59.8 |
| `wifi_good` | 10 ms | 32 | 2 | 7 | 0 | 5 | 1.23 | 23.36 | 30.3 |
| `wifi_good` | 10 ms | 48 | 2 | 7 | 0 | 5 | 1.08 | 25.98 | 45.0 |
| `wifi_good` | 10 ms | 64 | 2 | 7 | 0 | 5 | 0.98 | 27.88 | 59.6 |
| `wifi_good` | 20 ms | 32 | 1 | 6 | 0 | 5 | 1.43 | 21.52 | 30.1 |
| `wifi_good` | 20 ms | 48 | 1 | 6 | 0 | 5 | 1.23 | 23.44 | 45.0 |
| `wifi_good` | 20 ms | 64 | 1 | 6 | 0 | 5 | 1.12 | 26.00 | 59.8 |
| `wifi_busy` | 10 ms | 32 | 44 | 66 | 8 | 30 | 2.18 | 4.62 | 28.4 |
| `wifi_busy` | 10 ms | 48 | 44 | 66 | 8 | 30 | 2.05 | 4.73 | 40.1 |
| `wifi_busy` | 10 ms | 64 | 44 | 66 | 8 | 30 | 2.01 | 4.72 | 52.8 |
| `wifi_busy` | 20 ms | 32 | 16 | 34 | 1 | 19 | 2.13 | 5.49 | 24.6 |
| `wifi_busy` | 20 ms | 48 | 16 | 34 | 1 | 19 | 2.02 | 5.59 | 35.4 |
| `wifi_busy` | 20 ms | 64 | 16 | 34 | 1 | 19 | 1.95 | 5.62 | 47.7 |
| `dsl_far` | 10 ms | 32 | 11 | 6 | 5 | 0 | 1.75 | 5.89 | 28.7 |
| `dsl_far` | 10 ms | 48 | 11 | 6 | 5 | 0 | 1.63 | 5.96 | 40.5 |
| `dsl_far` | 10 ms | 64 | 11 | 6 | 5 | 0 | 1.57 | 5.99 | 53.2 |
| `dsl_far` | 20 ms | 32 | 9 | 9 | 0 | 0 | 1.61 | 7.46 | 24.8 |
| `dsl_far` | 20 ms | 48 | 9 | 9 | 0 | 0 | 1.48 | 7.59 | 35.7 |
| `dsl_far` | 20 ms | 64 | 9 | 9 | 0 | 0 | 1.39 | 7.66 | 48.2 |
| `mobile_hotspot` | 10 ms | 32 | 37 | 18 | 20 | 1 | 1.84 | 5.59 | 28.4 |
| `mobile_hotspot` | 10 ms | 48 | 37 | 18 | 20 | 1 | 1.72 | 5.68 | 40.2 |
| `mobile_hotspot` | 10 ms | 64 | 37 | 18 | 20 | 1 | 1.64 | 5.73 | 52.7 |
| `mobile_hotspot` | 20 ms | 32 | 18 | 13 | 6 | 1 | 1.76 | 6.22 | 24.6 |
| `mobile_hotspot` | 20 ms | 48 | 18 | 13 | 6 | 1 | 1.62 | 6.34 | 35.4 |
| `mobile_hotspot` | 20 ms | 64 | 18 | 13 | 6 | 1 | 1.57 | 6.36 | 47.7 |

## Where the 66 come from

Concealed frames on `wifi_busy`, 10 ms packets, 64 kbit/s, counted frame by frame:

- **30 late packets, all in the first 1.05 s.** The adaptive buffer starts at 10 ms and grows to 30 ms against 0–20 ms of jitter. These packets arrive after their moment; nothing in a later packet can help them. With a fixed 30 ms buffer there are no late packets, and today's code conceals 39.
- **44 lost frames in 22 pairs.** The network drops packets two at a time. FEC rebuilds 8 of them. It only covers the frame just before a packet, and only when that frame was speech.
- **For each pair (n, n+1), only n+1 can be rebuilt.** At frame n's deadline the receive queue was empty in every case: packet n+1 is lost, and packet n+2 leaves 20 ms after n and has not arrived yet. At n+1's deadline, n+2 is usually queued. So a copy one packet back rebuilds n+1, but n needs a copy that arrives 20 ms plus jitter after its own packet would have. A copy two packets back carries it, but at today's buffer depth it always comes too late. Measured: two copies per packet rebuild exactly the same frames as one, at 30 ms and at 40 ms of fixed buffer.

Best case without more buffering: 30 late + 22 first-of-pair = 52 concealed. Copies reach 61 (30 late, 19 first-of-pair, 6 lost before copies switch on at 1.03 s, since they need 100 packets of evidence, and 6 second-of-pair frames whose carrier was itself late).

### What more buffering would buy

Fixed receive buffer instead of the adaptive one, 10 ms packets, 64 kbit/s:

| buffer | sender | concealed | rebuilt | late | first sound | LSD (dB) |
| --- | --- | --- | --- | --- | --- | --- |
| 30 ms | today (FEC Automatic) | 39 | 5 | 0 | 50 ms | 1.55 |
| 30 ms | previous-frame copy, no FEC, 2× | 39 | 5 | 0 | 50 ms | 1.13 |
| 30 ms | two copies, no FEC, 2× | 39 | 5 | 0 | 50 ms | 1.29 |
| 40 ms | today (FEC Automatic) | 36 | 8 | 0 | 55 ms | 1.53 |
| 40 ms | previous-frame copy, no FEC, 2× | 31 | 13 | 0 | 55 ms | 1.09 |
| 40 ms | two copies, no FEC, 2× | 31 | 13 | 0 | 55 ms | 1.22 |

One more packet of buffering (40 ms) with copies gives 31, 53 % below the plan's 66. That is 10 ms more delay, which this task's targets rule out, so it is not done here. It is the orchestrator's call, together with the start-up behaviour of the adaptive buffer. The shared branch's relative-delay buffering (`8d52409`, `docs/silero-and-arrival-delay-20261102.md`) changes exactly that start-up, so the 30 late packets should be re-measured after the merge before deciding.

## What ships

### The plan (step 3: bitrate under loss)

`redundancy::PLAN` in `crates/relay-client/src/voice_jitter/redundancy.rs`:

- **One copy, of the previous frame.** A second copy, two packets back, rebuilt nothing on any profile at the buffer depth the adaptive buffer settles on, and costs a third of the bitrate.
- **FEC off while copies are on.** The copy covers the same frame FEC would, but for every packet (FEC only for speech) and at the full frame's quality. The encoder returns to its low-delay mode: 4 ms less codec delay and clearly less distortion. With FEC kept on as well, nothing more is rebuilt and LSD stays at FEC's level.
- **The frame and its copy share 1.5× the bitrate setting.** At 64 kbit/s each frame is coded at 44 kbit/s; never below 16 kbit/s. Today's encoder with FEC on sends only 0.74–0.89× the setting (least with 20 ms packets), so 1.5× stays under twice what the link carried before. A frame and its copy within the setting (1×) still beats today's sound, but keeps only a little over half of the gain. A larger share sounds slightly better, but it goes over 2× today's traffic with 20 ms packets.

`wifi_busy`, 64 kbit/s, every plan measured. *× shipped* is the payload while copies are on, against today's code over the same run:

| packet | plan | concealed | rebuilt | LSD (dB) | segSNR (dB) | kbit/s while on | × shipped |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 10 ms | shipped (FEC Automatic) | 66 | 8 (FEC) | 2.01 | 4.72 | 52.8 | 1.00× |
| 10 ms | **previous frame, no FEC, 1.5× (ships)** | 61 | 13 | 1.64 | 20.72 | 88.2 | 1.67× |
| 10 ms | previous frame, no FEC, 1.75× | 61 | 13 | 1.58 | 21.42 | 102.8 | 1.95× |
| 10 ms | previous frame, no FEC, 2× | 61 | 13 | 1.55 | 21.83 | 117.3 | 2.22× |
| 10 ms | previous frame, no FEC, 1× (within the setting) | 61 | 13 | 1.80 | 18.61 | 59.1 | 1.12× |
| 10 ms | previous frame plus FEC, 2× | 61 | 13 | 2.01 | 4.73 | 102.5 | 1.94× |
| 10 ms | two previous frames, no FEC, 2× | 61 | 13 | 1.68 | 20.11 | 117.2 | 2.22× |
| 20 ms | shipped (FEC Automatic) | 34 | 1 (FEC) | 1.95 | 5.62 | 47.7 | 1.00× |
| 20 ms | **previous frame, no FEC, 1.5× (ships)** | 33 | 2 | 1.59 | 21.29 | 88.5 | 1.86× |
| 20 ms | previous frame, no FEC, 1.75× | 33 | 2 | 1.54 | 21.78 | 103.3 | 2.17× |
| 20 ms | previous frame, no FEC, 2× | 33 | 2 | 1.50 | 22.92 | 118.0 | 2.47× |
| 20 ms | previous frame, no FEC, 1× (within the setting) | 33 | 2 | 1.73 | 19.48 | 58.8 | 1.23× |
| 20 ms | previous frame plus FEC, 2× | 33 | 2 | 1.95 | 5.63 | 89.1 | 1.87× |
| 20 ms | two previous frames, no FEC, 2× | 33 | 2 | 1.63 | 20.83 | 117.8 | 2.47× |

### Results, before and after (`wifi_busy`)

| packet | kbit/s | concealed before → after | rebuilt (FEC → copies) | LSD before → after (dB) | segSNR before → after (dB) | kbit/s before | kbit/s while copies on | ratio | copies on at |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 10 ms | 32 | 66 → **61** | 8 → 13 | 2.18 → **1.96** | 4.62 → 16.75 | 28.4 | 44.7 | 1.57× | 1.03 s |
| 10 ms | 48 | 66 → **61** | 8 → 13 | 2.05 → **1.72** | 4.73 → 19.30 | 40.1 | 66.4 | 1.66× | 1.03 s |
| 10 ms | 64 | 66 → **61** | 8 → 13 | 2.01 → **1.64** | 4.72 → 20.72 | 52.8 | 88.2 | 1.67× | 1.03 s |
| 20 ms | 32 | 34 → **33** | 1 → 2 | 2.13 → **1.92** | 5.49 → 17.44 | 24.6 | 44.4 | 1.80× | 2.04 s |
| 20 ms | 48 | 34 → **33** | 1 → 2 | 2.02 → **1.72** | 5.59 → 19.78 | 35.4 | 66.3 | 1.87× | 2.04 s |
| 20 ms | 64 | 34 → **33** | 1 → 2 | 1.95 → **1.59** | 5.62 → 21.29 | 47.7 | 88.5 | 1.86× | 2.04 s |

Every other profile (`lan_wired`, `wifi_good`, `dsl_far`, `mobile_hotspot`), at both packet sizes and all three bitrates, is identical to today's figures in every column. Their losses are single packets, so copies never switch on.

### When copies switch on and off

`RedundancyPolicy` in `crates/relay-client/src/voice_quality.rs` sits next to the existing `LossRecoveryPolicy` and works the same way. `ArrivalStats` now also counts loss events (gaps in the sequence) and frames lost in runs of two or more. Copies switch on when anyone you hear shows all of the following over the window:

- at least 100 packets of evidence;
- more than 2 % loss;
- at least 4 frames lost in runs;
- runs making up at least a third of the loss.

They switch off after a clean minute, and a minute with no packets at all neither ends nor restarts it. The Off choice of "Help others hear me through brief network losses" also keeps copies off, and so do packets shorter than 10 ms.

Single losses (`mobile_hotspot`, `dsl_far`) stay with FEC: tests `redundancy_turns_on_for_paired_losses_but_not_for_single_ones` and `redundancy_stays_on_for_a_clean_minute_like_recovery`. Switching rebuilds the encoder without a restart flag, as FEC switching already does, so listeners keep decoding and nobody hears a gap. With FEC set to Always, copies still replace FEC while they are on; the user's choice is honoured whenever copies are off.

### On the wire

The copy travels inside the Opus packet's own padding (RFC 6716 section 3.2.5), so the packet is still one valid Opus packet:

- The new frame becomes a code-3 packet with one frame and a padding length.
- The padding is laid out as one Opus extension (the format libopus 1.5 reads for DRED) with number 120, which no Opus release uses.
- Inside it: `R`, version 1, then one entry per copy: distance back in packets (1 byte), length (2 bytes), and the plain code-0 Opus packet.

The framing costs 8 bytes per packet with one copy. Every Opus decoder ignores padding, so an older Relay decodes the new frame bit for bit as if it were plain: the test decodes 100 packets both ways, FEC decoding included. A packet that would exceed 1,000 bytes goes out plain, leaving room for sealing and the whisper wrapper.

The receiver (`Speaker::push_timed` / `render` in `voice_jitter.rs`) unpacks first and queues the new frame as before. It keeps a copy only for a frame that is still to come, has not arrived, and matches its packet's duration and source clock. A copy is played only when its own packet is missing at the frame's deadline, before FEC and before concealment. Copies from restart packets are ignored, and copies at or behind the playout point are dropped.

### Capability

The capability is the same extension with no copies: 5 bytes of padding. A client sends it on the first packet of each talkspurt and on every 50th packet, whether or not copies are on. `Peers` records who has sent it (by the identity in the sealed frame). The sender adds copies only while every other member of the call has announced; someone who has not spoken yet counts as an older Relay. Test: `copies_wait_for_every_other_person_to_say_they_decode_them`.

Why not a header flag or a control message: today's client and server reject unknown header flag bits, and today's server would drop a new control field. Padding needs neither. **`relay-protocol` and `relay-server` are unchanged, and the server works as it is:** it routes the same sealed bytes.

Copies never carry audio to someone who could not hear it. The copy history is cleared on every talkspurt start, on an encoder restart, and whenever the whisper target or the privacy state changes.

## Delay, bandwidth and CPU

- **Receive delay:** unchanged. Same buffer at the end and same first sound on every profile (asserted by `copies_cut_busy_wifi_concealment_without_more_delay_or_double_bandwidth`, which also runs 20 ms packets).
- **Codec delay:** 2.5 ms while copies are on, against 6.5 ms with FEC on today.
- **Send delay:** none added. The copy is the previous packet's encoded bytes.
- **Bandwidth:** while copies are on, 44.7, 66.4 and 88.2 kbit/s of Opus payload at 32, 48 and 64 kbit/s with 10 ms packets. That is 1.57–1.67× today's 28.4, 40.1 and 52.8, and 1.80–1.87× with 20 ms packets. Off again after a clean minute. The capability note adds 5 bytes to one packet in 50 (under 0.1 kbit/s).
- **CPU:** no extra encoding. Copying about 100 bytes per packet on send. On receive, decoding a copy replaces a concealment decode, which costs about the same.

## Not done / for the orchestrator

- The 40 % target needs receive buffering (start-up and one more packet while copies are on), not more redundancy; see "What more buffering would buy". Re-measure after merging `8d52409`.
- These are simulated networks. Listening, and a real two-PC call with copies, are still to do (plan: orchestrator step).
- Copies switch on from the losses you receive, as FEC does. A one-way problem on your upload alone will not switch them on.

## Reproduce

The build environment is the Round 2 plan's (vcvars, cmake, `LK_CUSTOM_WEBRTC`). Then:

```bash
cargo test -j 8 -p relay-client --bin relay -- --ignored --nocapture --exact voice_jitter::tests::report_loss_redundancy_tables
```

Set `RELAY_RED_MODES` to pick what runs:

- `shipped` runs only today's code.
- `buffer` runs the fixed-buffer comparison.
- `check` runs the FEC mode check on a clean LAN.
