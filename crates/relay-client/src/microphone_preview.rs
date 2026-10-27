//! Local microphone monitoring using the call's PCM processing and render path.
//! No encoder, socket, RTC publication, recording or call-owned queue is created.
use super::*;

pub(super) struct Session {
    stop: Arc<AtomicBool>,
    controls: Arc<AudioControls>,
    input: Option<DeviceStream>,
    output: Option<DeviceStream>,
    worker: Option<JoinHandle<()>>,
}

pub(super) fn controls(settings: &AudioSettings, ptt_key: i32) -> AudioControls {
    let mut controls = AudioControls::new(settings);
    controls.local_preview = true;
    controls.ptt_key.store(ptt_key, Ordering::Release);
    // A whisper key or target from a conversation has no meaning in a local test.
    controls.whisper_enabled.store(false, Ordering::Release);
    controls
}

impl Session {
    pub(super) fn start(request: MicrophoneStart, level: Arc<AtomicU32>) -> Result<Self, String> {
        let MicrophoneStart { stop, settings, errors, controls, taps, .. } = request;
        let settings = &settings;
        ensure_not_cancelled(&stop)?;
        let counters = Arc::new(Counters::default());
        let capture = sample_queue(VecDeque::with_capacity(MAX_CAPTURE_SAMPLES));
        let playback = sample_queue(VecDeque::with_capacity(MAX_PLAYBACK_SAMPLES));
        let limit = Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES));
        let host = cpal::default_host();
        let output_device = audio_device(&host, &settings.output_device, false)?;
        let output_config = output_device
            .default_output_config()
            .map_err(|error| format!("Preview output format: {error}"))?;
        ensure_not_cancelled(&stop)?;
        let output = make_output(
            &output_device,
            &output_config,
            OutputState {
                playback_limit: limit.clone(),
                counters: counters.clone(),
                queue: playback.clone(),
                deafened: Arc::new(AtomicBool::new(false)),
                errors: errors.clone(),
                controls: controls.clone(),
                tones: Arc::new(Mutex::new(VecDeque::new())),
                stop: stop.clone(),
            },
        )?;
        ensure_not_cancelled(&stop)?;
        let input_device = audio_device(&host, &settings.input_device, true)?;
        let input_config = input_device
            .default_input_config()
            .map_err(|error| format!("Preview microphone format: {error}"))?;
        let input = make_input(
            &input_device,
            &input_config,
            InputState {
                queue: capture.clone(),
                level,
                muted: Arc::new(AtomicBool::new(false)),
                deafened: Arc::new(AtomicBool::new(false)),
                counters,
                errors: errors.clone(),
                controls: controls.clone(),
                stop: stop.clone(),
            },
        )?;
        ensure_not_cancelled(&stop)?;
        output
            .play()
            .map_err(|error| format!("Start preview output: {error}"))?;
        ensure_not_cancelled(&stop)?;
        input
            .play()
            .map_err(|error| format!("Start preview microphone: {error}"))?;
        ensure_not_cancelled(&stop)?;
        let worker_controls = controls.clone();
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("relay-mic-preview".into())
            .spawn(move || {
                // Native speech detection owns a thread-affine handle.
                let mut monitor = Monitor::new(
                    capture,
                    playback,
                    limit,
                    worker_controls,
                    worker_stop,
                    errors,
                    taps,
                );
                while !monitor.stop.load(Ordering::Acquire) {
                    poll_push_to_talk(&monitor.controls);
                    if !monitor.step() {
                        thread::sleep(Duration::from_micros(2500));
                    }
                }
                monitor.clear();
            })
            .map_err(|error| format!("Start microphone preview: {error}"))?;
        Ok(Self {
            stop,
            controls,
            input: Some(input),
            output: Some(output),
            worker: Some(worker),
        })
    }

    pub(super) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The next render callback emits silence before device disposal completes.
        self.stop.store(true, Ordering::Release);
        self.controls.wake.notify();
        self.input.take();
        self.output.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Monitor {
    capture: SampleQueue,
    playback: SampleQueue,
    limit: Arc<AtomicUsize>,
    controls: Arc<AudioControls>,
    stop: Arc<AtomicBool>,
    errors: ErrorSlot,
    processor: MicrophoneProcessor,
    generation: u64,
    taps: Arc<crate::self_listen::MeterTaps>,
}

