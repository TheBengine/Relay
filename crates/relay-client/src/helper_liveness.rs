//! Read-only helper observations independent of serialized network/control work.
//! An exact owned process handle, never a PID lookup, determines liveness.
use std::{
    process::Child,
    sync::{Arc, Mutex},
};

struct Process {
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}
impl Process {
    fn capture(child: &Child) -> Result<Self, String> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle;
            Ok(Self {
                handle: child
                    .as_handle()
                    .try_clone_to_owned()
                    .map_err(|_| "Cannot observe the private connection process.")?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Err("Private helper process observation requires Windows.".into())
        }
    }
    fn alive(&self) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::{
                Foundation::{HANDLE, WAIT_TIMEOUT},
                System::Threading::WaitForSingleObject,
            };
            unsafe { WaitForSingleObject(HANDLE(self.handle.as_raw_handle()), 0) == WAIT_TIMEOUT }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

struct Observation<H> {
    process: Arc<Process>,
    generation: [u8; 32],
    hosted: Option<H>,
}

pub(crate) struct Registry<H> {
    // Never held over helper HTTP requests, startup, shutdown or thread joins.
    current: Mutex<Option<Observation<H>>>,
}
impl<H: 'static> Registry<H> {
    pub const fn new() -> Self {
        Self {
            current: Mutex::new(None),
        }
    }

    pub fn observe(
        &'static self,
        child: &Child,
        generation: [u8; 32],
    ) -> Result<Registration<H>, String> {
        let process = Arc::new(Process::capture(child)?);
        *self
            .current
            .lock()
            .map_err(|_| "The connection status is unavailable.")? = Some(Observation {
            process: process.clone(),
            generation,
            hosted: None,
        });
        Ok(Registration {
            registry: self,
            process,
        })
    }
    pub fn alive(&self, generation: [u8; 32]) -> bool {
        self.current.lock().ok().is_some_and(|current| {
            current.as_ref().is_some_and(|observed| {
                observed.generation == generation && observed.process.alive()
            })
        })
    }
    pub fn set_hosted(&self, generation: [u8; 32], hosted: Option<H>) {
        if let Ok(mut current) = self.current.lock() {
            if let Some(observed) = current.as_mut().filter(|o| o.generation == generation) {
                observed.hosted = hosted;
            }
        }
    }
    pub fn retire(&self) {
        if let Ok(mut current) = self.current.lock() {
            *current = None;
        }
    }
    fn retire_process(&self, process: &Arc<Process>) {
        if let Ok(mut current) = self.current.lock() {
            if current
                .as_ref()
                .is_some_and(|o| Arc::ptr_eq(&o.process, process))
            {
                *current = None;
            }
        }
    }
}
impl<H: Clone + 'static> Registry<H> {
    pub fn hosted(&self) -> Option<H> {
        let current = self.current.lock().ok()?;
        let observed = current.as_ref()?;
        observed
            .process
            .alive()
            .then(|| observed.hosted.clone())
            .flatten()
    }
}

pub(crate) struct Registration<H: 'static> {
    registry: &'static Registry<H>,
    process: Arc<Process>,
}
impl<H: 'static> Drop for Registration<H> {
    fn drop(&mut self) {
        self.registry.retire_process(&self.process);
    }
}
