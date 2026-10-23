//! Windows logon-bound kernel objects. No account/profile files are consulted.
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{LocalFree, HANDLE, HLOCAL, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            GetLengthSid, GetTokenInformation, IsValidSid, TokenGroups, TokenUser,
            PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS,
            TOKEN_QUERY, TOKEN_USER,
        },
        System::{
            Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
            Threading::{
                CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, ReleaseMutex,
                WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            },
        },
    },
};

const IDENTITY_ERROR: &str = "Windows could not verify this Relay session. Open Relay and paste the invitation in Join friends.";
pub const PIPE_ACCESS: u32 = 0x0012019b; // Individual read/write rights; excludes pipe-instance creation.

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
pub fn raw(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}
/// Take exclusive ownership of an open Windows handle.
///
/// # Safety
/// `handle` must be valid, uniquely owned and releasable with CloseHandle.
pub unsafe fn own(handle: HANDLE) -> OwnedHandle {
    OwnedHandle::from_raw_handle(handle.0)
}

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.0));
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    user: Vec<u8>,
    logon: Vec<u8>,
    logon_text: String,
}

fn token_data(token: HANDLE, kind: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>, &'static str> {
    let mut needed = 0;
    unsafe {
        let _ = GetTokenInformation(token, kind, None, 0, &mut needed);
    }
    if needed == 0 || needed > 64 * 1024 {
        return Err(IDENTITY_ERROR);
    }
    let capacity = needed;
    let mut buffer = vec![0usize; (capacity as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            kind,
            Some(buffer.as_mut_ptr().cast()),
            capacity,
            &mut needed,
        )
    }
    .map_err(|_| IDENTITY_ERROR)?;
    if needed > capacity {
        return Err(IDENTITY_ERROR);
    }
    Ok(buffer)
}

fn sid(sid: PSID, buffer: &[usize]) -> Result<Vec<u8>, &'static str> {
    let address = sid.0 as usize;
    let start = buffer.as_ptr() as usize;
    let end = start + std::mem::size_of_val(buffer);
    if address < start || address.checked_add(8).is_none_or(|n| n > end) {
        return Err(IDENTITY_ERROR);
    }
    if !unsafe { IsValidSid(sid) }.as_bool() {
        return Err(IDENTITY_ERROR);
    }
    let length = unsafe { GetLengthSid(sid) } as usize;
    if !(8..=68).contains(&length) || address.checked_add(length).is_none_or(|n| n > end) {
        return Err(IDENTITY_ERROR);
    }
    Ok(unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), length) }.to_vec())
}

