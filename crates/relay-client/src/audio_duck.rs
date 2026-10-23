//! Optional Windows session ducking. Every changed session is restored on disable/exit.
use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    thread,
    time::Duration,
};
use windows::{
    core::Interface,
    Win32::{Media::Audio::*, System::Com::*},
};

pub struct Ducker {
    sender: Option<mpsc::SyncSender<(bool, f32)>>,
    owner: Option<thread::JoinHandle<()>>,
    last: Option<(bool, f32)>,
}
impl Ducker {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let owner = thread::spawn(move || unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                return;
            }
            run(receiver);
            CoUninitialize();
        });
        Self {
            sender: Some(sender),
            owner: Some(owner),
            last: None,
        }
    }
    pub fn set(&mut self, active: bool, volume: f32) {
        let setting = (active, volume.clamp(0.0, 1.0));
        if self.last != Some(setting)
            && self
                .sender
                .as_ref()
                .is_some_and(|s| s.try_send(setting).is_ok())
        {
            self.last = Some(setting);
        }
    }
}
impl Drop for Ducker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}
#[derive(Clone, Copy)]
struct VolumeState {
    original: f32,
    written: f32,
    manual_override: bool,
}
impl VolumeState {
    fn new(volume: f32) -> Self {
        Self {
            original: volume,
            written: volume,
            manual_override: false,
        }
    }
    fn target(&mut self, current: f32, fraction: f32) -> Option<f32> {
        if (current - self.written).abs() >= 0.001 {
            self.manual_override = true;
        }
        (!self.manual_override).then_some(self.original * fraction)
    }
    fn restore(&self, current: f32) -> Option<f32> {
        (!self.manual_override && (current - self.written).abs() < 0.001).then_some(self.original)
    }
}
struct Saved {
    control: ISimpleAudioVolume,
    state: VolumeState,
}
unsafe fn restore(saved: &mut HashMap<String, Saved>) {
    for entry in saved.values() {
        if let Some(original) = entry
            .control
            .GetMasterVolume()
            .ok()
            .and_then(|current| entry.state.restore(current))
        {
            let _ = entry.control.SetMasterVolume(original, std::ptr::null());
        }
    }
    saved.clear();
}
unsafe fn run(receiver: mpsc::Receiver<(bool, f32)>) {
    let mut saved = HashMap::new();
    let mut active = false;
    let mut fraction = 0.3;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok((enabled, volume)) => {
                active = enabled;
                fraction = volume;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !active {
            restore(&mut saved);
            continue;
        }
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            continue;
        };
        let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) else {
            continue;
        };
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            continue;
        };
        let Ok(sessions) = manager.GetSessionEnumerator() else {
            continue;
        };
        let Ok(endpoint) = device.GetId() else {
            continue;
        };
        let endpoint_key = endpoint.to_string().ok();
        CoTaskMemFree(Some(endpoint.0.cast()));
        let Some(endpoint_key) = endpoint_key else {
            continue;
        };
        let mut seen = HashSet::new();
        for index in 0..sessions.GetCount().unwrap_or(0) {
            let Ok(session) = sessions.GetSession(index) else {
                continue;
            };
            let Ok(control) = session.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let Ok(pid) = control.GetProcessId() else {
                continue;
            };
            if pid == 0 || pid == std::process::id() {
                continue;
            }
            let Ok(volume) = session.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            let Ok(current) = volume.GetMasterVolume() else {
                continue;
            };
            let Ok(identifier) = control.GetSessionInstanceIdentifier() else {
                continue;
            };
            let key = identifier.to_string().ok();
            CoTaskMemFree(Some(identifier.0.cast()));
            let Some(key) = key else {
                continue;
            };
            let key = format!("{endpoint_key}\n{key}");
            seen.insert(key.clone());
            let entry = saved.entry(key).or_insert_with(|| Saved {
                control: volume,
                state: VolumeState::new(current),
            });
            if let Some(target) = entry.state.target(current, fraction) {
                if entry
                    .control
                    .SetMasterVolume(target, std::ptr::null())
                    .is_ok()
                {
                    entry.state.written = target;
                }
            }
        }
        saved.retain(|key, entry| {
            if seen.contains(key) {
                return true;
            }
            if let Some(original) = entry
                .control
                .GetMasterVolume()
                .ok()
                .and_then(|current| entry.state.restore(current))
            {
                let _ = entry.control.SetMasterVolume(original, std::ptr::null());
            }
            false
        });
    }
    restore(&mut saved);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distinct_sessions_restore_their_own_baselines_and_manual_changes_win() {
        let mut sessions = HashMap::from([
            ("endpoint/session-1", VolumeState::new(0.8)),
            ("endpoint/session-2", VolumeState::new(0.4)),
        ]);
        for state in sessions.values_mut() {
            state.written = state.target(state.written, 0.25).unwrap();
        }
        assert_eq!(sessions["endpoint/session-1"].restore(0.2), Some(0.8));
        assert_eq!(sessions["endpoint/session-2"].restore(0.1), Some(0.4));
        let second = sessions.get_mut("endpoint/session-2").unwrap();
        assert_eq!(second.target(0.7, 0.25), None);
        assert_eq!(second.restore(0.7), None);
        assert_eq!(second.target(0.7, 0.1), None);
        assert_eq!(sessions["endpoint/session-1"].restore(0.2), Some(0.8));
        assert_eq!(sessions["endpoint/session-1"].restore(0.9), None);
    }
}
