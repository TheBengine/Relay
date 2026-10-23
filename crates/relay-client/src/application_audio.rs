//! Bounded per-track application-audio mixing into an already opened voice output.
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};
const MAX_TRACKS: usize = 16;
const MAX_PEER_VOLUMES: usize = 64;

struct ApplicationAudioStream {
    user_id: String,
    volume: f32,
    samples: VecDeque<[f32; 2]>,
}
// Default 10 ms decoded frames and normal callbacks retain the 40 ms bound.
const MAX_SAMPLES: usize = 960 * 2;
// Device demand is learned without adding a wait; large/default driver periods
// need a whole render block, one frame of headroom and room for the next
// decoded frame. This is capacity, not an additional startup wait.
pub(crate) const MAX_DEVICE_SAMPLES: usize = 48_000 / 5;
#[derive(Default)]
pub(crate) struct ApplicationAudioQueues {
    streams: HashMap<String, ApplicationAudioStream>,
    // Keep preferences across track retirement/deafen/reconnect, just as voice
    // volume does. Both this map and the active track map have hard bounds.
    peer_volumes: HashMap<String, f32>,
    device_samples: usize,
    decoded_samples: usize,
    primed: bool,
    waited_samples: usize,
    generation: u64,
    #[cfg(test)]
    dropped_samples: usize,
}
impl ApplicationAudioQueues {
    fn sample_limit(&self, device_samples: usize) -> usize {
        device_samples
            .saturating_add(self.decoded_samples.saturating_mul(2))
            .clamp(MAX_SAMPLES, MAX_DEVICE_SAMPLES)
    }
    pub(crate) fn prepare_block(&mut self, samples: usize) -> bool {
        self.device_samples = self.device_samples.max(samples.min(MAX_DEVICE_SAMPLES));
        // A single startup/catch-up block must not leave stale sound queued when
        // the device returns to short periods. No allocation or producer wait.
        let current_limit = self.sample_limit(samples);
        for stream in self.streams.values_mut() {
            let queue = &mut stream.samples;
            let excess = queue.len().saturating_sub(current_limit);
            queue.drain(..excess);
            #[cfg(test)]
            {
                self.dropped_samples += excess;
            }
        }
        let available = self
            .streams
            .values()
            .map(|s| s.samples.len())
            .max()
            .unwrap_or(0);
        if available == 0 {
            self.primed = false;
            self.waited_samples = 0;
            return false;
        }
        // Decoded frames are burstier than the2.5ms voice mixer. Retain enough
        // startup headroom for that burst boundary, but wait at most5ms worth
        // of callbacks before playing even a short final sound. No rate change.
        if !self.primed
            && available < samples.saturating_add(self.decoded_samples.saturating_mul(2))
            && self.waited_samples < 240
        {
            self.waited_samples = self.waited_samples.saturating_add(samples);
            return false;
        }
        self.primed = true;
        self.waited_samples = 0;
        true
    }
    #[cfg(test)]
    pub(crate) fn next_sample(&mut self) -> f32 {
        self.pop_frame(false).map_or(0.0, |frame| frame[0])
    }
    /// Consume one aligned stereo frame per track. Non-stereo devices retain
    /// the existing per-track mono downmix before the bounded room mix.
    pub(crate) fn pop_frame(&mut self, stereo: bool) -> Option<[f32; 2]> {
        let mut mixed = [0.0_f32; 2];
        let mut present = false;
        for stream in self.streams.values_mut() {
            if let Some(frame) = stream.samples.pop_front() {
                present = true;
                let frame = if stereo {
                    frame
                } else {
                    [(frame[0] + frame[1]) * 0.5; 2]
                };
                for (sum, value) in mixed.iter_mut().zip(frame) {
                    *sum += value * stream.volume;
                }
            }
        }
        present.then(|| mixed.map(|value| value.clamp(-1.0, 1.0)))
    }
    pub(crate) fn clock_state(&self) -> (u64, usize, usize) {
        (
            self.generation,
            self.streams
                .values()
                .map(|s| s.samples.len())
                .max()
                .unwrap_or(0),
            self.decoded_samples,
        )
    }
    #[cfg(test)]
    pub(crate) fn dropped_samples(&self) -> usize {
        self.dropped_samples
    }
    pub(crate) fn clear(&mut self) {
        self.streams.clear();
        self.generation = self.generation.wrapping_add(1);
        self.device_samples = 0;
        self.decoded_samples = 0;
        self.primed = false;
        self.waited_samples = 0;
    }
}
#[derive(Clone, Default)]
pub struct ApplicationAudioSink(Arc<Mutex<ApplicationAudioQueues>>);
impl ApplicationAudioSink {
    pub(crate) fn set_user_volume(&self, user_id: &str, volume: f32) {
        if user_id.is_empty() || user_id.len() > 128 {
            return;
        }
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 2.0)
        } else {
            1.0
        };
        if let Ok(mut queues) = self.0.lock() {
            if volume == 1.0 {
                queues.peer_volumes.remove(user_id);
            } else if queues.peer_volumes.contains_key(user_id)
                || queues.peer_volumes.len() < MAX_PEER_VOLUMES
            {
                queues.peer_volumes.insert(user_id.into(), volume);
            } else {
                return;
            }
            for stream in queues.streams.values_mut().filter(|s| s.user_id == user_id) {
                stream.volume = volume;
            }
        }
    }
    #[cfg(test)]
    pub fn push_guarded(&self, key: &str, pcm: &[i16], stop: &AtomicBool) {
        self.push_peer_guarded(key, key, pcm, stop);
    }
    /// Only decoded RTC application tracks enter here; this never opens a device.
    /// The owner comes from the verified participant, separately from the
    /// connection-scoped track key. Never infer an account by parsing that key.
    pub fn push_peer_guarded(&self, key: &str, user_id: &str, pcm: &[i16], stop: &AtomicBool) {
        if key.len() > 256
            || user_id.is_empty()
            || user_id.len() > 128
            || pcm.len() > 960 * 2 * 4
            || !pcm.len().is_multiple_of(2)
        {
            return;
        }
        // The decoded-track worker may wait for a short render/cleanup section;
        // dropping here loses an entire decoded frame. The real-time output
        // callback still uses try_queues, so it never waits for this producer.
        if let Ok(mut queues) = self.0.lock() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if !queues.streams.contains_key(key) && queues.streams.len() >= MAX_TRACKS {
                return;
            }
            if queues
                .streams
                .get(key)
                .is_some_and(|s| s.user_id != user_id)
            {
                return;
            }
            queues.decoded_samples = queues.decoded_samples.max(pcm.len() / 2);
            let limit = queues.sample_limit(queues.device_samples);
            let volume = queues.peer_volumes.get(user_id).copied().unwrap_or(1.0);
            let stream =
                queues
                    .streams
                    .entry(key.into())
                    .or_insert_with(|| ApplicationAudioStream {
                        user_id: user_id.into(),
                        volume,
                        samples: VecDeque::new(),
                    });
            let queue = &mut stream.samples;
            #[cfg(test)]
            let mut dropped = 0;
            for frame in pcm.chunks_exact(2) {
                if queue.len() == limit {
                    queue.pop_front();
                    #[cfg(test)]
                    {
                        dropped += 1;
                    }
                }
                queue.push_back([frame[0] as f32 / 32768.0, frame[1] as f32 / 32768.0]);
            }
            #[cfg(test)]
            {
                queues.dropped_samples += dropped;
            }
        }
    }
    pub fn clear_stream(&self, key: &str) {
        if let Ok(mut queues) = self.0.lock() {
            if queues.streams.remove(key).is_some() {
                queues.generation = queues.generation.wrapping_add(1);
            }
            if queues.streams.is_empty() {
                queues.decoded_samples = 0;
                queues.primed = false;
                queues.waited_samples = 0;
            }
        }
    }
    pub fn clear(&self) {
        if let Ok(mut queues) = self.0.lock() {
            queues.clear();
        }
    }
    pub(crate) fn try_queues(&self) -> Option<MutexGuard<'_, ApplicationAudioQueues>> {
        self.0.try_lock().ok()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn peer_gain_applies_to_queued_and_future_tracks_without_changing_other_people() {
        let sink = ApplicationAudioSink::default();
        let stop = AtomicBool::new(false);
        sink.set_user_volume("alice", 0.5);
        sink.push_peer_guarded(
            "connection-1:screen",
            "alice",
            &[8192, -4096].repeat(16),
            &stop,
        );
        sink.push_peer_guarded("connection-1:other", "bob", &[4096, 8192].repeat(16), &stop);
        assert_eq!(
            sink.try_queues().unwrap().pop_frame(true),
            Some([0.25, 0.1875])
        );
        sink.set_user_volume("alice", 0.0);
        assert_eq!(
            sink.try_queues().unwrap().pop_frame(true),
            Some([0.125, 0.25])
        );
        sink.set_user_volume("alice", 2.0);
        assert_eq!(
            sink.try_queues().unwrap().pop_frame(true),
            Some([0.625, 0.0])
        );
        sink.clear_stream("connection-1:screen");
        sink.push_peer_guarded("connection-2:screen", "alice", &[8192, -4096], &stop);
        assert_eq!(
            sink.try_queues().unwrap().pop_frame(true),
            Some([0.625, 0.0])
        );
        sink.clear();
        sink.push_peer_guarded(
            "connection-3:screen",
            "alice",
            &[8192, -4096].repeat(4),
            &stop,
        );
        assert_eq!(
            sink.try_queues().unwrap().pop_frame(false),
            Some([0.125; 2])
        );
        sink.set_user_volume("alice", f32::NAN);
        assert_eq!(
            sink.try_queues().unwrap().pop_frame(true),
            Some([0.25, -0.125])
        );
        sink.set_user_volume("alice", -10.0);
        assert_eq!(sink.try_queues().unwrap().pop_frame(true), Some([0.0; 2]));
        sink.set_user_volume("alice", 100.0);
        assert_eq!(
            sink.try_queues().unwrap().pop_frame(true),
            Some([0.5, -0.25])
        );
    }

    #[test]
    fn peer_gain_is_bound_to_verified_owner_and_preference_storage_is_bounded() {
        let sink = ApplicationAudioSink::default();
        let stop = AtomicBool::new(false);
        sink.set_user_volume("alice", 0.0);
        sink.push_peer_guarded("arbitrary-track-name", "alice", &[8192, 8192], &stop);
        sink.push_peer_guarded("arbitrary-track-name", "bob", &[8192, 8192], &stop);
        let mut queues = sink.try_queues().unwrap();
        assert_eq!(queues.streams["arbitrary-track-name"].samples.len(), 1);
        assert_eq!(queues.pop_frame(true), Some([0.0; 2]));
        drop(queues);
        for index in 0..200 {
            sink.set_user_volume(&format!("person-{index}"), 0.5);
        }
        sink.set_user_volume("", 0.0);
        sink.set_user_volume(&"x".repeat(129), 0.0);
        assert_eq!(
            sink.try_queues().unwrap().peer_volumes.len(),
            MAX_PEER_VOLUMES
        );
        sink.set_user_volume("person-0", 1.0);
        sink.set_user_volume("new-person", 0.0);
        sink.push_peer_guarded("new-track", "new-person", &[8192, 8192], &stop);
        assert_eq!(sink.try_queues().unwrap().pop_frame(true), Some([0.0; 2]));
    }

    #[test]
    fn a_busy_renderer_does_not_discard_an_entire_decoded_frame() {
        let sink = ApplicationAudioSink::default();
        let queues = sink.try_queues().unwrap();
        let producer = sink.clone();
        let (entered, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            entered.send(()).unwrap();
            producer.push_guarded("screen", &[8192; 960], &AtomicBool::new(false));
            finished.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let returned_while_busy = done.recv_timeout(Duration::from_millis(20)).is_ok();
        drop(queues);
        if !returned_while_busy {
            done.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        worker.join().unwrap();
        let mut queues = sink.try_queues().unwrap();
        let played: Vec<_> = (0..480).map(|_| queues.next_sample()).collect();
        println!("Owned decoded-frame/render overlap: producer returned while busy={returned_while_busy}; preserved samples={}", played.iter().filter(|sample| **sample == 0.25).count());
        assert!(
            played.iter().all(|sample| *sample == 0.25),
            "A busy render queue discarded a complete 10 ms decoded frame"
        );
        assert_eq!(queues.next_sample(), 0.0);
    }

    #[test]
    fn stop_during_render_contention_does_not_republish_cancelled_audio() {
        let sink = ApplicationAudioSink::default();
        let queues = sink.try_queues().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let producer = sink.clone();
        let producer_stop = stop.clone();
        let (entered, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            entered.send(()).unwrap();
            producer.push_guarded("retired-screen", &[8192; 960], &producer_stop);
            finished.send(()).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let returned_while_busy = done.recv_timeout(Duration::from_millis(20)).is_ok();
        stop.store(true, Ordering::Release);
        drop(queues);
        if !returned_while_busy {
            done.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        worker.join().unwrap();
        assert!(sink.try_queues().unwrap().streams.is_empty());
    }

    #[test]
    fn device_demand_and_catchup_are_bounded_and_cleanup_resets_learning() {
        let sink = ApplicationAudioSink::default();
        let stop = AtomicBool::new(false);
        sink.push_guarded("screen", &[8192; 960], &stop);
        {
            let mut queues = sink.try_queues().unwrap();
            queues.prepare_block(240);
            assert_eq!(queues.sample_limit(queues.device_samples), MAX_SAMPLES);
            queues.prepare_block(4800);
        }
        for _ in 0..100 {
            sink.push_guarded("screen", &[-8192; 960], &stop);
        }
        {
            let mut queues = sink.try_queues().unwrap();
            assert_eq!(queues.streams["screen"].samples.len(), 5760);
            assert!(queues.streams["screen"].samples.len() <= MAX_DEVICE_SAMPLES);
            queues.prepare_block(240);
            assert_eq!(queues.streams["screen"].samples.len(), MAX_SAMPLES);
            queues.prepare_block(usize::MAX);
        }
        for _ in 0..100 {
            sink.push_guarded("screen", &[8192; 960], &stop);
        }
        {
            let mut queues = sink.try_queues().unwrap();
            assert_eq!(queues.streams["screen"].samples.len(), MAX_DEVICE_SAMPLES);
            queues.clear();
            assert_eq!(queues.device_samples, 0);
            assert_eq!(queues.decoded_samples, 0);
            assert_eq!(queues.sample_limit(queues.device_samples), MAX_SAMPLES);
        }
        for _ in 0..100 {
            sink.push_guarded("fresh", &[8192; 960], &stop);
        }
        assert_eq!(
            sink.try_queues().unwrap().streams["fresh"].samples.len(),
            MAX_SAMPLES
        );
        sink.clear_stream("fresh");
        assert_eq!(sink.try_queues().unwrap().decoded_samples, 0);
    }

    #[test]
    fn a_slow_audio_device_receives_recent_application_audio() {
        let sink = ApplicationAudioSink::default();
        let stop = AtomicBool::new(false);
        for _ in 0..10 {
            sink.push_guarded("screen", &[8192; 1920], &stop);
        }
        for _ in 0..2 {
            sink.push_guarded("screen", &[-8192; 1920], &stop);
        }
        let mut queues = sink.try_queues().unwrap();
        for _ in 0..1920 {
            assert!((queues.next_sample() + 0.25).abs() < 0.001);
        }
        assert_eq!(queues.next_sample(), 0.0);
    }
    #[test]
    fn remote_tracks_are_mixed_and_room_cleanup_has_hard_bounds() {
        let sink = ApplicationAudioSink::default();
        let stop = AtomicBool::new(false);
        sink.push_guarded("a", &[8192; 1920], &stop);
        sink.push_guarded("b", &[8192; 1920], &stop);
        assert!((sink.try_queues().unwrap().next_sample() - 0.5).abs() < 0.001);
        sink.clear_stream("a");
        assert!((sink.try_queues().unwrap().next_sample() - 0.25).abs() < 0.001);
        for index in 0..100 {
            for _ in 0..20 {
                sink.push_guarded(&index.to_string(), &[100; 1920], &stop);
            }
        }
        let queues = sink.try_queues().unwrap();
        assert_eq!(queues.streams.len(), MAX_TRACKS);
        assert!(queues
            .streams
            .values()
            .all(|stream| stream.samples.len() <= MAX_SAMPLES));
        drop(queues);
        sink.clear();
        assert!(sink.try_queues().unwrap().streams.is_empty());
    }
}
