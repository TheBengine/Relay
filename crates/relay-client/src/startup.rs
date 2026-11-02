//! Per-user Windows startup. Only the managed personal installation registers.
use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Registry::*},
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const POLICY: &str = r"Software\Relay";
const VALUE: &str = "Relay";
const PREFERENCE: &str = "StartWithWindows";
const ERROR: &str = "Windows startup could not be changed. Check your Windows startup settings.";
struct State {
    command: String,
    enabled: bool,
    error: Option<String>,
}
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
pub fn allowed(
    personal: bool,
    installation: &relay_protocol::installation::InstallationState,
) -> bool {
    personal
        && matches!(installation, relay_protocol::installation::InstallationState::Ready { record: Some(record) }
        if record.app_id.eq_ignore_ascii_case("3bed570c-afb7-4f36-8a88-8c6f1772d6af"))
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn key(path: &str) -> Result<Key, String> {
    let path = wide(path);
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            0,
            PWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE | KEY_WOW64_64KEY,
            None,
            &mut key,
            None,
        )
    }
    .ok()
    .map_err(|_| ERROR.to_owned())?;
    Ok(Key(key))
}
fn read(key: &Key, name: &str) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>, String> {
    let name = wide(name);
    let mut kind = REG_VALUE_TYPE::default();
    let mut length = 0;
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut length),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    result.ok().map_err(|_| ERROR.to_owned())?;
    if length > 32768 {
        return Err(ERROR.into());
    }
    let mut bytes = vec![0; length as usize];
    unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut length),
        )
    }
    .ok()
    .map_err(|_| ERROR.to_owned())?;
    bytes.truncate(length as usize);
    Ok(Some((kind, bytes)))
}
fn write(key: &Key, name: &str, value: Option<&(REG_VALUE_TYPE, Vec<u8>)>) -> Result<(), String> {
    let name = wide(name);
    let result = match value {
        Some((kind, bytes)) => unsafe {
            RegSetValueExW(key.0, PCWSTR(name.as_ptr()), 0, *kind, Some(bytes))
        },
        None => unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) },
    };
    if value.is_none() && result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    result.ok().map_err(|_| ERROR.to_owned())
}
fn command(path: &Path) -> Result<String, String> {
    let path = path.to_str().ok_or(ERROR)?;
    if path.contains(['"', '\r', '\n', '\0']) {
        return Err(ERROR.into());
    }
    Ok(format!("\"{path}\" --start-in-tray"))
}
fn run_value(command: &str) -> (REG_VALUE_TYPE, Vec<u8>) {
    (
        REG_SZ,
        wide(command)
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect(),
    )
}
fn preference(value: Option<&(REG_VALUE_TYPE, Vec<u8>)>) -> Result<bool, String> {
    match value {
        None => Ok(true),
        Some((kind, bytes)) if *kind == REG_DWORD && bytes.as_slice() == 0u32.to_le_bytes() => {
            Ok(false)
        }
        Some((kind, bytes)) if *kind == REG_DWORD && bytes.as_slice() == 1u32.to_le_bytes() => {
            Ok(true)
        }
        _ => Err(ERROR.into()),
    }
}
fn owned(
    current: Option<&(REG_VALUE_TYPE, Vec<u8>)>,
    expected: &(REG_VALUE_TYPE, Vec<u8>),
) -> bool {
    current.is_none_or(|value| value == expected)
}
fn apply(command: &str, enabled: bool, save: bool) -> Result<(), String> {
    apply_at(command, enabled, save, RUN, POLICY)
}
fn apply_at(
    command: &str,
    enabled: bool,
    save: bool,
    run_path: &str,
    policy_path: &str,
) -> Result<(), String> {
    let run = key(run_path)?;
    let old = read(&run, VALUE)?;
    let expected = run_value(command);
    if !owned(old.as_ref(), &expected) {
        return Err(
            "Another Relay installation owns Windows startup. Its setting was kept.".into(),
        );
    }
    let desired = enabled.then_some(&expected);
    // Do not rewrite unchanged entries: Windows' own disabled-startup state is retained.
    if old.as_ref() != desired {
        write(&run, VALUE, desired)?;
    }
    if save {
        let saved = key(policy_path).and_then(|policy| {
            write(
                &policy,
                PREFERENCE,
                Some(&(REG_DWORD, u32::from(enabled).to_le_bytes().to_vec())),
            )
        });
        if let Err(error) = saved {
            let _ = write(&run, VALUE, old.as_ref());
            return Err(error);
        }
    }
    Ok(())
}
pub fn initialize(managed_personal_installation: bool) {
    if !managed_personal_installation {
        return;
    }
    let Ok(command) = std::env::current_exe()
        .map_err(|_| ERROR.to_owned())
        .and_then(|path| command(&path))
    else {
        return;
    };
    let result = key(POLICY)
        .and_then(|key| read(&key, PREFERENCE))
        .and_then(|value| preference(value.as_ref()));
    let enabled = result.as_ref().copied().unwrap_or(false);
    let error = result.and_then(|_| apply(&command, enabled, false)).err();
    let _ = STATE.set(Mutex::new(State {
        command,
        enabled,
        error,
    }));
}
pub fn status() -> Option<(bool, Option<String>)> {
    let state = STATE.get()?.lock().ok()?;
    Some((state.enabled, state.error.clone()))
}
pub fn set_enabled(enabled: bool) {
    let Some(mut state) = STATE.get().and_then(|state| state.lock().ok()) else {
        return;
    };
    state.error = apply(&state.command, enabled, true).err();
    if state.error.is_none() {
        state.enabled = enabled;
    }
}
pub fn unregister_current() -> Result<(), String> {
    let path = std::env::current_exe().map_err(|_| ERROR.to_owned())?;
    // Keep the user's opt-out for a later reinstall, and preserve foreign entries.
    apply(&command(&path)?, false, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn isolated_registry_roundtrip_preserves_opt_out_and_foreign_installations() {
        struct Fixture(String);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let path = wide(&self.0);
                unsafe {
                    let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()));
                }
            }
        }
        let fixture = Fixture(format!(
            r"Software\RelayStartupTest-{}",
            uuid::Uuid::new_v4()
        ));
        let run_path = format!(r"{}\Run", fixture.0);
        let policy_path = format!(r"{}\Preferences", fixture.0);
        let run = key(&run_path).unwrap();
        let policy = key(&policy_path).unwrap();
        let command = command(Path::new(r"C:\Relay startup test\relay.exe")).unwrap();
        let enabled = preference(read(&policy, PREFERENCE).unwrap().as_ref()).unwrap();
        apply_at(&command, enabled, false, &run_path, &policy_path).unwrap();
        assert_eq!(read(&run, VALUE).unwrap(), Some(run_value(&command)));
        apply_at(&command, false, true, &run_path, &policy_path).unwrap();
        assert!(read(&run, VALUE).unwrap().is_none());
        // Simulated restart/update reads the persisted preference.
        let enabled = preference(read(&policy, PREFERENCE).unwrap().as_ref()).unwrap();
        assert!(!enabled);
        apply_at(&command, enabled, false, &run_path, &policy_path).unwrap();
        assert!(read(&run, VALUE).unwrap().is_none());
        apply_at(&command, true, true, &run_path, &policy_path).unwrap();
        assert_eq!(read(&run, VALUE).unwrap(), Some(run_value(&command)));
        let foreign = run_value(r#""D:\Another Relay\relay.exe" --start-in-tray"#);
        write(&run, VALUE, Some(&foreign)).unwrap();
        for enabled in [true, false] {
            assert!(apply_at(&command, enabled, true, &run_path, &policy_path).is_err());
            assert_eq!(read(&run, VALUE).unwrap(), Some(foreign.clone()));
        }
    }
    #[test]
    fn portable_and_isolated_profiles_never_register() {
        use relay_protocol::installation::{InstallationRecord, InstallationState};
        let installed = InstallationState::Ready {
            record: Some(InstallationRecord {
                app_id: "3bed570c-afb7-4f36-8a88-8c6f1772d6af".into(),
                version: "0.10.0".into(),
                sequence: "1".into(),
                payload_sha256: String::new(),
                schema: None,
            }),
        };
        assert!(allowed(true, &installed));
        assert!(!allowed(false, &installed));
        assert!(!allowed(true, &InstallationState::Ready { record: None }));
        assert!(!allowed(
            true,
            &InstallationState::RepairRequired {
                reason: "incomplete"
            }
        ));
    }
    #[test]
    fn new_and_existing_installs_default_on_but_explicit_opt_out_survives() {
        assert_eq!(preference(None), Ok(true));
        assert_eq!(
            preference(Some(&(REG_DWORD, 0u32.to_le_bytes().to_vec()))),
            Ok(false)
        );
        assert_eq!(
            preference(Some(&(REG_DWORD, 1u32.to_le_bytes().to_vec()))),
            Ok(true)
        );
        assert!(preference(Some(&(REG_SZ, vec![1]))).is_err());
    }
    #[test]
    fn command_quotes_spaces_and_starts_hidden() {
        assert_eq!(
            command(Path::new(r"C:\Program Files\Relay\dist\relay.exe")).unwrap(),
            r#""C:\Program Files\Relay\dist\relay.exe" --start-in-tray"#
        );
        assert!(command(Path::new("C:\\bad\"path.exe")).is_err());
    }
    #[test]
    fn only_absent_or_exactly_owned_run_entries_can_change() {
        let own = run_value(r#""C:\Relay\relay.exe" --start-in-tray"#);
        let other = run_value(r#""D:\Relay\relay.exe" --start-in-tray"#);
        assert!(owned(None, &own));
        assert!(owned(Some(&own), &own));
        assert!(!owned(Some(&other), &own));
    }
}
