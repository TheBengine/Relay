# Relay patch to eframe 0.27.2

Source: the already locked local crates.io 0.27.2 source. Original file hashes
are recorded in UPSTREAM-SHA256.json. No dependency versions were upgraded.

The native repaint scheduler retained expired requests for hidden Windows
windows even though winit cannot deliver their normal redraws. ControlFlow::Poll
then spun until Relay's explicit background pump painted a frame. Both native
run paths now drop undeliverable requests and compute Wait / WaitUntil / Poll
from the remaining deadlines. Visible redraws and future deadlines are retained.
Relay's tray pump continues handling background work and wakes explicitly on
restore/quit. The iOS workaround is retained; non-Windows visibility is unchanged.

Focused scheduling regressions are in native/run.rs (relay_repaint_tests).
The upstream MIT OR Apache-2.0 license still applies to this modified dependency.
