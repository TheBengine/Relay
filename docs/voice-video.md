# Settings, voice and video

*Moved from the project README on 16 October 2026 so the README can stay short.*

## Settings and appearance

Settings opens a full-window workspace with grouped navigation and searchable page names. Account, Profiles, Encryption, Privacy & Safety, Connections, Appearance, Accessibility, Notifications, Sounds, Keybinds, Advanced and Server Settings expose Relay's controls. **Voice & Video** separates Voice, Video and Debugging into tabs. Close or Escape returns to the conversation; source preview remains an explicit separate action.

Appearance offers **Dark**, **Onyx**, **Ash**, **Light** and **Sync with computer**, plus Cozy/Compact messages, text scaling, interface zoom, message-group spacing and GIF autoplay. Text scaling and interface zoom are independent and compound. With autoplay off, a GIF can still be played manually; Reduced Motion prevents playback, including expanded previews. Appearance and other visual preferences apply immediately. Changed voice settings use **Save Changes** or **Reset**; selecting a different audio device reconnects the current call when saved. Video quality changes apply after restarting sharing.

The familiar organization and display controls use Discord's official [appearance guide](https://support.discord.com/hc/en-us/articles/207260127-How-to-Change-Discord-Color-Themes-and-Customize-Appearance-Settings) and [desktop display article](https://discord.com/blog/making-discord-on-desktop-look-just-right-display-settings-to-ease-the-eyes) as design references. Relay's controls operate on its own native features. See the [settings acceptance checklist](settings-release-qa.md) for release coverage and remaining interaction checks.

## Low-latency receive buffer

For low receive buffering, choose **User Settings → Voice & Video → Low latency · 10 ms**, then save. The `--low-latency` startup flag also selects this preset. **Automatic · 10–80 ms** adds buffering when needed. Existing saved manual targets are preserved. Local queue limits and smaller device-buffer requests reduce avoidable backlog; they are not a measurement or guarantee of total call delay. See [the latency comparison](latency-comparison.md) for the measured scope and Mumble acceptance procedure.

## Hear your microphone before a call

In **Voice & Video → Voice**, choose your input and output devices, use headphones, then select **Listen to Myself**. The local preview plays microphone PCM through Relay's input gain, noise reduction, voice-activation or push-to-talk processing and selected output volume. With push-to-talk, hold the displayed key to listen. Release clears queued preview speech and its render history. **Stop Listening**, leaving the page, closing Settings, Save and Reset stop the preview.

The preview uses a snapshot of the unsaved audio choices. Editing an audio setting or the push-to-talk key stops it; start listening again to hear the new choices. **Save Changes** applies the choices to calls. Preview does not change saved call settings, record audio or send it to the network. Leave a call before starting it. Use headphones to avoid microphone/speaker feedback.

This tests local processing and device selection. It does not test Opus compression, a remote listener, internet reliability or physical microphone-to-ear latency. Real-headset and unplug/recovery acceptance remain separate from synthetic callback tests.

## Speech clarity and brief dropouts

**Keep the beginning of words** preserves the preceding 10 ms when Voice Activity opens (20 ms with 20 ms packets). It adds that much speech delay, is included in the settings budget and can be disabled. Push-to-talk never sends a lead-in. Mute and recipient changes clear the short history.

**Help others hear me through brief network losses** is an optional outgoing speech-recovery choice for 10/20 ms packets. It adds a little codec delay and needs the next packet to reach the listener on time. Listeners can choose **Automatic** or **Balanced** receive buffering for uneven arrival. Recovery is inactive with 2.5/5 ms packets and cannot fix every dropout. Packet duration and receive-buffer defaults remain 10 ms.

The final output mixer protects against overload from combined voices, shared audio and volume changes. Automatic receive timing distinguishes actual late arrivals from missing packets, and quiet-queue recovery avoids removing low-level words. See [the audio change and measured scope](audio-call-quality-20261020.md) for tradeoffs and evidence.

## Voice and native video

Voice settings include named microphone/playback devices, gain, master and per-person volume, voice activity/continuous/push-to-talk transmission, RNNoise suppression, bitrate and source-clock-aware jitter controls. Echo cancellation is optional. Microphone Preview opens local input and output; event-sound previews are output only. Devices are opened for explicit join, test or preview actions. Audio DNS/start/drop and global push-to-talk polling run off the UI thread. Join remains cancellable in the global footer, and optional sound-cue errors leave a call running.

The protected voice protocol adds an inner authenticated encryption layer whose call keys come from MLS; the relay can route packets without decrypting the Opus payload. It carries source sequence/sample timing, replay protection, concealment and talkspurt resets. Capture sends complete frames as they become available and playout follows source deadlines, removing redundant scheduling waits. These code changes do not establish a physical latency result. Diagnostics distinguish encrypted UDP round-trip probes from microphone-to-speaker delay.

Sharing has explicit source selection and local preview. Native WebRTC carries screen/window or camera video through the bundled SFU, with GCM end-to-end frame encryption enabled for the current protected call. Plaintext frames, unknown senders and incorrect keys are rejected. New clients do not publish unprotected JPEG call previews as a fallback. Current settings offer 480p through 4K or Source, requested 15/24/30/60 fps, automatic or 2–32 Mbps bitrate ceilings, adaptive quality and separate camera/screen encoder choices. Synthetic video evidence targets 30 fps. Windows Graphics Capture is used for native display/window capture. Hardware H.264 is available with a software alternative; the current hardware path stages pixels through CPU memory and is not zero-copy. Requested settings are targets, not guaranteed sustained capture or receiver FPS.

Automatic video bitrate allows up to **4 Mbit/s at 720p and 30 fps** to preserve motion in detailed scenes. Actual bitrate can be lower, and server/network limits still apply. Saved manual bitrate choices, including 2 Mbit/s, are preserved. The higher automatic ceiling can use more data; it does not guarantee receiver FPS or internet performance.

**Voice & Video → Debugging** shows measured video upload separately for active camera and screen publications. It uses recent, matching outbound RTC counters and an approximate local sampling interval, including when adaptive quality is off. Missing, reset, ambiguous or stale samples show as unavailable. The figure is RTP video payload traffic including retransmissions, excluding headers, microphone voice and application audio; it does not measure what viewers receive. The counter definition follows [WebRTC statistics](https://www.w3.org/TR/webrtc-stats/#dom-rtcsentrtpstreamstats-bytessent). Capture FPS is labeled as capture FPS, separately from requested FPS and measured upload.

When refreshed call keys interrupt your camera or screen sharing, a reminder beside the voice-room controls identifies the stopped sources. It remains until you explicitly restart the relevant source or leave the call. A call update never restarts capture automatically.

**Application audio** is an opt-in for a selected window's owning process tree. It does not capture a full desktop mix or silently duplicate the microphone. Receivers play that screen-share audio through a bounded mixer. Local previews do not publish video or application audio. Closing preview, stopping sharing, room changes and logout cancel old capture generations.

Actual local synthetic SFU/SDK, codec, authorization and lifecycle tests are documented in [verification](verification.md). Real headsets, webcams, game windows, sustained multi-stream calls, audio/video synchronization and WAN/TURN behavior require the [hardware acceptance steps](hardware-acceptance.md). Two local clients on one PC cannot establish two-PC or internet reliability.
