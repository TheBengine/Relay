//! SPSC device handoff. A callback owns one rtrb endpoint for its entire lifetime.
//! The control/worker side may borrow the other endpoint; its mutex is never
//! acquired by the callback. Endpoints return only after a stream has stopped.
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

pub(crate) struct Queue {
    state: Mutex<State>,
    written: AtomicUsize,
    read: AtomicUsize,
    trim: AtomicUsize,
}
pub(crate) struct State {
    producer: Option<rtrb::Producer<f32>>,
    consumer: Option<rtrb::Consumer<f32>>,
    capacity: usize,
}
impl Queue {
    pub(crate) fn new(capacity: usize) -> Self {
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        Self {
            state: Mutex::new(State {
                producer: Some(producer),
                consumer: Some(consumer),
                capacity,
            }),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            trim: AtomicUsize::new(0),
        }
    }
    pub(crate) fn from_samples(samples: VecDeque<f32>, capacity: usize) -> Self {
        let queue = Self::new(capacity.max(samples.len()));
        queue.lock().unwrap().extend(samples);
        queue
    }
    pub(crate) fn lock(&self) -> Result<Guard<'_>, ()> {
        self.state
            .lock()
            .map(|state| Guard::new(state, self))
            .map_err(|_| ())
    }
    pub(crate) fn try_lock(&self) -> Result<Guard<'_>, ()> {
        self.state
            .try_lock()
            .map(|state| Guard::new(state, self))
            .map_err(|_| ())
    }
    pub(crate) fn producer(self: &Arc<Self>) -> Producer {
        let endpoint = self
            .state
            .lock()
            .unwrap()
            .producer
            .take()
            .expect("one device producer");
        Producer {
            endpoint: Some(endpoint),
            owner: self.clone(),
        }
    }
    pub(crate) fn consumer(self: &Arc<Self>) -> Consumer {
        let endpoint = self
            .state
            .lock()
            .unwrap()
            .consumer
            .take()
            .expect("one device consumer");
        Consumer {
            endpoint: Some(endpoint),
            owner: self.clone(),
        }
    }
}
pub(crate) struct Producer {
    endpoint: Option<rtrb::Producer<f32>>,
    owner: Arc<Queue>,
}
impl Producer {
    pub(crate) fn push_bounded(&mut self, value: f32, limit: usize) -> bool {
        let producer = self.endpoint.as_mut().unwrap();
        if producer.buffer().capacity() - producer.slots() >= limit {
            return false;
        }
        if producer.push(value).is_err() {
            return false;
        }
        self.owner.written.fetch_add(1, Ordering::Release);
        true
    }
    pub(crate) fn request_clear(&self) {
        self.owner
            .trim
            .fetch_max(self.owner.written.load(Ordering::Acquire), Ordering::AcqRel);
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        self.owner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .producer = self.endpoint.take();
    }
}
pub(crate) struct Consumer {
    endpoint: Option<rtrb::Consumer<f32>>,
    owner: Arc<Queue>,
}
impl Consumer {
    pub(crate) fn synchronize(&mut self) {
        let consumer = self.endpoint.as_mut().unwrap();
        let count = self
            .owner
            .trim
            .load(Ordering::Acquire)
            .saturating_sub(self.owner.read.load(Ordering::Acquire))
            .min(consumer.slots());
        for _ in 0..count {
            let _ = consumer.pop();
        }
        self.owner.read.fetch_add(count, Ordering::Release);
    }
    pub(crate) fn len(&self) -> usize {
        self.endpoint.as_ref().unwrap().slots()
    }
    pub(crate) fn clear(&mut self) {
        let consumer = self.endpoint.as_mut().unwrap();
        let count = consumer.slots();
        for _ in 0..count {
            let _ = consumer.pop();
        }
        self.owner.read.fetch_add(count, Ordering::Release);
    }
    pub(crate) fn pop_front(&mut self) -> Option<f32> {
        let value = self.endpoint.as_mut().unwrap().pop().ok()?;
        self.owner.read.fetch_add(1, Ordering::Release);
        Some(value)
    }
    pub(crate) fn drain(&mut self, range: std::ops::RangeTo<usize>) {
        self.synchronize();
        for _ in 0..range.end {
            if self.pop_front().is_none() {
                break;
            }
        }
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        self.owner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .consumer = self.endpoint.take();
    }
}
pub(crate) struct Guard<'a> {
    state: MutexGuard<'a, State>,
    owner: &'a Queue,
    #[cfg(test)]
    snapshot: Vec<f32>,
}
impl<'a> Guard<'a> {
    fn new(state: MutexGuard<'a, State>, owner: &'a Queue) -> Self {
        let mut result = Self {
            state,
            owner,
            #[cfg(test)]
            snapshot: Vec::new(),
        };
        result.synchronize();
        result
    }
    fn synchronize(&mut self) {
        if let Some(consumer) = self.state.consumer.as_mut() {
            let count = self
                .owner
                .trim
                .load(Ordering::Acquire)
                .saturating_sub(self.owner.read.load(Ordering::Acquire))
                .min(consumer.slots());
            for _ in 0..count {
                let _ = consumer.pop();
            }
            self.owner.read.fetch_add(count, Ordering::Release);
        }
    }
    pub(crate) fn len(&self) -> usize {
        let available = if let Some(consumer) = self.state.consumer.as_ref() {
            consumer.slots()
        } else if let Some(producer) = self.state.producer.as_ref() {
            self.state.capacity - producer.slots()
        } else {
            0
        };
        available.saturating_sub(
            self.owner
                .trim
                .load(Ordering::Acquire)
                .saturating_sub(self.owner.read.load(Ordering::Acquire)),
        )
    }
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.state.capacity
    }
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(crate) fn clear(&mut self) {
        if let Some(consumer) = self.state.consumer.as_mut() {
            let count = consumer.slots();
            for _ in 0..count {
                let _ = consumer.pop();
            }
            self.owner.read.fetch_add(count, Ordering::Release);
        } else {
            self.owner
                .trim
                .fetch_max(self.owner.written.load(Ordering::Acquire), Ordering::AcqRel);
        }
    }
    pub(crate) fn pop_front(&mut self) -> Option<f32> {
        let value = self.state.consumer.as_mut()?.pop().ok()?;
        self.owner.read.fetch_add(1, Ordering::Release);
        Some(value)
    }
    pub(crate) fn push_back(&mut self, value: f32) {
        if let Some(producer) = self.state.producer.as_mut() {
            if producer.push(value).is_ok() {
                self.owner.written.fetch_add(1, Ordering::Release);
            }
        }
    }
    pub(crate) fn extend(&mut self, values: impl IntoIterator<Item = f32>) {
        for value in values {
            self.push_back(value);
        }
    }
    #[cfg(test)]
    pub(crate) fn drain(
        &mut self,
        range: impl std::ops::RangeBounds<usize>,
    ) -> std::vec::IntoIter<f32> {
        use std::ops::Bound;
        let count = match range.end_bound() {
            Bound::Excluded(n) => *n,
            Bound::Included(n) => n + 1,
            Bound::Unbounded => self.len(),
        };
        if self.state.consumer.is_none() {
            self.owner.trim.fetch_add(count, Ordering::AcqRel);
            Vec::new().into_iter()
        } else {
            (0..count)
                .filter_map(|_| self.pop_front())
                .collect::<Vec<_>>()
                .into_iter()
        }
    }
    pub(crate) fn append_bounded(&mut self, samples: &[f32], limit: usize) -> bool {
        let before = self.len();
        let written = self.owner.written.load(Ordering::Acquire);
        self.extend(samples.iter().copied());
        let after = self.owner.written.load(Ordering::Acquire);
        // Absolute positions prevent a concurrent callback from discarding newer
        // speech that was not part of the original excess.
        self.owner
            .trim
            .fetch_max(after.saturating_sub(limit), Ordering::AcqRel);
        self.synchronize();
        before + samples.len() > limit || after - written < samples.len()
    }
    #[cfg(test)]
    pub(crate) fn iter(&mut self) -> std::slice::Iter<'_, f32> {
        self.synchronize();
        self.snapshot.clear();
        if let Some(consumer) = self.state.consumer.as_mut() {
            if let Ok(chunk) = consumer.read_chunk(consumer.slots()) {
                let (a, b) = chunk.as_slices();
                self.snapshot.extend_from_slice(a);
                self.snapshot.extend_from_slice(b);
            }
        }
        self.snapshot.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn five_seconds_of_ten_ms_blocks_preserve_order_across_consumer_pauses() {
        let queue = Arc::new(Queue::new(480 * 16));
        let mut producer = queue.producer();
        let mut consumer = queue.consumer();
        let writer = std::thread::spawn(move || {
            for block in 0..500 {
                for sample in 0..480 {
                    assert!(producer.push_bounded((block * 480 + sample) as f32, 480 * 16));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        let mut received = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while received < 500 * 480 {
            assert!(
                std::time::Instant::now() < deadline,
                "Producer stopped or audio was lost at {received}"
            );
            while let Some(sample) = consumer.pop_front() {
                assert_eq!(sample, received as f32);
                received += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis((received as u64 % 17) + 1));
        }
        writer.join().unwrap();
    }
    #[test]
    fn concurrent_playout_and_clear_preserve_newer_samples() {
        let queue = Arc::new(Queue::new(16));
        let mut consumer = queue.consumer();
        queue.lock().unwrap().extend([1.0, 2.0, 3.0, 4.0]);
        assert_eq!(consumer.pop_front(), Some(1.0));
        assert!(queue.lock().unwrap().append_bounded(&[5.0, 6.0], 3));
        assert_eq!(consumer.pop_front(), Some(2.0));
        consumer.synchronize();
        assert_eq!(consumer.pop_front(), Some(4.0));
        queue.lock().unwrap().clear();
        queue.lock().unwrap().extend([7.0, 8.0]);
        consumer.synchronize();
        assert_eq!(consumer.pop_front(), Some(7.0));
        assert_eq!(consumer.pop_front(), Some(8.0));
    }

    #[test]
    fn device_producer_never_takes_the_consumer_mutex_and_only_excess_is_dropped() {
        let queue = Arc::new(Queue::new(4));
        let mut producer = queue.producer();
        let mut consumer = queue.lock().unwrap();
        assert!(producer.push_bounded(1.0, 2));
        assert!(producer.push_bounded(2.0, 2));
        assert!(!producer.push_bounded(3.0, 2));
        assert_eq!(consumer.pop_front(), Some(1.0));
        assert!(producer.push_bounded(4.0, 2));
        assert_eq!(consumer.drain(..).collect::<Vec<_>>(), [2.0, 4.0]);
    }
}
