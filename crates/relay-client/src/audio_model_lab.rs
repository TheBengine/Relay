//! Bounded model comparisons. One worker per audio controls, shared across
//! model switches; old generations/results never become audible. No fallback
//! denoiser is substituted for the model being compared.
use crate::noise_suppressor::{Suppressor, BLOCK};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{sync_channel, Receiver, SyncSender},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Model {
    External(relay_audio_models::Model),
    Dpdf(crate::dpdfnet::Variant),
}
impl Model {
    pub(crate) fn delay_samples(self) -> usize {
        match self {
            Self::External(model) => model.delay_samples(),
            Self::Dpdf(_) => 2400,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::External(relay_audio_models::Model::DeepFilter3) => "DeepFilterNet 3",
            Self::External(relay_audio_models::Model::DeepFilter3LowLatency) => {
                "DeepFilterNet 3 Low Latency"
            }
            Self::External(relay_audio_models::Model::Gtcrn) => "GTCRN",
            Self::External(relay_audio_models::Model::RnNoiseNew) => "RNNoise (newer model)",
            Self::Dpdf(crate::dpdfnet::Variant::Eight) => "DPDFNet 8",
            Self::Dpdf(crate::dpdfnet::Variant::Two) => "DPDFNet 2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Status {
    Idle,
    Preparing,
    Warming,
    Active,
    Behind,
    Unavailable,
}
impl Status {
    pub(crate) fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Preparing,
            2 => Self::Warming,
            3 => Self::Active,
            4 => Self::Behind,
            5 => Self::Unavailable,
            _ => Self::Idle,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Idle => "Start Listen to Myself to hear this model.",
            Self::Preparing => {
                "Preparing the selected model… The test is silent until it is ready."
            }
            Self::Warming => "Warming up the selected model…",
            Self::Active => "Playing the selected model.",
            Self::Behind => {
                "This model is falling behind. Late audio is skipped; try a lighter model."
            }
            Self::Unavailable => {
                "This model could not run. Choose another model to continue the test."
            }
        }
    }
}

enum Engine {
    External(relay_audio_models::Stream),
    Dpdf(Box<crate::dpdfnet::Stream>),
}
impl Engine {
    fn new(model: Model) -> Result<Self, String> {
        match model {
            Model::External(model) => relay_audio_models::Stream::new(model).map(Self::External),
            Model::Dpdf(variant) => crate::dpdfnet::load_blocking(variant)
                .map(|model| Self::Dpdf(Box::new(crate::dpdfnet::Stream::new(model, variant)))),
        }
    }
    fn process(&mut self, block: &mut [f32; BLOCK]) -> Result<(), String> {
        match self {
            Self::External(stream) => stream.process(block),
            Self::Dpdf(stream) => stream.process(block),
        }
    }
}

struct Job {
    epoch: u64,
    number: u64,
    model: Model,
    captured: Instant,
    max_age: Duration,
    samples: [f32; BLOCK],
}
struct Done {
    epoch: u64,
    number: u64,
    samples: [f32; BLOCK],
    status: Status,
}