impl Identity {
    #[cfg(test)]
    pub fn assert_created_pipe_acl(&self, pipe: HANDLE) {
        use windows::Win32::Security::{
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo,
                SE_KERNEL_OBJECT,
            },
            DACL_SECURITY_INFORMATION,
        };
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            GetSecurityInfo(
                pipe,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&mut descriptor),
            )
        }
        .ok()
        .expect("Inspect owned pipe descriptor");
        let actual = Descriptor(descriptor);
        let expected = self.descriptor(true).unwrap();
        let normalized = |descriptor: &Descriptor| {
            let mut text = PWSTR::null();
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor.0,
                    1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    None,
                )
            }
            .expect("Read owned DACL");
            let _allocation = LocalAllocation(text.0.cast());
            unsafe { text.to_string() }.unwrap()
        };
        assert!(
            normalized(&actual) == normalized(&expected),
            "Created pipe DACL differs from explicit logon policy"
        );
    }

    #[cfg(test)]
    pub fn different_fixture_identity(&self) -> Self {
        let mut different = self.clone();
        different.user.push(0);
        different
    }

    pub fn current() -> Result<Self, &'static str> {
        Self::process(unsafe { GetCurrentProcess() })
    }

    fn process(process: HANDLE) -> Result<Self, &'static str> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
            .map_err(|_| IDENTITY_ERROR)?;
        let token = unsafe { own(token) };
        let user = token_data(raw(&token), TokenUser)?;
        if std::mem::size_of_val(user.as_slice()) < size_of::<TOKEN_USER>() {
            return Err(IDENTITY_ERROR);
        }
        let user = sid(
            unsafe { &*user.as_ptr().cast::<TOKEN_USER>() }.User.Sid,
            &user,
        )?;
        let groups = token_data(raw(&token), TokenGroups)?;
        let bytes = std::mem::size_of_val(groups.as_slice());
        if bytes < size_of::<TOKEN_GROUPS>() {
            return Err(IDENTITY_ERROR);
        }
        let table = unsafe { &*groups.as_ptr().cast::<TOKEN_GROUPS>() };
        let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
        let count = table.GroupCount as usize;
        let entry_size = size_of::<windows::Win32::Security::SID_AND_ATTRIBUTES>();
        if count > 1024 || offset + count * entry_size > bytes {
            return Err(IDENTITY_ERROR);
        }
        let entries = unsafe { std::slice::from_raw_parts(table.Groups.as_ptr(), count) };
        let mut logons = entries
            .iter()
            .filter(|entry| entry.Attributes & 0xc0000000 == 0xc0000000);
        let group = logons.next().ok_or(IDENTITY_ERROR)?;
        if logons.next().is_some() {
            return Err(IDENTITY_ERROR);
        }
        let logon = sid(group.Sid, &groups)?;
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(group.Sid, &mut text) }.map_err(|_| IDENTITY_ERROR)?;
        let _allocation = LocalAllocation(text.0.cast());
        let logon_text = unsafe { text.to_string() }.map_err(|_| IDENTITY_ERROR)?;
        if !logon_text.starts_with("S-1-5-5-") || logon_text.len() > 184 {
            return Err(IDENTITY_ERROR);
        }
        Ok(Self {
            user,
            logon,
            logon_text,
        })
    }

    pub fn namespace(&self, scope: &str) -> String {
        let mut hash = Sha256::new();
        for bytes in [
            b"Relay.Main.v1".as_slice(),
            self.user.as_slice(),
            self.logon.as_slice(),
            scope.as_bytes(),
        ] {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        format!("Relay.Main.v1.{:x}", hash.finalize())
    }

    pub fn descriptor(&self, pipe: bool) -> Result<Descriptor, &'static str> {
        let rights = if pipe { "0x0012019b" } else { "GA" };
        // Owner Rights suppresses the owner's implicit WRITE_DAC. The account
        // SID alone must not bypass a logon-scoped DACL from another logon.
        let text = wide(&format!(
            "D:P(A;;{rights};;;{})(A;;RC;;;OW)",
            self.logon_text
        ));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|_| IDENTITY_ERROR)?;
        Ok(Descriptor(descriptor))
    }

    pub fn account_sid(&self) -> Result<String, &'static str> {
        let mut text = PWSTR::null();
        unsafe { ConvertSidToStringSidW(PSID(self.user.as_ptr().cast_mut().cast()), &mut text) }
            .map_err(|_| IDENTITY_ERROR)?;
        let _allocation = LocalAllocation(text.0.cast());
        unsafe { text.to_string() }.map_err(|_| IDENTITY_ERROR)
    }

    /// HKCU protocol ownership is shared by this account's Windows logons.
    /// This lock is separate from the logon-scoped main-window lifetime lock.
    pub fn protocol_lease(&self, scheme: &str) -> Result<Lease, &'static str> {
        let user = self.account_sid()?;
        let sddl = wide(&format!("D:P(A;;GA;;;{user})(A;;RC;;;OW)"));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|_| IDENTITY_ERROR)?;
        let descriptor = Descriptor(descriptor);
        let mut hash = Sha256::new();
        for bytes in [
            b"Relay.Protocol.v1".as_slice(),
            self.user.as_slice(),
            scheme.as_bytes(),
        ] {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let name = wide(&format!("Global\\Relay.Protocol.v1.{:x}", hash.finalize()));
        let attributes = descriptor.attributes();
        let handle = unsafe { CreateMutexW(Some(&attributes), false, PCWSTR(name.as_ptr())) }
            .map_err(|_| IDENTITY_ERROR)?;
        let handle = unsafe { own(handle) };
        match unsafe { WaitForSingleObject(raw(&handle), 1500) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Lease {
                handle,
                _thread_bound: std::marker::PhantomData,
            }),
            _ => Err(IDENTITY_ERROR),
        }
    }

    pub fn peer(&self, pipe: HANDLE, server: bool) -> Result<Peer, &'static str> {
        let mut pid = 0;
        unsafe {
            if server {
                GetNamedPipeServerProcessId(pipe, &mut pid)
            } else {
                GetNamedPipeClientProcessId(pipe, &mut pid)
            }
        }
        .map_err(|_| IDENTITY_ERROR)?;
        if pid == 0 {
            return Err(IDENTITY_ERROR);
        }
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .map_err(|_| IDENTITY_ERROR)?;
        let process = unsafe { own(process) };
        if Self::process(raw(&process))? != *self
            || unsafe { WaitForSingleObject(raw(&process), 0) } != WAIT_TIMEOUT
        {
            return Err(IDENTITY_ERROR);
        }
        let mut still = 0;
        unsafe {
            if server {
                GetNamedPipeServerProcessId(pipe, &mut still)
            } else {
                GetNamedPipeClientProcessId(pipe, &mut still)
            }
        }
        .map_err(|_| IDENTITY_ERROR)?;
        if still != pid {
            return Err(IDENTITY_ERROR);
        }
        Ok(Peer {
            pid,
            _process: process,
        })
    }
}

