//! Native adapter: direct key handles, bounded snapshots and staged publication.
use super::*;
use crate::instance_identity::{self, Identity, Lease};
use std::{cell::RefCell, os::windows::io::OwnedHandle};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, HANDLE},
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, QueryDosDeviceW, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::Registry::*,
        UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST},
    },
};

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn wide(text: &str) -> Vec<u16> {
    instance_identity::wide(text)
}
fn checked_key(key: Key) -> Result<Key, ()> {
    let name = wide("SymbolicLinkValue");
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0;
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result.is_ok() && kind != REG_LINK {
        Ok(key)
    } else {
        Err(())
    }
}
fn child(parent: HKEY, name: &str) -> Result<Option<Key>, ()> {
    child_direct(parent, name)?.map(checked_key).transpose()
}
fn child_direct(parent: HKEY, name: &str) -> Result<Option<Key>, ()> {
    if name.is_empty() || name.contains('\\') || name.contains('\0') {
        return Err(());
    }
    let name = wide(name);
    let mut key = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            parent,
            PCWSTR(name.as_ptr()),
            REG_OPTION_OPEN_LINK.0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    result.ok().map_err(|_| ())?;
    Ok(Some(Key(key)))
}

fn user_classes(identity: &Identity) -> Result<Key, ()> {
    let software = child(HKEY_CURRENT_USER, "Software")?.ok_or(())?;
    let classes = child_direct(software.0, "Classes")?.ok_or(())?;
    let name = wide("SymbolicLinkValue");
    let mut kind = REG_VALUE_TYPE::default();
    let mut body = [0u8; 4096];
    let mut length = body.len() as u32;
    let status = unsafe {
        RegQueryValueExW(
            classes.0,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(body.as_mut_ptr()),
            Some(&mut length),
        )
    };
    if status == ERROR_FILE_NOT_FOUND || status.is_ok() && kind != REG_LINK {
        return checked_key(classes);
    }
    status.ok().map_err(|_| ())?;
    if !length.is_multiple_of(2) || length as usize > body.len() {
        return Err(());
    }
    let mut words: Vec<_> = body[..length as usize]
        .chunks_exact(2)
        .map(|n| u16::from_le_bytes([n[0], n[1]]))
        .collect();
    if words.last() == Some(&0) {
        words.pop();
    }
    let target = String::from_utf16(&words).map_err(|_| ())?;
    let sid = identity.account_sid().map_err(|_| ())?;
    let expected = format!(r"\Registry\User\{sid}_Classes");
    if !target.eq_ignore_ascii_case(&expected) {
        return Err(());
    }
    // Windows implements HKCU\Software\Classes as this OS-owned hive alias.
    // Verify it against the process token, then open that exact user hive;
    // never follow an arbitrary SymbolicLinkValue supplied by a registry key.
    child(HKEY_USERS, &format!("{sid}_Classes"))?.ok_or(())
}
fn descend(root: HKEY, path: &[&str]) -> Result<Option<Key>, ()> {
    let mut current = None;
    for name in path {
        let parent = current.as_ref().map_or(root, |key: &Key| key.0);
        let Some(next) = child(parent, name)? else {
            return Ok(None);
        };
        current = Some(next);
    }
    Ok(current)
}

