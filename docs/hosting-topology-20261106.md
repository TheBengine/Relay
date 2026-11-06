# Hosting topology: who hosts, LAN gateways, mixed routes (6 Nov 2026)

Branch `session/hosting-topology-20261106`, from `7228d9f`. Everything here is
from source and from tests on one PC. Nothing has been tried between real PCs
or homes.

The user's goal: the best PC in the group hosts, chosen by measurement; a PC on
a LAN hosts the people on that LAN; and people can always talk to each other,
whether they are on the LAN, on iroh or on the internet.

Assumptions made without asking (as the user asked):

- "Host" means two different things in Relay, and this doc keeps them apart:
  the **community host** (the PC that runs `relay-server` and the database)
  and the **voice forwarder** (whoever passes voice packets on: the community
  host, or an approved LAN voice gateway).
- "Over the SFU": Relay voice never goes through the SFU (LiveKit). The SFU
  only carries video, screen sharing and screen-share audio. A member "on the
  SFU" therefore still sends voice to the community host, over Tailscale or the
  internet (UDP, or the optional TCP fallback). The tests treat that member as
  "reaches the host over the internet".
- The shipped server's database schema is 31 (`community::SCHEMA_VERSION`).
  Nothing here changes the schema. API changes are only extra JSON fields that
  older apps and servers ignore or never get.

## Today

### 1. Who hosts

- **How the host is chosen: by hand.** The community host is the PC where
  someone pressed *Host on this PC* (or ran `start-server.cmd`, or the Debian
  package). Nothing picks it automatically.
- **What is measured** (`relay-client/src/host_measurements.rs`, server
  `hosting_measurements.rs`): every 30 s during a call, an *approved co-host*
  (a member who accepted a Replica or Voice gateway role) reports its free
  processor, free disk and upload. Upload comes from one 256 KiB POST to the
  host, at most once a minute. Members also report "voice quality" for each
  co-host in the call. That sample's ping is the member's own ping to the
  *current host*, and its jitter and loss are measured on the co-host's voice,
  which also came through the current host. So it says little about the
  co-host's own machine.
- **Not measured:** uptime, NAT type, and anything about the *current* host's
  machine. The server code says so: "The active host is not measured from
  members' side yet".
- **How it is used:** the numbers go into the colour-and-text assessments in
  Server Settings → Hosting (`relay-protocol/src/hosting.rs`). There is also a
  switch advisor (`relay-protocol/src/host_switch.rs`): a gain of 20 points held
  for 10 minutes, then a 1-hour cooldown. The server always calls it with
  "current host: not measured" and "moving is not possible"
  (`relay-server/src/hosting.rs`, `ABILITIES` are both `false`). Even if it were
  allowed, every candidate's score would be empty, because the score uses the
  *Planned handover* assessment, and its "Takeover" line is red while planned
  handover is off.
- **When a better PC comes online, nothing happens.** The hosting page shows
  one line, for example "Automatic switching is allowed, but it waits until
  moving a community is available in this version." Planned handover exists as
  code, but it is disabled for ordinary clients. The unexpected-takeover steps
  A3–A5 are paused (`docs/plan-status-20261027.md`).

### 2. LAN

- **Who can be a gateway:** an administrator offers the *Voice gateway* role,
  the member accepts it, and the member turns on *Allow approved local voice
  gateways* (default off). While that member is in a voice call, their app
  registers its private IPv4 address and port for that call
  (`gateway_control.rs` `serve_loop`). Every such member on every LAN registers.
  There is no election, and two of them on one LAN both register.
- **Who uses it:** another member in the same call with the same setting turned
  on fetches the list, which the server sorts by account id. The member takes
  the **first** entry whose address is on one of its own subnets
  (`lease_loop`), so today "the lowest account id on your subnet" wins by
  accident. The member leases a 2-second hop key from it. The member's app
  sends voice through the gateway only when the whole path to the host answers
  and is at least 2 ms faster than the normal route, or when the normal route
  fails (`local_voice_gateway.rs` `usable`). Two gateways on the same LAN may
  even lease from each other.
