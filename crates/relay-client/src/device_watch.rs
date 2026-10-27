//! Windows endpoint notifications (IMMNotificationClient). Callbacks only set
//! flags and ask for a repaint; the UI thread decides what to reopen.
use crate::device_follow::Change;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

const DEFAULT_INPUT: u8 = 1;
const DEFAULT_OUTPUT: u8 = 2;
const DEVICES: u8 = 4;

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Shared between the COM callback and the UI thread.
struct Flags {
    pending: AtomicU8,
    wake: Wake,
}

impl Flags {
    fn raise(&self, bits: u8) {
        if self.pending.fetch_or(bits, Ordering::AcqRel) & bits != bits {
            (self.wake)();
        }
    }
}

pub(crate) struct DeviceWatch {
    flags: Arc<Flags>,
    #[cfg(windows)]
    _registration: platform::Registration,
}

impl DeviceWatch {
    /// `wake` is called (at most once per new kind of change) from a COM thread.
    pub(crate) fn start(wake: Wake) -> Result<Self, String> {
        let flags = Arc::new(Flags { pending: AtomicU8::new(0), wake });
        #[cfg(windows)]
        let registration = platform::Registration::new(flags.clone())?;
        Ok(Self {
            flags,
            #[cfg(windows)]
            _registration: registration,
        })
    }

    /// Changes since the previous call.
    pub(crate) fn take(&self) -> Change {
        decode(self.flags.pending.swap(0, Ordering::AcqRel))
    }
}

fn decode(bits: u8) -> Change {
    Change {
        default_input: bits & DEFAULT_INPUT != 0,
        default_output: bits & DEFAULT_OUTPUT != 0,
        devices: bits & DEVICES != 0,
    }
}

#[cfg(windows)]
mod platform {
    use super::{Flags, DEFAULT_INPUT, DEFAULT_OUTPUT, DEVICES};
    use std::sync::Arc;
    use windows::{
        core::{implement, PCWSTR},
        Win32::{
            Media::Audio::{
                eCapture, eConsole, eRender, EDataFlow, ERole, IMMDeviceEnumerator,
                IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
            },
            System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED},
            UI::Shell::PropertiesSystem::PROPERTYKEY,
        },
    };

    #[implement(IMMNotificationClient)]
    struct Client {
        flags: Arc<Flags>,
    }

    impl IMMNotificationClient_Impl for Client_Impl {
        fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
            self.flags.raise(DEVICES);
            Ok(())
        }
        fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
            self.flags.raise(DEVICES);
            Ok(())
        }
        fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
            self.flags.raise(DEVICES);
            Ok(())
        }
        fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> windows::core::Result<()> {
            // cpal's "System default" is the console default; the communications
            // default does not change what Relay opens.
            if role == eConsole {
                if flow == eCapture {
                    self.flags.raise(DEFAULT_INPUT);
                } else if flow == eRender {
                    self.flags.raise(DEFAULT_OUTPUT);
                }
            }
            Ok(())
        }
        fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// A dedicated MTA thread owns the enumerator and the registered client,
    /// so the UI thread's COM apartment (OLE drag-drop, dialogs) is untouched.
    pub(super) struct Registration {
        stop: Option<std::sync::mpsc::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Registration {
        pub(super) fn new(flags: Arc<Flags>) -> Result<Self, String> {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            let thread = std::thread::Builder::new()
                .name("relay-device-watch".into())
                .spawn(move || unsafe {
                    let initialised = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
                    let registered = (|| -> Result<(IMMDeviceEnumerator, IMMNotificationClient), String> {
                        let enumerator: IMMDeviceEnumerator =
                            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                                .map_err(|error| format!("Watch audio devices: {error}"))?;
                        let client: IMMNotificationClient = Client { flags }.into();
                        enumerator
                            .RegisterEndpointNotificationCallback(&client)
                            .map_err(|error| format!("Watch audio devices: {error}"))?;
                        Ok((enumerator, client))
                    })();
                    match registered {
                        Ok((enumerator, client)) => {
                            let _ = ready_tx.send(Ok(()));
                            // Wait for the app to drop the watch (or to exit).
                            let _ = stop_rx.recv();
                            let _ = enumerator.UnregisterEndpointNotificationCallback(&client);
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                        }
                    }
                    if initialised {
                        windows::Win32::System::Com::CoUninitialize();
                    }
                })
                .map_err(|error| format!("Watch audio devices: {error}"))?;
            match ready_rx.recv() {
                Ok(Ok(())) => Ok(Self { stop: Some(stop_tx), thread: Some(thread) }),
                Ok(Err(error)) => {
                    let _ = thread.join();
                    Err(error)
                }
                Err(_) => Err("Watch audio devices: the watcher stopped".into()),
            }
        }
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            self.stop.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn flags_are_taken_once_and_wake_only_for_new_kinds() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let flags = Flags {
            pending: AtomicU8::new(0),
            wake: Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        };
        flags.raise(DEVICES);
        flags.raise(DEVICES);
        flags.raise(DEFAULT_OUTPUT);
        assert_eq!(wakes.load(Ordering::Relaxed), 2, "a burst of the same event wakes once");
        let change = decode(flags.pending.swap(0, Ordering::AcqRel));
        assert_eq!(change, Change { default_input: false, default_output: true, devices: true });
        assert_eq!(decode(flags.pending.load(Ordering::Acquire)), Change::default());
    }

    #[cfg(windows)]
    #[test]
    fn registration_with_windows_succeeds_and_drops_cleanly() {
        let watch = DeviceWatch::start(Arc::new(|| {})).expect("register endpoint notifications");
        let _ = watch.take();
        drop(watch);
    }
}