pub struct Peer {
    pub pid: u32,
    _process: OwnedHandle,
}
pub struct Descriptor(PSECURITY_DESCRIPTOR);
impl Descriptor {
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        }
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.0 .0));
        }
    }
}

pub struct Lease {
    handle: OwnedHandle,
    _thread_bound: std::marker::PhantomData<*mut ()>,
}
pub enum Election {
    Owner(Lease),
    Existing,
}
pub fn elect(identity: &Identity, namespace: &str) -> Result<Election, &'static str> {
    let descriptor = identity.descriptor(false)?;
    let attributes = descriptor.attributes();
    let name = wide(&format!("Local\\{namespace}"));
    let handle = unsafe { CreateMutexW(Some(&attributes), false, PCWSTR(name.as_ptr())) }
        .map_err(|_| IDENTITY_ERROR)?;
    let handle = unsafe { own(handle) };
    match unsafe { WaitForSingleObject(raw(&handle), 0) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Election::Owner(Lease {
            handle,
            _thread_bound: std::marker::PhantomData,
        })),
        WAIT_TIMEOUT => Ok(Election::Existing),
        _ => Err(IDENTITY_ERROR),
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(raw(&self.handle));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_logon_is_stable_and_objects_exclude_instance_creation_from_client_rights() {
        let identity = Identity::current().unwrap();
        assert!(identity == Identity::current().unwrap());
        assert_eq!(PIPE_ACCESS & 4, 0);
        assert_ne!(
            identity.namespace("main"),
            identity.namespace("owned-fixture")
        );
        assert!(identity.descriptor(true).is_ok() && identity.descriptor(false).is_ok());
    }
    #[test]
    fn lifetime_mutex_is_exclusive_between_threads_and_released_by_drop() {
        let identity = Identity::current().unwrap();
        let name = identity.namespace(&uuid::Uuid::new_v4().to_string());
        let Election::Owner(lease) = elect(&identity, &name).unwrap() else {
            panic!("Fresh fixture occupied")
        };
        let other = identity.clone();
        let other_name = name.clone();
        assert!(std::thread::spawn(move || matches!(
            elect(&other, &other_name).unwrap(),
            Election::Existing
        ))
        .join()
        .unwrap());
        drop(lease);
        assert!(matches!(
            elect(&identity, &name).unwrap(),
            Election::Owner(_)
        ));
    }
}