- **What the gateway does:** on its own, it is one extra hop. A→G→host→G→B, so
  speech between two people on the LAN still goes out to the host and back. With
  the mesh (`relay-protocol/src/voice_mesh.rs`, server
  `gateway_admission/mesh.rs`), the gateway also copies a home member's frame
  to the other people at the same gateway. The host then skips that pair, but
  only while the whole mesh has proven that it carries the pair.
- **How it is shown:** in the call panel, "Local voice gateways: Using a
  measured gateway route" or "Enabled · using the normal voice route". It does
  not say which PC, or why that one. Server Settings → Hosting shows the role
  and an Enable/Stop button. `lan_discovery.rs` and `lan_host_ui.rs` find the
  *community server* on the LAN. They have nothing to do with voice gateways.

### 3. Mixed routes

The voice host is a star (`relay-server/src/voice.rs` `relay_packet`): it sends
every frame to every other person in the channel, except a pair that the mesh
verifiably carries. The server's mesh plan (`host_relay: false`) only ever
holds people in a home with a gateway, so someone on iroh or on the internet is
never "carried" and always gets everything from the host.

**A and B on the LAN, C over iroh or the internet: yes, all three hear each
other.** Test `a_lan_home_an_iroh_member_and_an_internet_member_all_hear_each_other`
(relay-server, real router and real `relay_packet`) checks this. In the test,
Gus (11) is the gateway, Cleo (22) leases from him, Otto (33) is on the
internet or Tailscale, and Lena (55) is on iroh. Each of the 12 speaker→listener
pairs gets each frame exactly once, by exactly one way:

| Pair | Without mesh | With mesh (one home) | After Cleo's hop fails |
|---|---|---|---|
| Gus ↔ Cleo | host | **gateway only** (host skips) | host |
| Gus/Cleo ↔ Otto | host | host | host |
| Gus/Cleo ↔ Lena | host | host | host |
| Otto ↔ Lena | host | host | host |

Paths per packet, with the mesh on:
- A→B: A → gateway (LAN) → B. A's ordinary copy still goes to the host, which
  leaves out B.
- A→C: A → host (direct, or A → gateway hop → host) → C's iroh helper → C.
- C→A and C→B: C → iroh → host → A and B.

The client data-plane simulation agrees
(`simulation_a_lan_pair_and_a_remote_member_hear_each_other_once`,
`local_voice_gateway/mesh/tests.rs`): every listener hears every frame once,
and none of the home's own speech goes up its internet link.

**A on the LAN, B "over the SFU", C over iroh:** for voice this is the same
picture, because B's voice goes to the host and not to the SFU. For **video and
screen sharing it breaks**. The SFU needs the LAN or Tailscale, and iroh carries
no video, so C sees no video and A and B see none from C. This is documented in
`docs/connection-modes.md`.

Where it breaks or is weak today:

1. **The community host is the only bridge.** When it goes away, C hears
   nobody and nobody hears C. A and B keep talking over the gateway only until
   the mesh keys run out (2-second leases that only the host renews). Test:
   `simulation_when_the_host_goes_away_only_the_lan_pair_keeps_talking`.
2. **A LAN gateway cannot bridge someone who is not on its LAN.** Leases only
   go to private addresses on the member's own subnet, and the "member link
   over iroh" was never built (`voice-mesh-two-households.md`). So someone who
   can reach the gateway but not the host has no way in. In practice everyone
   in a call reaches the host, because the call ticket comes from it.
3. **Up to about 1.2 s of silence for a LAN pair** when a gateway hop dies
   without a word. The host goes back to relaying only when the coverage ends
   (`READY` = 1.2 s), or when a member reports an unhealthy hop.
4. **Without a mesh**, LAN speech makes a round trip through the host, which
   may be across the internet.
5. **Two gateways on one LAN** become two separate "homes" with no link
   between them. The planner then leaves the second home out, and the host
   carries it. That still works, but the LAN traffic goes through the host.

## Done

Commits on `session/hosting-topology-20261106` after `7228d9f`. The "Today"
section above was committed first (`1c6aa80`), before any change.