fn entries(key: HKEY) -> Result<(BTreeMap<String, Value>, Vec<String>), ()> {
    let mut values = 0;
    let mut subkeys = 0;
    unsafe {
        RegQueryInfoKeyW(
            key,
            PWSTR::null(),
            None,
            None,
            Some(&mut subkeys),
            None,
            None,
            Some(&mut values),
            None,
            None,
            None,
            None,
        )
    }
    .ok()
    .map_err(|_| ())?;
    if values > 32 || subkeys > 32 {
        return Err(());
    }
    let mut found = BTreeMap::new();
    for index in 0..values {
        let mut name = [0u16; 256];
        let mut length = name.len() as u32;
        let mut kind = 0;
        let mut body = [0u8; 8192];
        let mut size = body.len() as u32;
        unsafe {
            RegEnumValueW(
                key,
                index,
                PWSTR(name.as_mut_ptr()),
                &mut length,
                None,
                Some(&mut kind),
                Some(body.as_mut_ptr()),
                Some(&mut size),
            )
        }
        .ok()
        .map_err(|_| ())?;
        if length as usize >= name.len() || size as usize > body.len() || kind == REG_LINK.0 {
            return Err(());
        }
        let name = String::from_utf16(&name[..length as usize])
            .map_err(|_| ())?
            .to_ascii_lowercase();
        if found
            .insert(
                name,
                Value {
                    kind,
                    bytes: body[..size as usize].to_vec(),
                },
            )
            .is_some()
        {
            return Err(());
        }
    }
    let mut names = Vec::new();
    for index in 0..subkeys {
        let mut name = [0u16; 256];
        let mut length = name.len() as u32;
        unsafe {
            RegEnumKeyExW(
                key,
                index,
                PWSTR(name.as_mut_ptr()),
                &mut length,
                None,
                PWSTR::null(),
                None,
                None,
            )
        }
        .ok()
        .map_err(|_| ())?;
        if length == 0 || length as usize >= name.len() {
            return Err(());
        }
        let name = String::from_utf16(&name[..length as usize])
            .map_err(|_| ())?
            .to_ascii_lowercase();
        if name.contains(['\\', '\0']) || names.contains(&name) {
            return Err(());
        }
        names.push(name);
    }
    names.sort();
    Ok((found, names))
}
struct Scan {
    tree: Tree,
    keys: BTreeMap<String, Key>,
    bytes: usize,
}
impl Scan {
    fn add(&mut self, path: String, key: Key) -> Result<(), ()> {
        if self.keys.len() >= 32 || path.matches('\\').count() > 8 {
            return Err(());
        }
        let (values, names) = entries(key.0)?;
        self.bytes += values
            .iter()
            .map(|(name, value)| name.len() + value.bytes.len())
            .sum::<usize>();
        if self.bytes > 65536 {
            return Err(());
        }
        let parent = key.0;
        self.keys.insert(path.clone(), key);
        self.tree.insert(path.clone(), values);
        for name in names {
            let key = child(parent, &name)?.ok_or(())?;
            let next = if path.is_empty() {
                name
            } else {
                format!("{path}\\{name}")
            };
            self.add(next, key)?;
        }
        Ok(())
    }
}
fn scan(parent: HKEY, scheme: &str) -> Result<Option<Scan>, ()> {
    let Some(root) = child(parent, scheme)? else {
        return Ok(None);
    };
    let mut scan = Scan {
        tree: Tree::new(),
        keys: BTreeMap::new(),
        bytes: 0,
    };
    scan.add(String::new(), root)?;
    Ok(Some(scan))
}

struct Probe {
    state: FileState,
    _pins: Vec<OwnedHandle>,
}
fn probe(path: &str) -> Probe {
    let mut pins = Vec::new();
    let state = (|| -> Result<FileState, ()> {
        let path = executable_path(path).ok_or(())?;
        let drive = wide(&path[..2]);
        let mut devices = [0u16; 512];
        let length = unsafe { QueryDosDeviceW(PCWSTR(drive.as_ptr()), Some(&mut devices)) };
        if length == 0 {
            return Err(());
        }
        let end = devices.iter().position(|n| *n == 0).ok_or(())?;
        let device = String::from_utf16(&devices[..end]).map_err(|_| ())?;
        let suffix = device.strip_prefix(r"\Device\HarddiskVolume").ok_or(())?;
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        let mut ancestors = vec![path[..3].to_owned()];
        let mut current = path[..3].to_owned();
        for part in path[3..].split('\\') {
            if !current.ends_with('\\') {
                current.push('\\');
            }
            current.push_str(part);
            ancestors.push(current.clone());
        }
        for (index, ancestor) in ancestors.iter().enumerate() {
            let name = wide(ancestor);
            let result = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    FILE_READ_ATTRIBUTES.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                    HANDLE::default(),
                )
            };
            let handle = match result {
                Ok(handle) => unsafe { instance_identity::own(handle) },
                Err(error) if index > 0 && matches!(error.code().0 & 0xffff, 2 | 3) => {
                    return Ok(FileState::Missing)
                }
                Err(_) => return Err(()),
            };
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            unsafe { GetFileInformationByHandle(instance_identity::raw(&handle), &mut info) }
                .map_err(|_| ())?;
            let last = index == ancestors.len() - 1;
            if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0) == last
            {
                return Err(());
            }
            pins.push(handle);
        }
        Ok(FileState::Present)
    })()
    .unwrap_or(FileState::Unknown);
    Probe { state, _pins: pins }
}

