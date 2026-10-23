//! Best-effort Windows scheduling hints, scoped to the audio worker's lifetime.
#[cfg(windows)]
mod platform {
    use std::{
        marker::PhantomData,
        rc::Rc,
        sync::{Mutex, Once},
    };
    use windows::{
        core::PCWSTR,
        Win32::{Foundation::HANDLE, System::Threading::*},
    };

    pub(crate) struct AudioThreadGuard(Option<HANDLE>, PhantomData<Rc<()>>);
    impl AudioThreadGuard {
        pub(crate) fn new(task: &str) -> Self {
            let name: Vec<u16> = task.encode_utf16().chain(Some(0)).collect();
            let mut index = 0;
            let handle =
                unsafe { AvSetMmThreadCharacteristicsW(PCWSTR(name.as_ptr()), &mut index) }.ok();
            if let Some(handle) = handle {
                if unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) }.is_ok() {
                    eprintln!("audio thread priority: {task}");
                } else {
                    unavailable();
                }
            } else {
                unavailable();
            }
            Self(handle, PhantomData)
        }
    }
    impl Drop for AudioThreadGuard {
        fn drop(&mut self) {
            if let Some(handle) = self.0 {
                unsafe {
                    let _ = AvRevertMmThreadCharacteristics(handle);
                }
            }
        }
    }
    fn unavailable() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            eprintln!("Audio priority hint unavailable; continuing with normal scheduling")
        });
    }

    // One process setting, restored exactly to the previous policy on call exit.
    // A failed read never replaces a policy we cannot restore.
    pub(crate) fn set_call_power_mode(on: bool) {
        static SAVED: Mutex<Option<PROCESS_POWER_THROTTLING_STATE>> = Mutex::new(None);
        static FAILED: Once = Once::new();
        let Ok(mut saved) = SAVED.lock() else {
            return;
        };
        unsafe {
            let process = GetCurrentProcess();
            let size = std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32;
            if on && saved.is_none() {
                let mut previous = PROCESS_POWER_THROTTLING_STATE {
                    Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                    ..Default::default()
                };
                if GetProcessInformation(
                    process,
                    ProcessPowerThrottling,
                    (&mut previous as *mut PROCESS_POWER_THROTTLING_STATE).cast(),
                    size,
                )
                .is_err()
                {
                    return;
                }
                let mask = PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                    | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
                let next = PROCESS_POWER_THROTTLING_STATE {
                    Version: previous.Version,
                    ControlMask: previous.ControlMask | mask,
                    StateMask: previous.StateMask & !mask,
                };
                if SetProcessInformation(
                    process,
                    ProcessPowerThrottling,
                    (&next as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                    size,
                )
                .is_ok()
                {
                    *saved = Some(previous);
                } else {
                    FAILED.call_once(|| {
                        eprintln!("Call power hint unavailable; audio remains enabled")
                    });
                }
            } else if !on {
                if let Some(previous) = saved.as_ref() {
                    if SetProcessInformation(
                        process,
                        ProcessPowerThrottling,
                        (previous as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                        size,
                    )
                    .is_ok()
                    {
                        *saved = None;
                    } else {
                        FAILED.call_once(|| eprintln!("Could not restore call power policy"));
                    }
                }
            }
        }
    }
}
#[cfg(windows)]
pub(crate) use platform::{set_call_power_mode, AudioThreadGuard};
#[cfg(not(windows))]
pub(crate) struct AudioThreadGuard;
#[cfg(not(windows))]
impl AudioThreadGuard {
    pub(crate) fn new(_: &str) -> Self {
        Self
    }
}
#[cfg(not(windows))]
pub(crate) fn set_call_power_mode(_: bool) {}

pub(crate) struct CallPowerGuard;
impl CallPowerGuard {
    pub(crate) fn new() -> Self {
        set_call_power_mode(true);
        Self
    }
}
impl Drop for CallPowerGuard {
    fn drop(&mut self) {
        set_call_power_mode(false);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn audio_priority_and_power_hints_are_scoped_and_nonfatal() {
        std::thread::spawn(|| {
            let _audio = super::AudioThreadGuard::new("Audio");
            super::set_call_power_mode(true);
            super::set_call_power_mode(true);
            super::set_call_power_mode(false);
            super::set_call_power_mode(false);
        })
        .join()
        .unwrap();
    }
}