pub(crate) struct Service {
    jobs: SyncSender<Job>,
    results: Mutex<Receiver<Done>>,
    epoch: Arc<AtomicU64>,
    status: Arc<AtomicU64>,
    available: bool,
}
fn tagged(epoch: u64, status: Status) -> u64 {
    (epoch << 3) | status as u64
}
impl Service {
    fn read_status(&self, epoch: u64) -> Status {
        let value = self.status.load(Ordering::Acquire);
        if value >> 3 == epoch {
            Status::from_u32((value & 7) as u32)
        } else {
            Status::Preparing
        }
    }
    pub(crate) fn new() -> Arc<Self> {
        let (jobs, inbox) = sync_channel::<Job>(40);
        let (outbox, results) = sync_channel::<Done>(40);
        let epoch = Arc::new(AtomicU64::new(0));
        let status = Arc::new(AtomicU64::new(0));
        let (worker_epoch, worker_status) = (epoch.clone(), status.clone());
        let spawned = std::thread::Builder::new()
            .name("relay-model-comparison".into())
            .spawn(move || {
                let mut engine: Option<(u64, Model, u64, usize, Engine)> = None;
                let mut failed = None;
                while let Ok(job) = inbox.recv() {
                    if worker_epoch.load(Ordering::Acquire) != job.epoch
                        || job.captured.elapsed() > job.max_age
                    {
                        continue;
                    }
                    if failed == Some((job.epoch, job.model)) {
                        continue;
                    }
                    let reset = engine
                        .as_ref()
                        .is_none_or(|(epoch, model, expected, _, _)| {
                            *epoch != job.epoch || *model != job.model || *expected != job.number
                        });
                    if reset {
                        worker_status
                            .store(tagged(job.epoch, Status::Preparing), Ordering::Release);
                        engine = match Engine::new(job.model) {
                            Ok(engine) => Some((job.epoch, job.model, job.number, 0, engine)),
                            Err(error) => {
                                eprintln!("Voice model {}: {error}", job.model.name());
                                failed = Some((job.epoch, job.model));
                                worker_status.store(
                                    tagged(job.epoch, Status::Unavailable),
                                    Ordering::Release,
                                );
                                None
                            }
                        };
                    }
                    if worker_epoch.load(Ordering::Acquire) != job.epoch
                        || job.captured.elapsed() > job.max_age
                    {
                        engine = None;
                        continue;
                    }
                    let Some((_, _, expected, processed, stream)) = &mut engine else {
                        continue;
                    };
                    let mut samples = job.samples;
                    let result = stream.process(&mut samples);
                    *expected = job.number.wrapping_add(1);
                    *processed += BLOCK;
                    if worker_epoch.load(Ordering::Acquire) != job.epoch {
                        engine = None;
                        continue;
                    }
                    if result.is_err() || samples.iter().any(|v| !v.is_finite()) {
                        failed = Some((job.epoch, job.model));
                        worker_status
                            .store(tagged(job.epoch, Status::Unavailable), Ordering::Release);
                        engine = None;
                        continue;
                    }
                    let status = if *processed <= job.model.delay_samples() {
                        Status::Warming
                    } else {
                        Status::Active
                    };
                    worker_status.store(tagged(job.epoch, status), Ordering::Release);
                    let _ = outbox.try_send(Done {
                        epoch: job.epoch,
                        number: job.number,
                        samples,
                        status,
                    });
                }
            });
        let available = spawned.is_ok();
        Arc::new(Self {
            jobs,
            results: Mutex::new(results),
            epoch,
            status,
            available,
        })
    }
}