### One rule for choosing a machine (`relay-protocol/src/host_choice.rs`)

- **Score, from measurements only**: upload against what the group needs
  (twice the estimate gives full marks), free processor, and for a gateway its
  round trip to the host. Weights are 50/30/20, or 60/40 without a round trip.
  A machine without measured upload and processor has no score. Nothing is
  guessed.
- **Leader**: the highest score. Scores within 15 points count as equal, and
  then the machine that has stood longest wins (the running host, or the
  gateway already in use), then the smaller id. Every PC with the same list
  picks the same machine.
- **No flapping**: the current choice changes only when another machine has
  been the leader for **60 s without a break**. After a change nothing changes
  for 5 minutes. Nothing is compared while the current choice itself is not
  measured, and an unmeasured machine never wins.
- Tests: 7, including measurements that flap (40 s ahead, then a dip,
  30 times; and two machines trading a 30-point lead every minute): no change
  at all.

### (a) Mixed routes: every pair can talk

This already worked through the host (see "Today"). It is now locked in by
tests:

- Server, real router and `relay_packet`: LAN pair on the gateway, Otto on
  the internet or Tailscale (the "SFU" member, for voice), Lena on iroh. All
  12 pairs get each frame exactly once, without the mesh, with it, and after a
  failed hop.
- Gateway simulation: a1/a2 on the LAN gateway, c remote. 50 frames: 50 + 0
  for the LAN pair (gateway only), 0 + 50 for every pair with c (host only),
  and no home upload for home speech.
- Host gone after frame 20: the LAN pair keeps all 50 frames over the
  gateway; c gets 20 and is heard for 20.

No bridge code was needed. The host already relays everyone the mesh does not
verifiably carry, and the server's mesh plan never contains a person outside a
home.

### (b) Host choice by measured capability

- **The running host is measured now.** The server samples its own free
  processor (Windows `GetSystemTimes`, Linux `/proc/stat`). Members time a
  256 KiB random download from the new `GET /api/hosting/measurements/download`
  (once a minute at most per member; the app does it every 5 minutes), and the
  host's upload is taken as at least the fastest fresh rate. The app sends
  `host_download_kbps` only after the probe answered, so a shipped server never
  sees the field.
- **Ranking and stable preference** (`relay-server/src/hosting.rs`
  `host_choice`): the running host and every approved co-host are scored, the
  preference above runs over them, and the hosting view gets `host_ranking`
  (an extra field that older apps ignore). The old switch advisor (10 min /
  1 h, never fed any data) is no longer used by the server.
- **Shown in plain English** in Server Settings → Hosting → *Choosing the
  host*, one line per machine, for example
  `1. Bo's PC (preferred): 92 of 100. upload 40 Mbit/s (19.8x what 8 people need), processor 80% free`
  and `2. This host (Home) (hosting now): …`. Then the advice, for example
  "Bo's PC has been clearly better by measurement than This host (Home) for
  20 s. It is preferred once that holds for 60 s without a break. Hosting is
  set to keep the current host, so nothing moves."
- Tests: the server ranking test goes from nothing measured, to Bo and the
  host measured ("Wait"), to the lead held for 60 s ("preferred"). The policy
  sentence is checked, and so is a refused 0 kbit/s report. Also download
  probe tests (random bytes, rate limit, sign-in) and the host-machine
  sampling test.

### (c) LAN gateway election

- **Offers carry the election data**: `capability` (the gateway's reported
  upload and processor, and its round trip to the host from its mesh report)
  and `serving_secs` (how long this exact registration has served; a renewal
  keeps it). Both are optional fields: the shipped app ignores them, and the
  new app treats a shipped server's offers as unmeasured. Unmeasured offers are
  ordered by id, which is the order the app used before.
- **Members** (`gateway_control.rs` `lease_loop`) list every 5 s while in
  a call, keep only gateways on their own subnets, and follow the preference.
  The first pick is immediate; a later change needs the 60-second lead.
