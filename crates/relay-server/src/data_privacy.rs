//! Protect the explicitly selected Windows community before opening any database.
//! Administrator privileges can still override a DACL; this isolates ordinary users.
use std::{
    ffi::OsString,
    fs, io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, LocalFree, BOOL, HANDLE, HLOCAL},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SetSecurityInfo, SE_FILE_OBJECT,
            },
            GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL,
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            PSID, TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.0 .0));
        }
    }
}
fn wide(value: &std::ffi::OsStr) -> Result<Vec<u16>> {
    let mut text: Vec<_> = value.encode_wide().collect();
    if text.contains(&0) {
        return Err("Data path contains a null character".into());
    }
    text.push(0);
    Ok(text)
}
// Normalize lexically only: canonicalize would follow a junction before the
// checked handle has a chance to reject it. Accept ordinary and verbatim file
// paths, but never generic Win32 device namespaces.
fn absolute_storage_path(path: &Path) -> Result<PathBuf> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    if name.contains(&0) {
        return Err("Data path contains a null character".into());
    }
    for unit in &mut name {
        if *unit == u16::from(b'/') {
            *unit = u16::from(b'\\');
        }
    }
    if name.starts_with(&[92, 92, 63, 92]) {
        if name.len() >= 7
            && (name[4] >= 65 && name[4] <= 90 || name[4] >= 97 && name[4] <= 122)
            && name[5..7] == [58, 92]
        {
            name.drain(..4);
        } else if name.len() >= 8
            && matches!(name[4], 85 | 117)
            && matches!(name[5], 78 | 110)
            && matches!(name[6], 67 | 99)
            && name[7] == 92
        {
            name.splice(..8, [92, 92]);
        } else {
            return Err("Choose a drive or UNC community folder, not a device namespace".into());
        }
    } else if name.starts_with(&[92, 92, 46, 92]) {
        return Err("Choose a drive or UNC community folder, not a device namespace".into());
    }
    let path = std::path::absolute(PathBuf::from(OsString::from_wide(&name)))?;
    if !matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(_, _))
    ) {
        return Err("Choose a drive or UNC community folder, not a device namespace".into());
    }
    Ok(path)
}
fn native_file_name(path: &Path) -> Result<Vec<u16>> {
    let path = absolute_storage_path(path)?;
    let name: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut extended: Vec<u16> = match path.components().next() {
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::UNC(_, _)) => {
            // UNC extended names begin \\?\UNC\, replacing the ordinary \\.
            let mut extended: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
            extended.extend_from_slice(&name[2..]);
            extended
        }
        _ => {
            let mut extended: Vec<u16> = r"\\?\".encode_utf16().collect();
            extended.extend_from_slice(&name);
            extended
        }
    };
    extended.push(0);
    Ok(extended)
}
fn current_user_sid() -> Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = OwnedHandle(token);
        let mut needed = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err("Cannot determine the Windows hosting account".into());
        }
        let mut buffer = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )?;
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text)?;
        let sid = text.to_string();
        let _ = LocalFree(HLOCAL(text.0.cast()));
        Ok(sid?)
    }
}
impl Descriptor {
    fn new(sddl: &str) -> Result<Self> {
        let text = wide(std::ffi::OsStr::new(sddl))?;
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self(descriptor))
    }
    fn apply(&self, path: &Path) -> Result<()> {
        reject_reparse_ancestors(path)?;
        // Open the object itself, never a reparse target. Recheck link count on
        // this handle so a raced rename cannot redirect the ACL to another file.
        let file = checked_handle(path, true)?;
        unsafe {
            let mut present = BOOL::default();
            let mut defaulted = BOOL::default();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted)?;
            if !present.as_bool() || dacl.is_null() {
                return Err("Missing private Windows ACL".into());
            }
            let status = SetSecurityInfo(
                file.0,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                PSID::default(),
                PSID::default(),
                Some(dacl),
                None,
            );
            if status.0 != 0 {
                return Err(io::Error::from_raw_os_error(status.0 as i32).into());
            }
        }
        Ok(())
    }
}
fn checked_handle(path: &Path, writable_acl: bool) -> Result<OwnedHandle> {
    // Explicit extended names work even when the executable/host policy has
    // not opted into MAX_PATH removal. OPEN_REPARSE_POINT stays in force.
    let name = native_file_name(path)?;
    let access = FILE_READ_ATTRIBUTES.0
        | if writable_acl {
            READ_CONTROL.0 | WRITE_DAC.0
        } else {
            0
        };
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            HANDLE::default(),
        )?
    };
    let file = OwnedHandle(handle);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(file.0, &mut info)?;
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(format!(
            "Community storage contains a junction or symbolic link: {}",
            path.display()
        )
        .into());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 && info.nNumberOfLinks > 1 {
        return Err(format!(
            "Community storage contains a hard-linked file: {}",
            path.display()
        )
        .into());
    }
    Ok(file)
}
fn reject_reparse_ancestors(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 => {
                return Err(format!(
                    "Community storage contains a junction or symbolic link: {}",
                    ancestor.display()
                )
                .into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn validate_root(path: &Path) -> Result<PathBuf> {
    let path = absolute_storage_path(path)?;
    if path.parent().is_none() || path.file_name().is_none() {
        return Err("Choose a dedicated community data folder, not a drive root".into());
    }
    let mut protected: Vec<_> = [
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .collect();
    protected.push(std::env::current_dir()?);
    if let Some(parent) = std::env::current_exe()?.parent() {
        protected.push(parent.to_path_buf());
    }
    let key = path
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_lowercase();
    if protected.iter().any(|value| {
        absolute_storage_path(value).is_ok_and(|value| {
            value
                .to_string_lossy()
                .trim_end_matches(['\\', '/'])
                .to_lowercase()
                == key
        })
    }) {
        return Err(
            "Choose a dedicated community data folder, not a profile/system/application root"
                .into(),
        );
    }
    reject_reparse_ancestors(&path)?;
    Ok(path)
}
pub fn protect_data_directory(path: &Path) -> Result<()> {
    let root = validate_root(path)?;
    // Preflight all existing descendants before changing any ACL. Files linked
    // outside the selected community are rejected, not followed or modified.
    let mut paths = Vec::new();
    if root.exists() {
        let mut pending = vec![root.clone()];
        while let Some(path) = pending.pop() {
            let _ = checked_handle(&path, false)?;
            if path.is_dir() {
                for entry in fs::read_dir(&path)? {
                    pending.push(entry?.path());
                }
            }
            paths.push(path);
            if paths.len() + pending.len() > 100_000 {
                return Err(
                    "Community storage has too many files for safe startup validation".into(),
                );
            }
        }
    }
    let sid = current_user_sid()?;
    let acl = Descriptor::new(&format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid})"))?;
    fs::create_dir_all(&root)?;
    acl.apply(&root)?;
    for path in paths {
        if path != root {
            acl.apply(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let work = std::env::var_os("RELAY_TEST_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if package
                    .parent()
                    .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "outputs"))
                {
                    package.parent().unwrap().parent().unwrap().join("work")
                } else {
                    package.join("work")
                }
            });
        fs::create_dir_all(&work).unwrap();
        tempfile::Builder::new()
            .prefix("privacy-unit-")
            .tempdir_in(work)
            .unwrap()
    }
    #[test]
    fn refuses_broad_profile_and_drive_roots() {
        assert!(validate_root(Path::new("C:\\")).is_err());
        assert!(validate_root(Path::new(r"\\?\C:\")).is_err());
        let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        assert!(validate_root(&profile).is_err());
        let encoded = native_file_name(&profile).unwrap();
        let verbatim_profile = OsString::from_wide(&encoded[..encoded.len() - 1]);
        assert!(validate_root(Path::new(&verbatim_profile)).is_err());
        assert!(validate_root(&std::env::current_dir().unwrap()).is_err());
    }
    #[test]
    fn native_paths_are_lexical_utf16_and_support_drive_unc_and_verbatim_forms() {
        fn decode(path: &Path) -> String {
            let name = native_file_name(path).unwrap();
            assert_eq!(name.last(), Some(&0));
            String::from_utf16(&name[..name.len() - 1]).unwrap()
        }
        assert_eq!(
            decode(Path::new(r"C:/owned/folder/../community")),
            r"\\?\C:\owned\community"
        );
        assert_eq!(
            decode(Path::new(r"\\?\C:\owned\folder\..\community")),
            r"\\?\C:\owned\community"
        );
        assert_eq!(
            decode(Path::new(r"\\server\share\owned/../community")),
            r"\\?\UNC\server\share\community"
        );
        assert_eq!(
            decode(Path::new(r"\\?\UNC\server\share\owned\..\community")),
            r"\\?\UNC\server\share\community"
        );
        let mut units: Vec<u16> = r"C:\owned\".encode_utf16().collect();
        units.extend([0xd800, 0x4e2d]);
        let path = PathBuf::from(OsString::from_wide(&units));
        let encoded = native_file_name(&path).unwrap();
        assert_eq!(&encoded[4..encoded.len() - 1], units.as_slice());
        for unsafe_name in [
            r"\\.\C:\owned",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1",
            r"\\?\pipe\relay",
        ] {
            assert!(native_file_name(Path::new(unsafe_name)).is_err());
        }
    }
    fn long_data_path(fixture: &Path) -> PathBuf {
        let parent = fixture.join("owned nested path");
        let padding = 212usize
            .saturating_sub(parent.as_os_str().encode_wide().count() + 1)
            .max(16);
        parent.join("d".repeat(padding))
    }
    #[test]
    fn newly_created_long_backup_survives_repeated_private_startup() {
        let fixture = fixture();
        let data = long_data_path(fixture.path());
        protect_data_directory(&data).unwrap();
        let backup = data
            .join("backups")
            .join("pre-migration-00112233-4455-6677-8899-aabbccddeeff.sqlite3");
        assert!(backup.as_os_str().encode_wide().count() > 260);
        fs::create_dir_all(backup.parent().unwrap()).unwrap();
        fs::write(&backup, b"owned migration backup preserved").unwrap();
        Descriptor::new("D:P(A;OICI;FA;;;WD)")
            .unwrap()
            .apply(&backup)
            .unwrap();
        protect_data_directory(&data).unwrap();
        protect_data_directory(&data).unwrap();
        assert_eq!(
            fs::read(&backup).unwrap(),
            b"owned migration backup preserved"
        );
        let verbatim = native_file_name(&data).unwrap();
        protect_data_directory(Path::new(&OsString::from_wide(
            &verbatim[..verbatim.len() - 1],
        )))
        .unwrap();
        assert_eq!(
            fs::read(&backup).unwrap(),
            b"owned migration backup preserved"
        );
    }
    #[test]
    fn long_hard_link_is_rejected_before_external_file_changes() {
        let fixture = fixture();
        let data = long_data_path(fixture.path());
        fs::create_dir_all(data.join("backups")).unwrap();
        let outside = fixture.path().join("outside.txt");
        fs::write(&outside, b"outside sentinel").unwrap();
        let linked = data
            .join("backups")
            .join("pre-migration-00112233-4455-6677-8899-aabbccddeeff.sqlite3");
        assert!(linked.as_os_str().encode_wide().count() > 260);
        fs::hard_link(&outside, &linked).unwrap();
        let error = protect_data_directory(&data).unwrap_err();
        assert!(error.to_string().contains("hard-linked"));
        assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
        assert_eq!(fs::read(&linked).unwrap(), b"outside sentinel");
    }
    #[test]
    fn hard_link_rejection_preserves_external_file() {
        let fixture = fixture();
        let data = fixture.path().join("data");
        fs::create_dir(&data).unwrap();
        let outside = fixture.path().join("outside.txt");
        fs::write(&outside, b"outside sentinel").unwrap();
        fs::hard_link(&outside, data.join("linked.txt")).unwrap();
        let error = protect_data_directory(&data).unwrap_err();
        assert!(error.to_string().contains("hard-linked"));
        assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
    }
    #[test]
    fn existing_files_and_new_database_folder_can_be_protected() {
        let fixture = fixture();
        let data = fixture.path().join("data");
        fs::create_dir_all(data.join("uploads")).unwrap();
        fs::write(data.join("uploads/existing.txt"), b"preserved").unwrap();
        let permissive = Descriptor::new("D:P(A;OICI;FA;;;WD)").unwrap();
        permissive.apply(&data).unwrap();
        permissive
            .apply(&data.join("uploads/existing.txt"))
            .unwrap();
        protect_data_directory(&data).unwrap();
        assert_eq!(
            fs::read(data.join("uploads/existing.txt")).unwrap(),
            b"preserved"
        );
        fs::write(data.join("future.txt"), b"new private file").unwrap();
    }
}
