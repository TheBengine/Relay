# Network profiles and automatic loss recovery — 28 October 2026

Relay now turns on **Help others hear me through brief network losses** by itself when packets go missing, and turns it off again when the network is clean. This page records how that was measured on five simulated networks, before and after the change.

These are simulated networks on a simulated clock. They use the real Opus encoder and decoder and Relay's real receive buffer, but no sound devices, sockets or real Wi-Fi. They show how the receive path behaves under each kind of trouble; they are not a listening test.

## The setting

**Help others hear me through brief network losses** is now a choice of three:

- **Automatic (new default):** recovery switches on when more than 2 of every 100 packets you receive from someone go missing over the last 5–10 seconds, and switches off after a clean minute. It needs at least 100 packets (one second of 10 ms packets) from that person before it trusts the figure, so one early loss cannot switch it on. While it is on, the codec adds about 4 ms of delay.
- **Always:** the old "on". Saved profiles that had the switch on keep it as Always.
- **Off:** the shortest codec delay. A lost packet is filled in by guessing.

Saved profiles that had the switch off (the old default) move to Automatic. Recovery still only works with 10 or 20 ms packets.

Relay has no report from the other person about how they receive you. Automatic therefore uses the loss you measure on their packets as a stand-in for the loss on yours. Home Wi-Fi and mobile loss usually hits both directions, but a one-way problem (for example their upload only) will not switch your recovery on. In a group call the worst person you hear decides.

The plan's step 5 (hold the adaptive bitrate while loss is high) does not apply: Relay has no adaptive voice bitrate, and the bitrate only changes when someone moves the slider, so nothing was changed there.

## How the simulation works

`network_profiles::simulate` in `crates/relay-client/src/voice_jitter.rs` sends 1,000 packets of 10 ms (10 seconds) of the audio bench's synthetic talker (`voice_test_signals::talker`, same seed as `docs/audio-bench-baseline-20261028.md`). Packets are encoded at 96 kbit/s with the same encoder factory the call uses. Packet 1 leaves at 10 ms. The receiver uses the factory receive setting (Automatic, 10 ms floor) and renders 5 ms of audio every 5 ms, exactly as the `stable_ten_ms_path_starts_ten_ms_earlier_without_losing_audio` test does.

Each network is drawn once from a fixed seed, before any audio is encoded, so Off and Automatic see the same losses at the same moments.

| profile | one way | jitter | loss | loss events | reorder |
| --- | --- | --- | --- | --- | --- |
| `lan_wired` | 5 ms | 0 | 0 % | — | 0 % |
| `wifi_good` | 8 ms | 0–5 ms | 0.5 % | single | 0 % |
| `wifi_busy` | 10 ms | 0–20 ms | 3 % | 2 packets in a row | 0 % |
| `dsl_far` | 40 ms | 0–10 ms | 1 % | single | 0 % |
| `mobile_hotspot` | 30 ms | 0–40 ms | 5 % | single | 2 % |

Jitter is extra queueing delay drawn evenly between zero and the stated value; packets keep their order unless the reorder share swaps one with the next. The plan gave jitter and loss; the one-way delays for the Wi-Fi and hotspot rows are this page's choice.

With Automatic, the simulated sender reads the loss the simulated receiver measures over the 5 s window. That is the symmetric-path assumption a real sender makes.

## Results

**Columns:** *lost* = packets the network dropped. *concealed* = frames played by guessing. *rebuilt* = lost frames rebuilt from recovery data in the next packet. *late* = packets that arrived after their moment had passed. *held* = frames of extra buffering added while playing. *first sound* = time of the first audio out (packet 1 leaves at 10 ms). *rendered* = audio samples played; 480,000 is the full 10 seconds. *buffer* = receive buffering at the end.

### Before (today's receive code, recovery off: the old default)

| profile | lost | concealed | late | held | first sound | rendered | buffer |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `lan_wired` | 0 | 0 | 0 | 0 | 15 ms | 480,000 | 10 ms |
| `wifi_good` | 2 | 7 | 5 | 2 | 20 ms | 480,480 | 20 ms |
| `wifi_busy` | 44 | 74 | 30 | 2 | 25 ms | 480,960 | 30 ms |
| `dsl_far` | 11 | 11 | 0 | 1 | 60 ms | 480,480 | 20 ms |
| `mobile_hotspot` | 37 | 38 | 1 | 3 | 80 ms | 480,480 | 30 ms |