- **Gap-free move**: the member first turns the old hop off for audio. The
  next voice tick then sends the binding keepalive on the normal route, which
  stays registered with the host the whole time. Audio goes that way and is
  never dropped (test
  `retiring_a_path_sends_audio_on_the_normal_route_and_never_drops_it`).
  Only then is the old lease released and the new gateway leased. It is
  selected as soon as its full path is measured faster, as before.
- **Gateways** (`serve_loop`): an approved PC does not start serving where
  another gateway on its LAN would win. A serving gateway that has not been
  the winner for 60 s keeps serving for 20 s more, so its people move first.
  It then stops, rests 5 minutes and uses the elected gateway like everyone
  else. The result is one gateway per LAN, and so one "home" in the mesh.
- **Shown in the call panel** under *Local voice gateways*, for example "Gus's
  PC serves voice for your network: upload 30 Mbit/s (14.9x what 8 people
  need), processor 70% free, 20 ms to the host. It was elected from 2 approved
  gateways here because it measured best or has served longest." While a move
  is pending it adds, for example, "Ada's PC now measures better (12 s);
  everyone here moves to it after 60 s, without a gap."
- Simulated LAN (`simulation_a_lan_moves_to_a_better_gateway_once_and_without_a_gap`,
  three minutes in 5 s rounds): Gus has served for 10 minutes, and Ada's much
  stronger PC starts. Lena moves from Gus to Ada **once, at 60 s**. Max, who
  joins at 30 s, picks Ada at once. Gus stops serving at **80 s**, after Lena
  has moved.

## Not done

- **Moving the community host itself.** The preference now names the best
  machine, but nothing moves the community. Planned handover is still off
  (`ABILITIES.planned_handover = false`), the failover steps A3–A5 are paused,
  and the database move is not gap-free for calls. I did not turn it on: it
  moves the database and the sessions, and it needs the two-PC test first.
  Until then "the best PC hosts" is advice on the Hosting page, not automatic.
- **Uptime and NAT type** are still not measured, so they are not in the
  score.
- **The co-host "voice quality" samples** still measure the path through the
  current host (see "Today"). They are left out of the score on purpose.
- **A LAN gateway bridging someone who is not on its LAN** (an iroh member link
  to the gateway). The host remains the only bridge for remote members. If
  the host goes away they lose the call, and the LAN pair keeps talking for at
  most the 2 s gateway lease.
- **Several homes without a link between them**: the planner still carries
  only one home component, and the others go through the host. This works, but
  without the local shortcut.
- **The up to 1.2 s of silence** when a gateway hop dies without a word is
  unchanged.
- **Edge case in the election:** a gateway that restarts with a new instance
  loses its standing. If another gateway is about as good, the restarted one
  steps back. People still on it then fall back to the normal route after the
  hop dies (about 0.75 s), not by the gap-free move.
- **Mixed app versions:** a shipped app that serves as a gateway does not take
  part in the election. Its users keep it, and a new app on the same LAN may
  serve alongside it.
- **Video and screen sharing over iroh** (needs a media transport).
- Nothing here has been tried on real PCs, between homes, or over the
  internet. All results are from one PC: the real server code in-process, the
  gateway data plane over in-memory packets, and the election as pure
  functions over simulated time.

## Test runs (6 Nov 2026, this PC, `-j 8`, 8 test threads)

- `relay-protocol`: 81 + 8 + 1 passed. The `installation` tests need
  `RELAY_TEST_WORK_ROOT` set to an existing local folder.
- `relay-server`: 464 passed, 2 ignored; plus 27 in the second test target.
- `relay-client --bin relay --features native-rtc`: 1755 passed, 129 ignored,
  5 failed. The 5 are the sign-in vault tests that need their own QA profile
  (`central_account` / `network_tests` vault trials). AGENTS.md says they fail
  outside it by design, and none of them touches this work.
- Clippy on all three crates: nothing in the code added or changed here.
  `-D warnings` is already red on the base, in files this work does not touch
  (for example `relay-protocol` `handover.rs`, `takeover.rs` and
  `voice_mesh.rs`, and the client's `local_voice_gateway.rs:945`
  `never_loop`).