impl Monitor {
    fn new(
        capture: SampleQueue,
        playback: SampleQueue,
        limit: Arc<AtomicUsize>,
        controls: Arc<AudioControls>,
        stop: Arc<AtomicBool>,
        errors: ErrorSlot,
        taps: Arc<crate::self_listen::MeterTaps>,
    ) -> Self {
        let generation = controls.privacy_generation.load(Ordering::Acquire);
        Self {
            capture,
            playback,
            limit,
            controls,
            stop,
            errors,
            processor: MicrophoneProcessor::new(),
            generation,
            taps,
        }
    }

    fn clear(&self) {
        if let Ok(mut capture) = self.capture.lock() {
            capture.clear();
        }
        if let Ok(mut playback) = self.playback.lock() {
            playback.clear();
        }
    }

    /// One bounded PCM block, also driven directly by synthetic callback tests.
    fn step(&mut self) -> bool {
        if self.stop.load(Ordering::Acquire) {
            self.clear();
            return false;
        }
        let generation = self.controls.privacy_generation.load(Ordering::Acquire);
        if generation != self.generation {
            self.clear();
            self.processor = MicrophoneProcessor::new();
            #[cfg(feature = "native-rtc")]
            self.controls.echo_reference.clear();
            self.generation = generation;
        }
        if self.controls.transmission.load(Ordering::Acquire) == TransmissionMode::PushToTalk as u32
            && !self.controls.ptt_pressed.load(Ordering::Acquire)
        {
            // The meter still shows that the microphone works while the key is up.
            let raw = self.capture.try_lock().ok().and_then(|mut queue| {
                let mut peak = None::<f32>;
                while let Some(sample) = queue.pop_front() {
                    let sample = if sample.is_finite() { sample.abs() } else { 0.0 };
                    peak = Some(peak.unwrap_or(0.0).max(sample));
                }
                peak
            });
            if let Some(raw) = raw {
                self.taps.record_raw(raw);
            }
            self.clear();
            return false;
        }
        let samples = capture_processing_samples(
            self.controls.packet_samples.load(Ordering::Relaxed) as usize,
            &self.controls,
        );
        let mut storage = [0.0; AUDIO_FRAME_SAMPLES];
        let frame = &mut storage[..samples];
        let ready = self.capture.try_lock().is_ok_and(|mut queue| {
            if queue.len() < samples {
                return false;
            }
            for sample in frame.iter_mut() {
                *sample = queue.pop_front().unwrap_or(0.0);
            }
            true
        });
        if !ready {
            return false;
        }
        self.taps.record_raw(crate::self_listen::peak(frame));
        let audible = self.processor.process(frame, &self.controls);
        self.taps.record_processed(frame, audible);
        #[cfg(feature = "native-rtc")]
        if let Some(error) = self.processor.echo_error.take() {
            record_error(
                &self.errors,
                format!("Microphone preview processing: {error}"),
            );
            self.stop.store(true, Ordering::Release);
        }
        if audible
            && !self.stop.load(Ordering::Acquire)
            && self.controls.privacy_generation.load(Ordering::Acquire) == generation
        {
            if let Ok(mut queue) = self.playback.lock() {
                append_playback_frame(&mut queue, frame, self.limit.load(Ordering::Relaxed));
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> AudioSettings {
        AudioSettings {
            transmission: TransmissionMode::Continuous,
            microphone_gain: 1.0,
            playback_volume: 1.0,
            noise_suppression: false,
            automatic_microphone_volume: false,
            echo_cancellation: false,
            ..Default::default()
        }
    }

    fn monitor(settings: &AudioSettings) -> Monitor {
        Monitor::new(
            sample_queue(VecDeque::new()),
            sample_queue(VecDeque::new()),
            Arc::new(AtomicUsize::new(MAX_PLAYBACK_SAMPLES)),
            Arc::new(controls(settings, 0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(crate::self_listen::MeterTaps::default()),
        )
    }

    fn render(monitor: &Monitor) -> impl FnMut(&mut [f32]) + Send {
        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Default,
        };
        output_callback::<f32>(
            &config,
            OutputState {
                queue: monitor.playback.clone(),
                playback_limit: monitor.limit.clone(),
                counters: Arc::new(Counters::default()),
                deafened: Arc::new(AtomicBool::new(false)),
                errors: monitor.errors.clone(),
                controls: monitor.controls.clone(),
                tones: Arc::new(Mutex::new(VecDeque::new())),
                stop: monitor.stop.clone(),
            },
        )
    }

    #[test]
    fn microphone_preview_uses_capture_processing_and_output_volume_locally() {
        let mut settings = settings();
        settings.microphone_gain = 2.0;
        let mut monitor = monitor(&settings);
        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Default,
        };
        let counters = Arc::new(Counters::default());
        let level = Arc::new(AtomicU32::new(0));
        let mut capture = input_callback::<f32>(
            &config,
            InputState {
                queue: monitor.capture.clone(),
                level: level.clone(),
                muted: Arc::new(AtomicBool::new(false)),
                deafened: Arc::new(AtomicBool::new(false)),
                counters: counters.clone(),
                errors: monitor.errors.clone(),
                controls: monitor.controls.clone(),
                stop: monitor.stop.clone(),
            },
        );
        capture(&[0.1; 1920]);
        assert!(monitor.step());
        assert!(f32::from_bits(level.load(Ordering::Relaxed)) > 0.0);
        assert!(monitor
            .playback
            .lock()
            .unwrap()
            .iter()
            .all(|sample| (*sample - 0.2).abs() < 0.00001));
        let mut render = render(&monitor);
        let mut audible = [0.0; 480];
        let mut heard = false;
        for _ in 0..4 {
            render(&mut audible);
            heard |= audible.iter().any(|sample| sample.abs() > 0.01);
        }
        assert!(
            heard,
            "processed microphone reaches the actual render callback"
        );
        monitor
            .controls
            .playback_volume
            .store(0.0f32.to_bits(), Ordering::Release);
        assert!(
            monitor.step(),
            "fresh captured PCM must be ready for the zero-volume check"
        );
        for _ in 0..3 {
            render(&mut audible);
            assert!(audible.iter().all(|sample| *sample == 0.0));
        }
        assert_eq!(counters.sent.load(Ordering::Relaxed), 0);
        assert_eq!(counters.received.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn microphone_preview_ptt_release_discards_queued_audio_and_render_history() {
        let mut settings = settings();
        settings.transmission = TransmissionMode::PushToTalk;
        settings.whisper_enabled = true;
        let mut monitor = monitor(&settings);
        assert!(!monitor.controls.whisper_enabled.load(Ordering::Acquire));
        monitor.capture.lock().unwrap().extend([0.3; 960]);
        assert!(!monitor.step());
        assert!(monitor.capture.lock().unwrap().is_empty());
        monitor.controls.ptt_pressed.store(true, Ordering::Release);
        monitor
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        monitor.step();
        monitor.capture.lock().unwrap().extend([0.3; 960]);
        assert!(monitor.step());
        let mut render = render(&monitor);
        let mut output = [0.0; 240];
        let mut heard = false;
        for _ in 0..2 {
            render(&mut output);
            heard |= output.iter().any(|sample| sample.abs() > 0.01);
        }
        assert!(
            heard,
            "establish audible render history before testing release"
        );
        monitor.controls.ptt_pressed.store(false, Ordering::Release);
        monitor
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        assert!(!monitor.step());
        // Re-press before an output callback: its filter history must still reset.
        monitor.controls.ptt_pressed.store(true, Ordering::Release);
        monitor
            .controls
            .privacy_generation
            .fetch_add(1, Ordering::AcqRel);
        monitor.step();
        render(&mut output);
        assert!(output.iter().all(|sample| *sample == 0.0));
        monitor.capture.lock().unwrap().extend([0.1; 960]);
        assert!(monitor.step());
        monitor.stop.store(true, Ordering::Release);
        render(&mut output);
        assert!(
            output.iter().all(|sample| *sample == 0.0),
            "Stop silences pending playback"
        );
        assert!(!monitor.step());
        assert!(monitor.capture.lock().unwrap().is_empty());
        assert!(monitor.playback.lock().unwrap().is_empty());
    }

    #[test]
    fn microphone_preview_hears_live_edits_on_the_next_block_without_a_reset() {
        let mut monitor = monitor(&settings());
        monitor.capture.lock().unwrap().extend([0.1; 480]);
        assert!(monitor.step());
        let first = monitor.taps.take();
        assert!((first.raw_peak - 0.1).abs() < 1e-6);
        assert!((first.peak - 0.1).abs() < 1e-6);
        assert!(first.gate_open);
        let generation = monitor.generation;
        let mut edited = settings();
        edited.microphone_gain = 3.0;
        monitor.controls.apply_preview(&edited);
        assert_eq!(monitor.controls.privacy_generation.load(Ordering::Acquire), generation);
        monitor.playback.lock().unwrap().clear();
        monitor.capture.lock().unwrap().extend([0.1; 480]);
        assert!(monitor.step());
        assert_eq!(monitor.generation, generation, "no processor reset for a gain drag");
        let louder = monitor.taps.take();
        assert!((louder.raw_peak - 0.1).abs() < 1e-6, "raw level is measured before gain");
        assert!((louder.peak - 0.3).abs() < 1e-5);
        assert!(monitor.playback.lock().unwrap().iter().all(|sample| (*sample - 0.3).abs() < 1e-5));
        // Switching to push-to-talk is a privacy boundary: queued audio is dropped,
        // yet the raw meter still shows that the microphone works.
        edited.transmission = TransmissionMode::PushToTalk;
        monitor.controls.apply_preview(&edited);
        assert!(monitor.controls.privacy_generation.load(Ordering::Acquire) > generation);
        monitor.capture.lock().unwrap().extend([0.2; 480]);
        assert!(!monitor.step(), "the boundary itself discards queued speech");
        assert!(monitor.capture.lock().unwrap().is_empty());
        monitor.capture.lock().unwrap().extend([0.2; 480]);
        assert!(!monitor.step());
        assert!(monitor.capture.lock().unwrap().is_empty());
        let waiting = monitor.taps.take();
        assert!((waiting.raw_peak - 0.2).abs() < 1e-6);
        assert_eq!(waiting.peak, 0.0, "nothing is processed while the key is up");
        assert!(monitor.playback.lock().unwrap().is_empty(), "nothing is played while the key is up");
    }

    #[test]
    fn microphone_preview_honors_gate_and_dsp_blocks_with_bounded_playback() {
        let mut settings = settings();
        settings.transmission = TransmissionMode::VoiceActivation;
        settings.automatic_sensitivity = Some(false);
        settings.activation_threshold = 0.2;
        settings.activation_hold_ms = 0;
        let mut gated = monitor(&settings);
        gated.capture.lock().unwrap().extend([0.01; 480]);
        assert!(gated.step());
        assert!(gated.playback.lock().unwrap().is_empty());
        settings.transmission = TransmissionMode::Continuous;
        settings.set_packet_samples(120);
        settings.noise_suppression = true;
        let mut dsp = monitor(&settings);
        dsp.capture.lock().unwrap().extend([0.01; 120]);
        assert!(
            !dsp.step(),
            "RNNoise requires a complete 10 ms processing frame"
        );
        dsp.capture.lock().unwrap().extend([0.01; 360]);
        assert!(dsp.step());
        for _ in 0..40 {
            dsp.capture.lock().unwrap().extend([0.01; 480]);
            assert!(dsp.step());
        }
        assert!(dsp.playback.lock().unwrap().len() <= MAX_PLAYBACK_SAMPLES);
    }
}
