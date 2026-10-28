//! Windows camera arrival and removal: CM_Register_Notification on the camera
//! device interface classes, the camera counterpart of `device_watch`. The
//! callback only raises a flag and asks for a repaint; the UI thread lists
//! the cameras again in the background.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

type Wake = Arc<dyn Fn() + Send + Sync>;

struct Flag {
    pending: AtomicBool,
    wake: Wake,
}

impl Flag {
    fn raise(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }
}

pub(crate) struct CameraWatch {
    flag: Arc<Flag>,
    #[cfg(windows)]
    _registrations: Vec<platform::Registration>,
}

impl CameraWatch {
    /// `wake` runs on a system thread at most once until the next `take`.
    pub(crate) fn start(wake: Wake) -> Result<Self, String> {
        let flag = Arc::new(Flag {
            pending: AtomicBool::new(false),
            wake,
        });
        #[cfg(windows)]
        let registrations = platform::Registration::all(&flag)?;
        Ok(Self {
            flag,
            #[cfg(windows)]
            _registrations: registrations,
        })
    }

    /// Whether a camera arrived or left since the previous call.
    pub(crate) fn take(&self) -> bool {
        self.flag.pending.swap(false, Ordering::AcqRel)
    }
}

#[cfg(windows)]
mod platform {
    use super::Flag;
    use std::{ffi::c_void, sync::Arc};
    use windows::{
        core::GUID,
        Win32::Devices::DeviceAndDriverInstallation::{
            CM_Register_Notification, CM_Unregister_Notification, CM_NOTIFY_ACTION,
            CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL,
            CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_2,
            CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS, HCMNOTIFICATION,
        },
    };

    /// KSCATEGORY_VIDEO_CAMERA and KSCATEGORY_VIDEO: the classes Media
    /// Foundation lists video capture devices from.
    const CLASSES: [GUID; 2] = [
        GUID::from_u128(0xe532_3777_f976_4f5b_9b55_b946_99c4_6e44),
        GUID::from_u128(0x6994_ad05_93ef_11d0_a3cc_00a0_c922_3196),
    ];

    pub(super) struct Registration {
        handle: HCMNOTIFICATION,
        context: *const Flag,
    }

    // The handle is only passed back to cfgmgr32 on drop; the flag is shared.
    unsafe impl Send for Registration {}

    unsafe extern "system" fn changed(
        _: HCMNOTIFICATION,
        context: *const c_void,
        action: CM_NOTIFY_ACTION,
        _: *const CM_NOTIFY_EVENT_DATA,
        _: u32,
    ) -> u32 {
        if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
            || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
        {
            // SAFETY: the registration owns one strong count until it is
            // unregistered, and cfgmgr32 finishes callbacks before that returns.
            unsafe { (*(context as *const Flag)).raise() };
        }
        0
    }

    impl Registration {
        pub(super) fn all(flag: &Arc<Flag>) -> Result<Vec<Self>, String> {
            CLASSES.iter().map(|class| Self::new(flag, *class)).collect()
        }

        fn new(flag: &Arc<Flag>, class: GUID) -> Result<Self, String> {
            let filter = CM_NOTIFY_FILTER {
                cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
                Flags: 0,
                FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
                Reserved: 0,
                u: CM_NOTIFY_FILTER_0 {
                    DeviceInterface: CM_NOTIFY_FILTER_0_2 { ClassGuid: class },
                },
            };
            let context = Arc::into_raw(flag.clone());
            let mut handle = HCMNOTIFICATION::default();
            // SAFETY: the filter outlives the call; the context stays valid
            // until Drop unregisters and releases it.
            let result = unsafe {
                CM_Register_Notification(&filter, Some(context.cast()), Some(changed), &mut handle)
            };
            if result != CR_SUCCESS {
                // SAFETY: not registered, so nothing else holds this count.
                unsafe { drop(Arc::from_raw(context)) };
                return Err(format!("Watch for cameras: CM_Register_Notification returned {}", result.0));
            }
            Ok(Self { handle, context })
        }
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            // SAFETY: registered in `new`; after unregistering no callback runs.
            unsafe {
                let _ = CM_Unregister_Notification(self.handle);
                drop(Arc::from_raw(self.context));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn registers_with_windows_and_reports_each_change_once() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let watch = CameraWatch::start(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }))
        .expect("Windows accepts camera notifications without opening a camera");
        assert!(!watch.take());
        // A burst of arrivals wakes the UI once and is read once.
        watch.flag.raise();
        watch.flag.raise();
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(watch.take());
        assert!(!watch.take());
        drop(watch);
    }
}