fn write_key(direct: &Key) -> Result<Key, ()> {
    let empty = wide("");
    let mut key = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            direct.0,
            PCWSTR(empty.as_ptr()),
            0,
            KEY_READ | KEY_WRITE | REG_SAM_FLAGS(0x10000) | KEY_WOW64_64KEY,
            &mut key,
        )
    }
    .ok()
    .map_err(|_| ())?;
    checked_key(Key(key))
}
fn create_key(parent: HKEY, name: &str, class: &str) -> Result<Key, ()> {
    let name = wide(name);
    let mut class = wide(class);
    let mut key = HKEY::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION::default();
    unsafe {
        RegCreateKeyExW(
            parent,
            PCWSTR(name.as_ptr()),
            0,
            PWSTR(class.as_mut_ptr()),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE | REG_SAM_FLAGS(0x10000) | KEY_WOW64_64KEY,
            None,
            &mut key,
            Some(&mut disposition),
        )
    }
    .ok()
    .map_err(|_| ())?;
    let key = Key(key);
    if disposition != REG_CREATED_NEW_KEY {
        return Err(());
    }
    checked_key(key)
}
fn read_class(key: &Key) -> Result<String, ()> {
    let mut text = [0u16; 256];
    let mut length = text.len() as u32;
    unsafe {
        RegQueryInfoKeyW(
            key.0,
            PWSTR(text.as_mut_ptr()),
            Some(&mut length),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }
    .ok()
    .map_err(|_| ())?;
    if length as usize >= text.len() {
        return Err(());
    }
    String::from_utf16(&text[..length as usize]).map_err(|_| ())
}
fn staging_class(scheme: &str, owner: &Owner) -> String {
    format!("Relay.Invitation.v1|{scheme}|{}", owner.path_sha256)
}
fn staging_owner_hash(key: &Key, scheme: &str) -> Result<String, ()> {
    let text = read_class(key)?;
    let prefix = format!("Relay.Invitation.v1|{scheme}|");
    let hash = text.strip_prefix(&prefix).ok_or(())?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(());
    }
    Ok(hash.into())
}
fn rename(key: &Key, name: &str) -> Result<(), ()> {
    let name = wide(name);
    unsafe { RegRenameKey(key.0, PCWSTR::null(), PCWSTR(name.as_ptr())) }
        .ok()
        .map_err(|_| ())
}
fn parent<'a>(path: &'a str, root: &'a str) -> (&'a str, &'a str) {
    if path.is_empty() {
        ("", root)
    } else {
        path.rsplit_once('\\').unwrap_or(("", path))
    }
}
/// This only touches a private preparation/retirement name made by this call.
/// A changed tree is preserved. No RegDeleteTree or recursive registry deletion.
fn erase_exact(classes: HKEY, name: &str, expected: &Tree) -> Result<(), ()> {
    let found = scan(classes, name)?.ok_or(())?;
    if found.tree != *expected {
        return Err(());
    }
    let mut keys = BTreeMap::new();
    for (path, key) in found.keys {
        keys.insert(path, write_key(&key)?);
    }
    // Keep the ownership marker until all other values are gone. A process
    // crash during cleanup then leaves either a verifiable subset of the
    // owned tree, or empty known containers with atomic class metadata.
    let mut values: Vec<_> = expected
        .iter()
        .flat_map(|(path, node)| node.iter().map(move |(name, value)| (path, name, value)))
        .collect();
    values.sort_by_key(|(path, name, _)| path.is_empty() && name.as_str() == MARKER);
    for (path, name, value) in values {
        if entries(keys.get(path).ok_or(())?.0)?.0.get(name) != Some(value) {
            return Err(());
        }
        let name = wide(name);
        unsafe { RegDeleteValueW(keys.get(path).ok_or(())?.0, PCWSTR(name.as_ptr())) }
            .ok()
            .map_err(|_| ())?;
    }
    let mut order: Vec<_> = keys.keys().cloned().collect();
    order.sort_by_key(|key| std::cmp::Reverse(key.len()));
    for path in order {
        let (values, children) = entries(keys.get(&path).ok_or(())?.0)?;
        if !values.is_empty() || !children.is_empty() {
            return Err(());
        }
        let (parent, child) = parent(&path, name);
        let parent = if path.is_empty() {
            classes
        } else {
            keys.get(parent).ok_or(())?.0
        };
        let child = wide(child);
        unsafe { RegDeleteKeyExW(parent, PCWSTR(child.as_ptr()), KEY_WOW64_64KEY.0, 0) }
            .ok()
            .map_err(|_| ())?;
        keys.remove(&path);
    }
    Ok(())
}
struct Prepared {
    classes: HKEY,
    name: String,
    root: Option<Key>,
    expected: Tree,
    armed: bool,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        self.root.take();
        if self.armed {
            let _ = erase_exact(self.classes, &self.name, &self.expected);
        }
    }
}
struct Store {
    scheme: String,
    classes: Key,
    _lease: Lease,
    probes: RefCell<BTreeMap<String, Probe>>,
    #[cfg(test)]
    fail_after: Option<usize>,
    #[cfg(test)]
    temporary_names: RefCell<Vec<String>>,
    #[cfg(test)]
    crash_after: Option<usize>,
}
impl Store {
    fn open(scheme: &str) -> Result<Self, &'static str> {
        if scheme != "relay"
            && !(scheme.starts_with("relay-fixture-")
                && uuid::Uuid::parse_str(&scheme[14..]).is_ok())
        {
            return Err("Invalid owned protocol name");
        }
        let identity = Identity::current().map_err(|_| "Windows account identity")?;
        let lease = identity
            .protocol_lease(scheme)
            .map_err(|_| "Per-account registration lock")?;
        let classes = user_classes(&identity).map_err(|_| "Direct user classes path")?;
        Ok(Self {
            scheme: scheme.into(),
            classes,
            _lease: lease,
            probes: RefCell::new(BTreeMap::new()),
            #[cfg(test)]
            fail_after: None,
            #[cfg(test)]
            temporary_names: RefCell::new(Vec::new()),
            #[cfg(test)]
            crash_after: None,
        })
    }
    fn step(&self, count: &mut usize) -> Result<(), ()> {
        #[cfg(test)]
        if self.crash_after == Some(*count) {
            std::process::exit(17);
        }
        #[cfg(test)]
        if self.fail_after == Some(*count) {
            return Err(());
        }
        *count += 1;
        Ok(())
    }
    fn temporary(&self, retired: bool) -> String {
        format!(
            "{}.relay-{}-v1",
            self.scheme,
            if retired { "retired" } else { "prepared" }
        )
    }
    fn recover_slots(&self, owner_limit: Option<&str>) -> Result<(), ()> {
        for retired in [false, true] {
            let name = self.temporary(retired);
            let Some(found) = scan(self.classes.0, &name)? else {
                continue;
            };
            let hash = staging_owner_hash(found.keys.get("").ok_or(())?, &self.scheme)?;
            if owner_limit.is_some_and(|limit| limit != hash) {
                return Err(());
            }
            if let Some(owner) = Owner::from_tree(&found.tree) {
                if owner.path_sha256 != hash {
                    return Err(());
                }
                let expected = owner.values();
                if !found.tree.iter().all(|(key, node)| {
                    expected.get(key).is_some_and(|values| {
                        node.iter()
                            .all(|(name, value)| values.get(name) == Some(value))
                    })
                }) {
                    return Err(());
                }
            } else if !found.tree.iter().all(|(key, node)| {
                node.is_empty()
                    && ["", "defaulticon", "shell", "shell\\open", COMMAND].contains(&key.as_str())
            }) {
                return Err(());
            }
            let tree = found.tree;
            drop(found.keys);
            erase_exact(self.classes.0, &name, &tree)?;
        }
        Ok(())
    }
    fn prepare(&self, tree: &Tree, count: &mut usize) -> Result<Prepared, ()> {
        let name = self.temporary(false);
        let owner = Owner::from_tree(tree).ok_or(())?;
        // Class metadata is created atomically with the root. Even a crash
        // before the first value leaves bounded, verifiable owned preparation.
        let root = create_key(self.classes.0, &name, &staging_class(&self.scheme, &owner))?;
        #[cfg(test)]
        self.temporary_names.borrow_mut().push(name.clone());
        let mut prepared = Prepared {
            classes: self.classes.0,
            name,
            root: Some(root),
            expected: [(String::new(), BTreeMap::new())].into(),
            armed: true,
        };
        let mut keys = BTreeMap::new();
        let mut order: Vec<_> = tree
            .keys()
            .filter(|path| !path.is_empty())
            .cloned()
            .collect();
        order.sort_by_key(|path| path.len());
        for path in order {
            let (parent, name) = parent(&path, "");
            let parent = if parent.is_empty() {
                prepared.root.as_ref().ok_or(())?.0
            } else {
                keys.get(parent).map(|key: &Key| key.0).ok_or(())?
            };
            let key = create_key(parent, name, "")?;
            prepared.expected.insert(path.clone(), BTreeMap::new());
            keys.insert(path, key);
        }
        let mut values: Vec<_> = tree
            .iter()
            .flat_map(|(path, node)| node.iter().map(move |(name, value)| (path, name, value)))
            .collect();
        values.sort_by_key(|(path, name, _)| {
            if path.is_empty() && name.as_str() == MARKER {
                0
            } else {
                1
            }
        });
        for (path, name, value) in values {
            self.step(count)?;
            let handle = if path.is_empty() {
                prepared.root.as_ref().ok_or(())?.0
            } else {
                keys.get(path).ok_or(())?.0
            };
            let wide_name = wide(name);
            unsafe {
                RegSetValueExW(
                    handle,
                    PCWSTR(wide_name.as_ptr()),
                    0,
                    REG_VALUE_TYPE(value.kind),
                    Some(&value.bytes),
                )
            }
            .ok()
            .map_err(|_| ())?;
            prepared
                .expected
                .get_mut(path)
                .ok_or(())?
                .insert(name.clone(), value.clone());
        }
        drop(keys);
        if scan(self.classes.0, &prepared.name)?.ok_or(())?.tree != *tree {
            return Err(());
        }
        Ok(prepared)
    }
    fn mutate(&mut self, before: &Snapshot, next: Option<Tree>) -> Result<(), ()> {
        if self.snapshot()? != *before {
            return Err(());
        }
        let removed_owner = if next.is_none() {
            before.user.as_ref().and_then(Owner::from_tree)
        } else {
            None
        };
        self.recover_slots(
            removed_owner
                .as_ref()
                .map(|owner| owner.path_sha256.as_str()),
        )?;
        let mut count = 0;
        let mut prepared = next
            .as_ref()
            .map(|tree| self.prepare(tree, &mut count))
            .transpose()?;
        if self.snapshot()? != *before {
            return Err(());
        }
        let previous = scan(self.classes.0, &self.scheme)?;
        if previous.as_ref().map(|scan| &scan.tree) != before.user.as_ref() {
            return Err(());
        }
        self.step(&mut count)?;
        let mut retired = if let Some(previous) = previous {
            let expected = previous.tree;
            let root = write_key(previous.keys.get("").ok_or(())?)?;
            // Whole-tree move is permitted only for an exact known-owned tree.
            let owner = Owner::from_tree(&expected).ok_or(())?;
            if expected != owner.values() {
                return Err(());
            }
            if read_class(&root)? != staging_class(&self.scheme, &owner) {
                return Err(());
            }
            let name = self.temporary(true);
            rename(&root, &name)?;
            #[cfg(test)]
            self.temporary_names.borrow_mut().push(name.clone());
            Some(Prepared {
                classes: self.classes.0,
                name,
                root: Some(root),
                expected,
                armed: true,
            })
        } else {
            None
        };
        if let Some(prepared) = prepared.as_mut() {
            let publish = (|| {
                self.step(&mut count)?;
                rename(prepared.root.as_ref().ok_or(())?, &self.scheme)
            })();
            if publish.is_err() {
                if let Some(retired) = retired.as_mut() {
                    if rename(retired.root.as_ref().ok_or(())?, &self.scheme).is_ok() {
                        retired.armed = false;
                    }
                }
                return Err(());
            }
            prepared.armed = false;
        }
        // From here the complete canonical state is committed. Cleanup cannot
        // undo it or remove a new handler which another installation creates.
        #[cfg(test)]
        if self.crash_after == Some(9) {
            std::process::exit(17);
        }
        drop(retired);
        drop(prepared);
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
        Ok(())
    }
}
impl Registry for Store {
    fn snapshot(&mut self) -> Result<Snapshot, ()> {
        let found = scan(self.classes.0, &self.scheme)?;
        if let Some(found) = &found {
            if let Some(owner) = Owner::from_tree(&found.tree) {
                if read_class(found.keys.get("").ok_or(())?)? != staging_class(&self.scheme, &owner)
                {
                    return Err(());
                }
            }
        }
        let user = found.map(|scan| scan.tree);
        let effective = scan(HKEY_CLASSES_ROOT, &self.scheme)?.map(|scan| scan.tree);
        let machine = descend(HKEY_LOCAL_MACHINE, &["Software", "Classes"])?;
        let machine_present = match machine {
            Some(key) => child(key.0, &self.scheme)?.is_some(),
            None => false,
        };
        let user_choice_present = descend(
            HKEY_CURRENT_USER,
            &[
                "Software",
                "Microsoft",
                "Windows",
                "Shell",
                "Associations",
                "UrlAssociations",
                &self.scheme,
                "UserChoice",
            ],
        )?
        .is_some();
        Ok(Snapshot {
            user,
            effective,
            machine_present,
            user_choice_present,
        })
    }
    fn executable(&self, path: &str) -> FileState {
        let mut probes = self.probes.borrow_mut();
        probes
            .entry(path.into())
            .or_insert_with(|| probe(path))
            .state
    }
    fn commit(&mut self, before: &Snapshot, next: Option<Tree>) -> Result<(), ()> {
        self.mutate(before, next)
    }
}
fn current(remove: bool) -> Outcome {
    let Some(owner) = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().and_then(Owner::new))
    else {
        return Outcome::Unavailable;
    };
    let Ok(mut store) = Store::open("relay") else {
        return Outcome::Unavailable;
    };
    if remove {
        if store.recover_slots(Some(&owner.path_sha256)).is_err() {
            return Outcome::Unavailable;
        }
        unregister(&mut store, &owner)
    } else {
        let result = register(&mut store, &owner);
        if result == Outcome::Ready {
            let _ = store.recover_slots(None);
        }
        result
    }
}
pub fn register_current() -> Outcome {
    current(false)
}
pub fn unregister_current() -> Outcome {
    current(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> Owner {
        Owner::new(std::env::current_exe().unwrap().to_str().unwrap()).unwrap()
    }
    struct Fixture {
        store: Store,
        owner: Owner,
    }
    impl Fixture {
        fn new() -> Self {
            let scheme = format!("relay-fixture-{}", uuid::Uuid::new_v4());
            Self {
                store: Store::open(&scheme).unwrap(),
                owner: owner(),
            }
        }
        fn assert_no_temporary_keys(&self) {
            for name in self.store.temporary_names.borrow().iter() {
                assert!(
                    child(self.store.classes.0, name).unwrap().is_none(),
                    "Owned preparation/retirement key remained"
                );
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.store.fail_after = None;
            let _ = unregister(&mut self.store, &self.owner);
        }
    }
    #[test]
    fn real_synthetic_handler_is_complete_quoted_effective_and_removed_without_touching_relay() {
        let mut fixture = Fixture::new();
        let owner = fixture.owner.clone();
        assert_eq!(fixture.store.snapshot().unwrap().user, None);
        fixture.assert_no_temporary_keys();
        assert_eq!(register(&mut fixture.store, &owner), Outcome::Ready);
        let snapshot = fixture.store.snapshot().unwrap();
        assert_eq!(snapshot.user, Some(owner.values()));
        assert_eq!(snapshot.user, snapshot.effective);
        assert_eq!(register(&mut fixture.store, &owner), Outcome::Ready);
        assert_eq!(unregister(&mut fixture.store, &owner), Outcome::Removed);
        assert_eq!(fixture.store.snapshot().unwrap().user, None);
    }
    #[test]
    fn native_failed_writes_roll_back_created_keys_and_the_old_handler() {
        let mut fixture = Fixture::new();
        let owner = fixture.owner.clone();
        for step in 0..8 {
            fixture.store.fail_after = Some(step);
            assert_eq!(register(&mut fixture.store, &owner), Outcome::Unavailable);
            assert_eq!(fixture.store.snapshot().unwrap().user, None);
            fixture.assert_no_temporary_keys();
        }
        fixture.store.fail_after = None;
        assert_eq!(register(&mut fixture.store, &owner), Outcome::Ready);
        fixture.store.fail_after = Some(0);
        assert_eq!(unregister(&mut fixture.store, &owner), Outcome::Unavailable);
        assert_eq!(fixture.store.snapshot().unwrap().user, Some(owner.values()));
        fixture.store.fail_after = None;
    }
    #[test]
    fn native_stale_transfer_rolls_back_prepublication_failures_and_old_uninstall_preserves_new() {
        let mut fixture = Fixture::new();
        let owner = fixture.owner.clone();
        let old = Owner::new(
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!("missing-{}.exe", uuid::Uuid::new_v4()))
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let before = fixture.store.snapshot().unwrap();
        fixture.store.commit(&before, Some(old.values())).unwrap();
        for step in 0..9 {
            fixture.store.fail_after = Some(step);
            assert_eq!(register(&mut fixture.store, &owner), Outcome::Unavailable);
            assert_eq!(fixture.store.snapshot().unwrap().user, Some(old.values()));
            fixture.assert_no_temporary_keys();
        }
        fixture.store.fail_after = None;
        assert_eq!(register(&mut fixture.store, &owner), Outcome::Ready);
        assert_eq!(unregister(&mut fixture.store, &old), Outcome::Preserved);
        assert_eq!(fixture.store.snapshot().unwrap().user, Some(owner.values()));
        assert_eq!(unregister(&mut fixture.store, &owner), Outcome::Removed);
        fixture.assert_no_temporary_keys();
    }
    #[test]
    fn native_uninstall_preserves_a_changed_tree_including_foreign_values() {
        let mut fixture = Fixture::new();
        let owner = fixture.owner.clone();
        let mut changed = owner.values();
        changed
            .get_mut("")
            .unwrap()
            .insert("fixture-foreign-value".into(), Value::text("keep"));
        let before = fixture.store.snapshot().unwrap();
        fixture
            .store
            .commit(&before, Some(changed.clone()))
            .unwrap();
        assert_eq!(unregister(&mut fixture.store, &owner), Outcome::Preserved);
        assert_eq!(
            fixture.store.snapshot().unwrap().user,
            Some(changed.clone())
        );
        // These extra bytes belong to this synthetic test, never a real app.
        erase_exact(fixture.store.classes.0, &fixture.store.scheme, &changed).unwrap();
        assert_eq!(fixture.store.snapshot().unwrap().user, None);
        fixture.assert_no_temporary_keys();
    }
    #[test]
    fn file_probe_pins_only_direct_local_files_and_does_not_follow_a_directory_link() {
        let owner = owner();
        assert_eq!(probe(&owner.executable).state, FileState::Present);
        assert_eq!(probe(r"\\server\share\relay.exe").state, FileState::Unknown);
        let root = std::env::temp_dir().join(format!("protocol-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        assert_eq!(
            probe(root.join("missing.exe").to_str().unwrap()).state,
            FileState::Missing
        );
        let redirect = root.join("redirect");
        if std::os::windows::fs::symlink_dir(&root, &redirect).is_ok() {
            assert_eq!(
                probe(redirect.join("not-followed.exe").to_str().unwrap()).state,
                FileState::Unknown
            );
            std::fs::remove_dir(&redirect).unwrap();
        }
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    #[ignore = "Only an isolated subprocess with a fresh owned fixture scheme"]
    fn protocol_crash_fixture_child() {
        let scheme = std::env::var("RELAY_PROTOCOL_CRASH_SCHEME").unwrap();
        assert!(scheme.starts_with("relay-fixture-"));
        let mut store = Store::open(&scheme).unwrap();
        let owner = owner();
        if std::env::var("RELAY_PROTOCOL_CRASH_STALE").unwrap() == "1" {
            let old = Owner::new(
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join(format!("missing-{scheme}.exe"))
                    .to_str()
                    .unwrap(),
            )
            .unwrap();
            let before = store.snapshot().unwrap();
            store.commit(&before, Some(old.values())).unwrap();
        }
        store.crash_after = Some(
            std::env::var("RELAY_PROTOCOL_CRASH_STEP")
                .unwrap()
                .parse()
                .unwrap(),
        );
        let _ = register(&mut store, &owner);
        panic!("The fixture must exit at its selected crash boundary");
    }

    #[test]
    fn native_process_crash_recovers_bounded_prepared_retired_and_published_states() {
        use std::{
            os::windows::process::CommandExt,
            process::{Child, Command},
            time::{Duration, Instant},
        };
        struct OwnChild(Child);
        impl Drop for OwnChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for (step, stale) in [(0, false), (7, false), (8, false), (8, true), (9, true)] {
            let scheme = format!("relay-fixture-{}", uuid::Uuid::new_v4());
            let mut child = OwnChild(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "url_protocol::native::tests::protocol_crash_fixture_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("RELAY_PROTOCOL_CRASH_SCHEME", &scheme)
                    .env("RELAY_PROTOCOL_CRASH_STALE", if stale { "1" } else { "0" })
                    .env("RELAY_PROTOCOL_CRASH_STEP", step.to_string())
                    .creation_flags(0x08000000)
                    .spawn()
                    .unwrap(),
            );
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    assert_eq!(status.code(), Some(17));
                    break;
                }
                assert!(
                    Instant::now() < until,
                    "Owned crash fixture exceeded its deadline"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(child);
            let mut fixture = Fixture {
                store: Store::open(&scheme).unwrap(),
                owner: owner(),
            };
            let owner = fixture.owner.clone();
            let user = fixture.store.snapshot().unwrap().user;
            assert!(user.is_none() || user == Some(owner.values()));
            assert_eq!(register(&mut fixture.store, &owner), Outcome::Ready);
            fixture.store.recover_slots(None).unwrap();
            for retired in [false, true] {
                assert!(
                    child_direct(fixture.store.classes.0, &fixture.store.temporary(retired))
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(fixture.store.snapshot().unwrap().user, Some(owner.values()));
            assert_eq!(unregister(&mut fixture.store, &owner), Outcome::Removed);
        }
    }

    #[test]
    fn interrupted_cleanup_subsets_are_recoverable_until_and_after_the_last_marker() {
        let fixture = Fixture::new();
        let tree = fixture.owner.values();
        let mut values: Vec<_> = tree
            .iter()
            .flat_map(|(path, node)| node.keys().map(move |name| (path, name)))
            .collect();
        values.sort_by_key(|(path, name)| path.is_empty() && name.as_str() == MARKER);
        for removed in 0..=values.len() {
            let mut prepared = fixture.store.prepare(&tree, &mut 0).unwrap();
            prepared.armed = false;
            drop(prepared);
            let found = scan(fixture.store.classes.0, &fixture.store.temporary(false))
                .unwrap()
                .unwrap();
            for (path, name) in values.iter().take(removed) {
                let key = write_key(found.keys.get(*path).unwrap()).unwrap();
                let name = wide(name);
                unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) }
                    .ok()
                    .unwrap();
            }
            drop(found);
            fixture.store.recover_slots(None).unwrap();
            assert!(
                child_direct(fixture.store.classes.0, &fixture.store.temporary(false))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn unknown_staging_and_another_owners_cleanup_are_preserved() {
        let mut fixture = Fixture::new();
        let name = fixture.store.temporary(false);
        let key = create_key(fixture.store.classes.0, &name, "Foreign fixture metadata").unwrap();
        drop(key);
        assert!(fixture.store.recover_slots(None).is_err());
        let owner = fixture.owner.clone();
        assert_eq!(register(&mut fixture.store, &owner), Outcome::Unavailable);
        let found = scan(fixture.store.classes.0, &name).unwrap().unwrap();
        let tree = found.tree;
        drop(found.keys);
        erase_exact(fixture.store.classes.0, &name, &tree).unwrap();
        let mut prepared = fixture.store.prepare(&owner.values(), &mut 0).unwrap();
        prepared.armed = false;
        drop(prepared);
        assert!(fixture.store.recover_slots(Some(&"0".repeat(64))).is_err());
        assert_eq!(
            scan(fixture.store.classes.0, &name).unwrap().unwrap().tree,
            owner.values()
        );
        fixture
            .store
            .recover_slots(Some(&owner.path_sha256))
            .unwrap();
        assert!(child_direct(fixture.store.classes.0, &name)
            .unwrap()
            .is_none());
    }
}