pub(crate) struct Comparison {
    service: Arc<Service>,
    model: Model,
    epoch: u64,
    number: u64,
    handoff: u64,
    early: Option<Done>,
    status: Status,
    last_capture: Option<Instant>,
}
impl Comparison {
    pub(crate) fn new(service: Arc<Service>, model: Model, packet_samples: usize) -> Self {
        let epoch = service.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let status = if service.available {
            Status::Preparing
        } else {
            Status::Unavailable
        };
        service
            .status
            .store(tagged(epoch, status), Ordering::Release);
        Self {
            service,
            model,
            epoch,
            number: 0,
            handoff: packet_samples.div_ceil(BLOCK).clamp(1, 20) as u64,
            early: None,
            status,
            last_capture: None,
        }
    }
}
impl Suppressor for Comparison {
    fn process(&mut self, block: &mut [f32; BLOCK]) {
        let now = Instant::now();
        if self.last_capture.is_some_and(|last| {
            now.duration_since(last) > Duration::from_millis((self.handoff * 10 + 50).max(100))
        }) {
            self.epoch = self
                .service
                .epoch
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1);
            self.number = 0;
            self.early = None;
            self.status = Status::Preparing;
            self.service
                .status
                .store(tagged(self.epoch, self.status), Ordering::Release);
        }
        self.last_capture = Some(now);
        let number = self.number;
        self.number = self.number.wrapping_add(1);
        let accepted = match self.service.jobs.try_send(Job {
            epoch: self.epoch,
            number,
            model: self.model,
            captured: now,
            max_age: Duration::from_millis((self.handoff * 10 + 50).max(100)),
            samples: *block,
        }) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => false,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                block.fill(0.0);
                self.status = Status::Unavailable;
                return;
            }
        };
        block.fill(0.0);
        let Some(wanted) = number.checked_sub(self.handoff) else {
            return;
        };
        let mut found = None;
        if let Ok(results) = self.service.results.try_lock() {
            while let Some(result) = self.early.take().or_else(|| results.try_recv().ok()) {
                if result.epoch != self.epoch || result.number < wanted {
                    continue;
                }
                if result.number > wanted {
                    self.early = Some(result);
                    break;
                }
                found = Some(result);
                break;
            }
        }
        if let Some(result) =
            found.filter(|_| self.service.epoch.load(Ordering::Acquire) == self.epoch)
        {
            *block = result.samples;
            self.status = result.status;
        } else {
            self.status = match self.service.read_status(self.epoch) {
                Status::Active | Status::Warming if !accepted || number > self.handoff => {
                    Status::Behind
                }
                status => status,
            };
        }
    }
    fn delay_samples(&self) -> usize {
        self.model.delay_samples() + self.handoff as usize * BLOCK
    }
    fn name(&self) -> &'static str {
        self.model.name()
    }
    fn voice_probability(&self) -> Option<f32> {
        None
    }
    fn status(&self) -> Status {
        self.status
    }
}
impl Drop for Comparison {
    fn drop(&mut self) {
        // Invalidate in-flight work even if a mute/PTT reset is not immediately
        // followed by another model. The service survives rapid model changes.
        let _ = self.service.epoch.compare_exchange(
            self.epoch,
            self.epoch.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// A comparison model run on the calling thread, held back by the same
/// hand-off `Comparison` adds: its output is what the worker returns when it
/// keeps up (test `inline_matches_the_worker_when_it_keeps_up`). For the
/// bench, which needs deterministic output and the worker's time per block.
#[cfg(any(test, feature = "audio-bench"))]
pub(crate) struct Inline {
    model: Model,
    engine: Option<Engine>,
    held: std::collections::VecDeque<[f32; BLOCK]>,
    handoff: usize,
}
#[cfg(any(test, feature = "audio-bench"))]
impl Inline {
    /// `packet_samples`: as for `Comparison::new`.
    pub(crate) fn new(model: Model, packet_samples: usize) -> Self {
        let engine = Engine::new(model)
            .map_err(|error| eprintln!("Voice model {}: {error}", model.name()))
            .ok();
        let handoff = packet_samples.div_ceil(BLOCK).clamp(1, 20);
        Self { model, engine, held: std::collections::VecDeque::with_capacity(handoff + 1), handoff }
    }
}
#[cfg(any(test, feature = "audio-bench"))]
impl Suppressor for Inline {
    fn process(&mut self, block: &mut [f32; BLOCK]) {
        let mut samples = *block;
        // A model that fails stays failed and silent, as on the worker.
        if self.engine.as_mut().is_some_and(|engine| engine.process(&mut samples).is_err()) {
            self.engine = None;
        }
        if self.engine.is_none() {
            samples = [0.0; BLOCK];
        }
        self.held.push_back(samples);
        *block = if self.held.len() > self.handoff { self.held.pop_front().expect("held") } else { [0.0; BLOCK] };
    }
    fn delay_samples(&self) -> usize {
        self.model.delay_samples() + self.handoff * BLOCK
    }
    fn name(&self) -> &'static str {
        self.model.name()
    }
    fn voice_probability(&self) -> Option<f32> {
        None
    }
    fn status(&self) -> Status {
        if self.engine.is_some() { Status::Active } else { Status::Unavailable }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The bench's inline runner gives exactly what `Comparison` plays when
    /// the worker answers every block in time, at the same declared delay.
    #[test]
    fn inline_matches_the_worker_when_it_keeps_up() {
        let speech = crate::voice_test_signals::talker(0.6, (105.0, 135.0), 13, 0.3);
        let gtcrn = Model::External(relay_audio_models::Model::Gtcrn);
        for model in [model(), gtcrn].into_iter().filter(|model| match model {
            Model::External(model) => model.is_built_in(),
            Model::Dpdf(_) => true,
        }) {
            let (service, inbox, outbox) = manual();
            let mut worker = Comparison::new(service, model, BLOCK);
            let mut engine = Engine::new(model).unwrap();
            let mut inline = Inline::new(model, BLOCK);
            assert_eq!(inline.delay_samples(), worker.delay_samples(), "{model:?}");
            for chunk in speech.chunks_exact(BLOCK) {
                let mut via_worker: [f32; BLOCK] = chunk.try_into().unwrap();
                let mut via_inline = via_worker;
                worker.process(&mut via_worker);
                let mut job = inbox.try_recv().unwrap();
                engine.process(&mut job.samples).unwrap();
                outbox
                    .try_send(Done { epoch: job.epoch, number: job.number, samples: job.samples, status: Status::Active })
                    .unwrap();
                inline.process(&mut via_inline);
                assert_eq!(via_inline, via_worker, "{model:?}");
            }
        }
    }
    fn manual() -> (Arc<Service>, Receiver<Job>, SyncSender<Done>) {
        let (jobs, inbox) = sync_channel(40);
        let (outbox, results) = sync_channel(40);
        (
            Arc::new(Service {
                jobs,
                results: Mutex::new(results),
                epoch: Arc::new(AtomicU64::new(0)),
                status: Arc::new(AtomicU64::new(0)),
                available: true,
            }),
            inbox,
            outbox,
        )
    }
    fn model() -> Model {
        Model::External(relay_audio_models::Model::RnNoiseNew)
    }
    #[test]
    fn comparison_handoff_supports_both_packet_sizes_and_never_waits_for_results() {
        for packet in [480, 960] {
            let (service, inbox, outbox) = manual();
            let mut stream = Comparison::new(service, model(), packet);
            for number in 0..12 {
                let mut block = [number as f32 / 20.0; BLOCK];
                stream.process(&mut block);
                if number < packet / BLOCK {
                    assert_eq!(block, [0.0; BLOCK]);
                } else {
                    assert_eq!(block, [(number - packet / BLOCK) as f32 / 20.0; BLOCK]);
                }
                let job = inbox.try_recv().unwrap();
                outbox
                    .try_send(Done {
                        epoch: job.epoch,
                        number: job.number,
                        samples: job.samples,
                        status: Status::Active,
                    })
                    .unwrap();
            }
        }
    }
    #[test]
    fn comparison_switch_discards_old_results_status_and_inflight_private_audio() {
        let (service, inbox, outbox) = manual();
        let mut old = Comparison::new(service.clone(), model(), BLOCK);
        old.process(&mut [0.75; BLOCK]);
        let private = inbox.try_recv().unwrap();
        drop(old);
        assert_ne!(service.epoch.load(Ordering::Acquire), private.epoch);
        let mut new = Comparison::new(service.clone(), model(), BLOCK);
        service.status.store(
            tagged(private.epoch, Status::Unavailable),
            Ordering::Release,
        );
        outbox
            .try_send(Done {
                epoch: private.epoch,
                number: 0,
                samples: private.samples,
                status: Status::Active,
            })
            .unwrap();
        for _ in 0..3 {
            let mut block = [0.0; BLOCK];
            new.process(&mut block);
            assert_eq!(block, [0.0; BLOCK]);
        }
        assert_eq!(new.status(), Status::Preparing);
    }
    #[test]
    fn comparison_batched_callbacks_preserve_every_block_with_capture_sized_handoff() {
        for burst in [2, 4, 20] {
            let (service, inbox, outbox) = manual();
            let mut stream = Comparison::new(service, model(), burst * BLOCK);
            for batch in 0..5 {
                for i in 0..burst {
                    let number = batch * burst + i;
                    let mut block = [(number + 1) as f32 / 100.0; BLOCK];
                    stream.process(&mut block);
                    let expected = if batch == 0 {
                        0.0
                    } else {
                        (number + 1 - burst) as f32 / 100.0
                    };
                    assert_eq!(block, [expected; BLOCK]);
                }
                // Hardware supplies the next burst only after the worker has
                // processed this burst. No worker can run between these calls.
                for job in inbox.try_iter() {
                    outbox
                        .try_send(Done {
                            epoch: job.epoch,
                            number: job.number,
                            samples: job.samples,
                            status: Status::Active,
                        })
                        .unwrap();
                }
            }
        }
    }
    #[test]
    fn comparison_capture_stall_resets_history_and_discards_pre_stall_output() {
        let (service, inbox, outbox) = manual();
        let mut stream = Comparison::new(service.clone(), model(), BLOCK);
        stream.process(&mut [0.75; BLOCK]);
        let old = inbox.try_recv().unwrap();
        outbox
            .try_send(Done {
                epoch: old.epoch,
                number: old.number,
                samples: old.samples,
                status: Status::Active,
            })
            .unwrap();
        stream.last_capture = Some(Instant::now() - Duration::from_secs(1));
        let mut fresh = [0.0; BLOCK];
        stream.process(&mut fresh);
        assert_eq!(fresh, [0.0; BLOCK]);
        let next = inbox.try_recv().unwrap();
        assert_ne!(next.epoch, old.epoch);
        assert_eq!(next.number, 0);
        stream.process(&mut fresh);
        assert_eq!(fresh, [0.0; BLOCK]);
    }
    #[test]
    fn comparison_worker_age_budget_covers_the_full_supported_capture_burst() {
        for burst in [1, 2, 4, 20] {
            let (service, inbox, _outbox) = manual();
            let mut stream = Comparison::new(service, model(), burst * BLOCK);
            for _ in 0..burst {
                stream.process(&mut [0.0; BLOCK]);
            }
            for (index, job) in inbox.try_iter().enumerate() {
                // A 6 ms inference step is faster than real time, including
                // the final block of a 200 ms device callback.
                assert!(Duration::from_millis(index as u64 * 6) < job.max_age);
                assert!(job.max_age <= Duration::from_millis(250));
            }
        }
    }
    #[test]
    fn comparison_regular_200ms_callbacks_do_not_reset_the_stream() {
        let (service, inbox, _outbox) = manual();
        let mut stream = Comparison::new(service, model(), 9600);
        stream.process(&mut [0.0; BLOCK]);
        let first = inbox.try_recv().unwrap();
        stream.last_capture = Some(Instant::now() - Duration::from_millis(200));
        stream.process(&mut [0.0; BLOCK]);
        let next = inbox.try_recv().unwrap();
        assert_eq!(next.epoch, first.epoch);
        assert_eq!(next.number, first.number + 1);
    }
    #[test]
    fn comparison_overload_is_bounded_and_does_not_substitute_unprocessed_audio() {
        let (service, inbox, _outbox) = manual();
        let mut stream = Comparison::new(service.clone(), model(), BLOCK);
        service
            .status
            .store(tagged(stream.epoch, Status::Active), Ordering::Release);
        for _ in 0..1000 {
            let mut block = [0.5; BLOCK];
            stream.process(&mut block);
            assert_eq!(block, [0.0; BLOCK]);
        }
        assert_eq!(inbox.try_iter().count(), 40);
        assert_eq!(stream.status(), Status::Behind);
    }
}
