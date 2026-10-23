//! Optional packet classification. Failure never prevents or restarts a call.
#[cfg(windows)]
mod platform {
    use std::{
        net::{SocketAddr, UdpSocket},
        os::windows::io::AsRawSocket,
    };
    use windows::Win32::{
        Foundation::HANDLE, NetworkManagement::QoS::*, Networking::WinSock::SOCKET,
    };
    pub(crate) struct VoiceQos {
        handle: usize,
        flow: u32,
        socket: UdpSocket,
    }
    impl VoiceQos {
        pub(crate) fn new(
            socket: &UdpSocket,
            destination: SocketAddr,
            enabled: bool,
        ) -> Option<Self> {
            if !enabled {
                return None;
            }
            let socket = socket.try_clone().ok()?;
            let address = socket2::SockAddr::from(destination);
            let mut handle = HANDLE::default();
            let version = QOS_VERSION {
                MajorVersion: 1,
                MinorVersion: 0,
            };
            let mut flow = 0;
            unsafe {
                if !QOSCreateHandle(&version, &mut handle).as_bool() {
                    eprintln!("Voice packet priority unavailable; continuing normally");
                    return None;
                }
                if !QOSAddSocketToFlow(
                    handle,
                    SOCKET(socket.as_raw_socket() as usize),
                    Some(address.as_ptr().cast()),
                    QOSTrafficTypeVoice,
                    QOS_NON_ADAPTIVE_FLOW,
                    &mut flow,
                )
                .as_bool()
                {
                    let _ = QOSCloseHandle(handle);
                    eprintln!(
                        "Voice packet priority unavailable for this route; continuing normally"
                    );
                    return None;
                }
            }
            Some(Self {
                handle: handle.0 as usize,
                flow,
                socket,
            })
        }
    }
    impl Drop for VoiceQos {
        fn drop(&mut self) {
            unsafe {
                let handle = HANDLE(self.handle as *mut std::ffi::c_void);
                let _ = QOSRemoveSocketFromFlow(
                    handle,
                    SOCKET(self.socket.as_raw_socket() as usize),
                    self.flow,
                    0,
                );
                let _ = QOSCloseHandle(handle);
            }
        }
    }
}
#[cfg(windows)]
pub(crate) use platform::VoiceQos;
#[cfg(not(windows))]
pub(crate) struct VoiceQos;
#[cfg(not(windows))]
impl VoiceQos {
    pub(crate) fn new(_: &std::net::UdpSocket, _: std::net::SocketAddr, _: bool) -> Option<Self> {
        None
    }
}