### After (new code, Automatic: the new default)

| profile | lost | concealed | rebuilt | late | held | first sound | rendered | buffer | recovery on |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `lan_wired` | 0 | 0 | 0 | 0 | 0 | 15 ms | 480,000 | 10 ms | never |
| `wifi_good` | 2 | 7 | 0 | 5 | 2 | 20 ms | 480,480 | 20 ms | never |
| `wifi_busy` | 44 | 66 | 8 | 30 | 2 | 25 ms | 480,960 | 30 ms | at 1.03 s |
| `dsl_far` | 11 | 6 | 5 | 0 | 1 | 60 ms | 480,480 | 20 ms | at 1.29 s |
| `mobile_hotspot` | 37 | 18 | 20 | 1 | 3 | 80 ms | 480,480 | 30 ms | at 1.05 s |

The new code with recovery forced off gives exactly the "before" figures.

No profile played less audio or had a silent gap, and recovery added no receive buffering in any profile. Played audio above 480,000 is the held frames (480 samples each), not repeated speech.

### Acceptance against the plan

- **No profile loses rendered samples:** pass. Every profile plays at least 480,000 samples with no silent gap, before and after.
- **`wifi_busy` concealed frames at least halved:** **fail.** 74 → 66 (11 % fewer).
- **`lan_wired` first sound unchanged at 15 ms:** pass. 15 ms before and after; recovery never switches on.

### Why `wifi_busy` is not halved

Opus recovery data in a packet covers only the frame just before it. On `wifi_busy` every loss is two packets in a row, so the first frame of each pair has nothing to rebuild it from: at best half of the 44 lost frames can be rebuilt. The other 30 concealed frames are late packets during the first 1.25 seconds, while the receive buffer grows from 10 to 30 ms. They arrive late, not lost, so recovery cannot help them. Even with recovery data in every packet, the best possible result would be about 22 + 30 = 52, a 30 % drop.

Recovery data is also only sent for speech. 513 of the roughly 900 packets sent after the switch carried it; frames in pauses between syllables do not. Losing a pause is not audible, but it still counts as concealed here.

Where losses are single packets, recovery does what it should: `mobile_hotspot` drops from 38 to 18 concealed frames (53 % fewer) and `dsl_far` from 11 to 6.

Things that would get closer to halving `wifi_busy`, each with a cost, for a later decision:

- Sending each packet a second time two packets later while loss is high. This rebuilds both frames of a pair, but doubles voice upload exactly when the network is struggling, and it changes the send path (outside this task's files).
- Newer Opus deep redundancy (libopus 1.5 DRED), which covers up to a second. It needs a newer libopus with its model; the linked one reports no version.

A buffer that grows one step whenever recovery data arrives just too late was also tried. On `wifi_busy` it rebuilt one more frame (9 instead of 8) at the cost of 10 ms more delay (40 ms instead of 30), so it was left out.

## Other receive-side changes

- **Rebuilt is not concealed.** A frame rebuilt from recovery data now counts as *rebuilt*, not *concealed*. The receiver reads the recovery flag at the start of the Opus packet (RFC 6716 section 4.2.3) to tell them apart. The call statistics' "concealed" figure therefore falls when recovery works.
- **The last frame of a burst is rebuilt.** Before, recovery was only tried for the first missing frame in a row. The next packet's data belongs to exactly the frame before it, so it is now used for the last missing frame of any short burst.
- **No mid-sentence restart when recovery switches.** Turning recovery on or off rebuilds the encoder. The packet after it is no longer marked as the start of a new talkspurt, so listeners keep playing without re-buffering. Before this, the listener's buffer started over and left 5–15 ms of silence mid-sentence. A receiver also now treats a restart that continues the source clock exactly as an encoder change rather than a new talkspurt, which avoids the same gap after other mid-call changes such as packet size.

## Run it again

```bash
cargo test -p relay-client --bin relay -- --nocapture every_profile_renders_all_samples_and_reports_concealment automatic_loss_recovery_rebuilds_lost_speech_without_touching_a_clean_lan
```

Each profile prints its full report. Builds need the MSVC and LiveKit SDK environment described in the plan.
