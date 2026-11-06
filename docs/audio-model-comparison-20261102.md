# Voice model comparisons

Voice & Video > Noise Suppression > Compare voice models exposes real local processors. Start Listen to Myself, select a model, and Save Changes to keep the personal choice for calls. Enhanced remains the default and recommended option; its RNNoise, click removal and isolation are unchanged. The user's listening preference must decide any later recommendation change. Echo cancellation is unchanged and remains a manual listening test.

| Choice | Estimated added filter delay with 10 ms packets/capture |
| --- | ---: |
| None | 0 ms |
| Enhanced / Standard RNNoise | 10 ms |
| WebRTC high-strength NS | 7 ms |
| RNNoise then WebRTC / WebRTC then RNNoise | 17 ms |
| DeepFilterNet 3 | 40 ms |
| DeepFilterNet 3 Low Latency | 20 ms |
| GTCRN (16 kHz) | 45 ms |
| Newer native RNNoise | 30 ms |
| DPDFNet 8 / DPDFNet 2 exact | 60 ms |
| Studio automatic | 60 ms |

These are buffering budgets, not measured physical headset or WAN latency. Experimental models add a bounded worker handoff sized for the larger of packet size and observed capture callback, up to 200 ms; preview labels include that observed handoff. Device periods, playback reserve and call transport add more. GTCRN includes its 48/16 kHz conversion and hop adaptation, and reduces high-frequency bandwidth.

Fixed comparisons never substitute another denoiser. Preparing/warming/behind/unavailable states are visible during preview; missing or late output is silent. Studio remains explicitly automatic and may fall back to a lighter model. Sonora is an independent gain-controller choice, Silero an independent passive speech-detector comparison, and receive buffering an independent call setting. Mumble and DNS-Challenge are not denoiser models.

Each AudioControls owns at most one comparison worker; job/result queues are bounded to 40 blocks. Switching model, PTT/privacy reset or a capture discontinuity invalidates previous generations and state. A regular 200 ms callback is supported without being treated as a gap. A failed model remains failed for that generation; it is not retried every audio frame. Immutable model plans/templates are cached; stream states are separate. No model download is needed at runtime.

## Verification

- Real inference and independent stream/reset checks for all four newly imported model engines.
- DeepFilterNet outputs compared against original upstream runtime d375b2d with tract 0.21.4: RMS errors approximately 3.1e-8 (standard), 8.9e-10 (LL).
- GTCRN compared against independent ONNX Runtime/NumPy reference: RMS error approximately 3.5e-7. Its model-bypassed STFT/resampling adapter measures 1662 samples vs the conservative 1664-sample budget (under 0.05 ms difference).
- Generated fixtures and provenance are under crates/relay-audio-models/src/fixtures. An older offline probe called init twice, adding an unintended history prefix; it is not used as the reference.
- Bounded overload, stale-result/private-audio rejection, capture stalls, 20/40/200 ms bursts, saved settings, UI and existing Enhanced preview regression tests.

Reference agreement and synthetic buffering tests do not establish perceived speech quality, echo performance or hardware end-to-end latency. The user's model preference remains open. Public beta publication is separate from this local test build.

## Note til næste lydtest før stabil udgivelse

- [ ] Prøv modellerne med samme mikrofon, gain, tale og baggrundsstøj i **Listen to Myself** og i et rigtigt opkald. Brug Enhanced som reference. Notér taleklarhed, støj mellem ord og oplevet forsinkelse.
- [ ] Mål installationspakkens og den installerede klients størrelse med og uden de modeller, der kan undværes. Kontrollér også delte biblioteker og **Studio automatic**, som kan bruge en model som reserve.
- [ ] Vælg hvilke modeller der skal med ud fra lytningen og den målte besparelse. Afprøv opgradering fra gemte indstillinger, der vælger en udgået model.
