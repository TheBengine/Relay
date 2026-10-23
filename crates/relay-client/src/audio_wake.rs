//! Capture notifications and socket readiness replace a fixed voice-worker sleep.
use std::{
    net::UdpSocket,
    sync::{Condvar, Mutex},
    time::Duration,
};

#[derive(Default)]
pub(crate) struct Wake {
    pending: Mutex<bool>,
    condition: Condvar,
    #[cfg(windows)]
    event: Option<Event>,
}
impl Wake {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(windows)]
            event: Event::new(false).ok(),
            ..Default::default()
        }
    }
    pub(crate) fn notify(&self) {
        #[cfg(windows)]
        if let Some(event) = &self.event {
            event.signal();
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            *pending = true;
            self.condition.notify_one();
        }
    }
    fn wait_portable(&self, timeout: Duration) {
        if let Ok(mut pending) = self.pending.lock() {
            if !*pending {
                if let Ok((guard, _)) = self.condition.wait_timeout_while(pending, timeout, |p| !*p)
                {
                    pending = guard;
                } else {
                    return;
                }
            }
            *pending = false;
        }
    }
}

#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::Threading::{CreateEventW, SetEvent},
};

#[cfg(windows)]
pub(crate) struct Event(pub(crate) HANDLE);
// Kernel event handles may be signalled and waited on from any thread. This type
// owns the handle; it is closed only after every Arc owner has dropped it.
#[cfg(windows)]
unsafe impl Send for Event {}
#[cfg(windows)]
unsafe impl Sync for Event {}
#[cfg(windows)]
impl Event {
    pub(crate) fn new(manual: bool) -> windows::core::Result<Self> {
        unsafe { CreateEventW(None, manual, false, None).map(Self) }
    }
    pub(crate) fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}
#[cfg(windows)]
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub(crate) struct WorkerWait<'a> {
    wake: &'a Wake,
    #[cfg(windows)]
    socket: &'a UdpSocket,
    #[cfg(windows)]
    network: Option<Event>,
    #[cfg(windows)]
    timer: Option<Event>,
}
impl<'a> WorkerWait<'a> {
    pub(crate) fn new(wake: &'a Wake, socket: &'a UdpSocket) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            use windows::Win32::{
                Networking::WinSock::{WSAEventSelect, FD_READ, SOCKET, WSAEVENT},
                System::Threading::{
                    CreateWaitableTimerExW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
                },
            };
            let network = Event::new(true).ok().filter(|event| unsafe {
                WSAEventSelect(
                    SOCKET(socket.as_raw_socket() as usize),
                    WSAEVENT(event.0 .0 as isize),
                    FD_READ as i32,
                ) == 0
            });
            let timer = unsafe {
                CreateWaitableTimerExW(
                    None,
                    None,
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS.0,
                )
                .ok()
                .map(Event)
            };
            Self {
                wake,
                socket,
                network,
                timer,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = socket;
            Self { wake }
        }
    }
    /// Reset before draining, so a newly arriving datagram remains observable.
    pub(crate) fn begin_receive(&self) {
        #[cfg(windows)]
        if let Some(event) = &self.network {
            use std::os::windows::io::AsRawSocket;
            use windows::Win32::Networking::WinSock::{
                WSAEnumNetworkEvents, SOCKET, WSAEVENT, WSANETWORKEVENTS,
            };
            let mut events = WSANETWORKEVENTS::default();
            unsafe {
                WSAEnumNetworkEvents(
                    SOCKET(self.socket.as_raw_socket() as usize),
                    WSAEVENT(event.0 .0 as isize),
                    &mut events,
                );
            }
        }
    }
    pub(crate) fn wait(&self, mut timeout: Duration) {
        if timeout.is_zero() {
            return;
        }
        #[cfg(windows)]
        if let Some(wake) = &self.wake.event {
            use windows::Win32::System::Threading::{
                SetWaitableTimer, WaitForMultipleObjects, INFINITE,
            };
            // Without a socket event, retain bounded UDP polling as a fallback.
            if self.network.is_none() {
                timeout = timeout.min(Duration::from_millis(2));
            }
            let mut handles = [HANDLE::default(); 3];
            handles[0] = wake.0;
            let mut count = 1;
            if let Some(network) = &self.network {
                handles[count] = network.0;
                count += 1;
            }
            let mut milliseconds = timeout
                .as_millis()
                .saturating_add(u128::from(
                    !timeout.subsec_nanos().is_multiple_of(1_000_000),
                ))
                .min(u128::from(u32::MAX - 1)) as u32;
            if let Some(timer) = &self.timer {
                let due = -(timeout.as_nanos().div_ceil(100).min(i64::MAX as u128) as i64).max(1);
                if unsafe { SetWaitableTimer(timer.0, &due, 0, None, None, false) }.is_ok() {
                    handles[count] = timer.0;
                    count += 1;
                    milliseconds = INFINITE;
                }
            }
            let result = unsafe { WaitForMultipleObjects(&handles[..count], false, milliseconds) };
            if result != windows::Win32::Foundation::WAIT_FAILED {
                return;
            }
        }
        timeout = timeout.min(Duration::from_millis(2));
        self.wake.wait_portable(timeout);
    }
}
#[cfg(windows)]
impl Drop for WorkerWait<'_> {
    fn drop(&mut self) {
        if self.network.is_some() {
            use std::os::windows::io::AsRawSocket;
            use windows::Win32::Networking::WinSock::{WSAEventSelect, SOCKET, WSAEVENT};
            unsafe {
                WSAEventSelect(SOCKET(self.socket.as_raw_socket() as usize), WSAEVENT(0), 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{mpsc, Arc},
        thread,
        time::Instant,
    };
    #[test]
    fn capture_signal_before_wait_is_retained_and_consumed() {
        // Exercise the condition-variable fallback independently of Win32 events.
        let wake = Wake::default();
        wake.notify();
        let at = Instant::now();
        wake.wait_portable(Duration::from_secs(1));
        assert!(at.elapsed() < Duration::from_millis(250));
        assert!(!*wake.pending.lock().unwrap());
    }
    #[test]
    fn capture_interrupts_a_pending_worker_wait() {
        let wake = Arc::new(Wake::new());
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let worker_wake = wake.clone();
        let (ready, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let waiter = WorkerWait::new(&worker_wake, &socket);
            ready.send(()).unwrap();
            waiter.wait(Duration::from_secs(1));
        });
        receiver.recv().unwrap();
        wake.notify();
        worker.join().unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn udp_arrival_wakes_without_audio_and_rearms_after_drain() {
        let wake = Wake::new();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let waiter = WorkerWait::new(&wake, &socket);
        assert!(waiter.network.is_some());
        for byte in [1, 2, 3] {
            waiter.begin_receive();
            sender
                .send_to(&[byte], socket.local_addr().unwrap())
                .unwrap();
            let at = Instant::now();
            waiter.wait(Duration::from_secs(1));
            assert!(at.elapsed() < Duration::from_millis(250));
            let mut bytes = [0; 1];
            assert_eq!(socket.recv_from(&mut bytes).unwrap().0, 1);
            assert_eq!(bytes[0], byte);
        }
    }
}
