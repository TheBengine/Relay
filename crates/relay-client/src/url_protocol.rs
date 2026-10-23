//! Conservative ownership policy for the per-user invitation URL handler.
//! Ownership is bookkeeping, not publisher authentication or E2EE trust.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[cfg(windows)]
#[path = "url_protocol_windows.rs"]
mod native;
#[cfg(windows)]
pub fn register_current() -> Outcome {
    native::register_current()
}
#[cfg(windows)]
pub fn unregister_current() -> Outcome {
    native::unregister_current()
}

const MARKER: &str = "relay.invitationowner.v1";
const COMMAND: &str = "shell\\open\\command";
const TITLE: &str = "URL:Relay Invitation";
const REG_SZ: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Value {
    kind: u32,
    bytes: Vec<u8>,
}
impl Value {
    fn text(text: &str) -> Self {
        Self {
            kind: REG_SZ,
            bytes: text
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect(),
        }
    }
    fn string(&self) -> Option<String> {
        if self.kind != REG_SZ
            || self.bytes.len() < 2
            || self.bytes.len() > 8192
            || !self.bytes.len().is_multiple_of(2)
        {
            return None;
        }
        let words: Vec<u16> = self
            .bytes
            .chunks_exact(2)
            .map(|n| u16::from_le_bytes([n[0], n[1]]))
            .collect();
        if words.last() != Some(&0) || words[..words.len() - 1].contains(&0) {
            return None;
        }
        String::from_utf16(&words[..words.len() - 1]).ok()
    }
}
type Tree = BTreeMap<String, BTreeMap<String, Value>>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    schema: u8,
    executable: String,
    path_sha256: String,
}

/// Pure spelling validation. Native metadata checks must follow separately.
/// Refuse ambiguous Windows aliases instead of resolving them outside C:/D:.
fn executable_path(text: &str) -> Option<String> {
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let bytes = text.as_bytes();
    if !(8..=1024).contains(&text.len())
        || !matches!(bytes.first(), Some(b'C' | b'c' | b'D' | b'd'))
        || bytes.get(1..3) != Some(b":\\")
        || !text.to_ascii_lowercase().ends_with(".exe")
    {
        return None;
    }
    for part in text[3..].split('\\') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|c| c.is_control() || ":/<>\"|?*".contains(c))
        {
            return None;
        }
        let stem = part.split('.').next()?.to_ascii_lowercase();
        if matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
            || stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9')
        {
            return None;
        }
    }
    // Preserve component spelling: a directory can have case-sensitive names.
    // Aliases which cannot be proved identical are preserved, never reclaimed.
    let mut normalized = text.to_owned();
    normalized.replace_range(..1, &text[..1].to_ascii_uppercase());
    Some(normalized)
}

impl Owner {
    fn new(path: &str) -> Option<Self> {
        let executable = executable_path(path)?;
        Some(Self {
            schema: 1,
            path_sha256: format!("{:x}", Sha256::digest(executable.as_bytes())),
            executable,
        })
    }
    fn from_tree(tree: &Tree) -> Option<Self> {
        let text = tree.get("")?.get(MARKER)?.string()?;
        let owner: Self = serde_json::from_str(&text).ok()?;
        (owner == Self::new(&owner.executable)?).then_some(owner)
    }
    fn values(&self) -> Tree {
        let mut tree = Tree::new();
        for (key, name, text) in [
            ("", "", TITLE.to_owned()),
            ("", "url protocol", String::new()),
            (
                "",
                MARKER,
                serde_json::to_string(self).expect("Simple owner metadata"),
            ),
            ("defaulticon", "", format!("\"{}\",0", self.executable)),
            ("shell", "", "open".to_owned()),
            ("shell\\open", "", String::new()),
            (
                COMMAND,
                "",
                format!("\"{}\" --open-invitation \"%1\"", self.executable),
            ),
        ] {
            tree.entry(key.into())
                .or_default()
                .insert(name.into(), Value::text(&text));
        }
        tree
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    user: Option<Tree>,
    effective: Option<Tree>,
    machine_present: bool,
    user_choice_present: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileState {
    Present,
    Missing,
    Unknown,
}

trait Registry {
    fn snapshot(&mut self) -> Result<Snapshot, ()>;
    fn executable(&self, path: &str) -> FileState;
    /// Conditional publication of a complete handler. Preparation failures must
    /// preserve the previous active handler; never activate a partial tree.
    fn commit(&mut self, before: &Snapshot, next: Option<Tree>) -> Result<(), ()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    Removed,
    Preserved,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Register,
    Unregister,
}
pub fn installation_allowed(
    personal: bool,
    state: &relay_protocol::installation::InstallationState,
) -> bool {
    personal
        && !matches!(state,relay_protocol::installation::InstallationState::Ready{record:Some(record)}
        if record.app_id.eq_ignore_ascii_case("68e09dab-cada-4de3-b9d3-e2a9bdb8036c"))
}
pub fn operation(args: &[String]) -> Result<Option<Operation>, &'static str> {
    const REGISTER: &str = "--register-invitation-protocol";
    const UNREGISTER: &str = "--unregister-invitation-protocol";
    if !args
        .iter()
        .skip(1)
        .any(|arg| arg.starts_with(REGISTER) || arg.starts_with(UNREGISTER))
    {
        return Ok(None);
    }
    let valid = args.len() == 2
        || args.len() == 3 && args[1] == REGISTER && args[2].starts_with("--wait-for-installer=");
    if valid && args[1] == REGISTER {
        Ok(Some(Operation::Register))
    } else if args.len() == 2 && args[1] == UNREGISTER {
        Ok(Some(Operation::Unregister))
    } else {
        Err("Relay could not update invitation links. Open Relay and paste the invitation in Join friends.")
    }
}

fn register(registry: &mut impl Registry, owner: &Owner) -> Outcome {
    if registry.executable(&owner.executable) != FileState::Present {
        return Outcome::Unavailable;
    }
    let Ok(before) = registry.snapshot() else {
        return Outcome::Unavailable;
    };
    let own_values = owner.values();
    if before.user.as_ref() == Some(&own_values) {
        return if before.effective == before.user && !before.user_choice_present {
            Outcome::Ready
        } else {
            Outcome::Preserved
        };
    }
    if before.machine_present || before.user_choice_present {
        return Outcome::Preserved;
    }
    match &before.user {
        None if before.effective.is_none() => {}
        Some(previous) if before.effective == before.user => {
            let Some(old) = Owner::from_tree(previous) else {
                return Outcome::Preserved;
            };
            if *previous != old.values()
                || old == *owner
                || registry.executable(&old.executable) != FileState::Missing
            {
                return Outcome::Preserved;
            }
        }
        _ => return Outcome::Preserved,
    }
    if registry.commit(&before, Some(own_values)).is_ok() {
        Outcome::Ready
    } else {
        Outcome::Unavailable
    }
}

fn unregister(registry: &mut impl Registry, owner: &Owner) -> Outcome {
    let Ok(before) = registry.snapshot() else {
        return Outcome::Unavailable;
    };
    let Some(tree) = &before.user else {
        return Outcome::Removed;
    };
    let values = owner.values();
    if Owner::from_tree(tree).as_ref() != Some(owner)
        || tree.get(COMMAND).and_then(|node| node.get(""))
            != values.get(COMMAND).and_then(|node| node.get(""))
    {
        return Outcome::Preserved;
    }
    // A foreign addition makes ownership ambiguous. Preserve the whole tree;
    // never move or delete someone else's values during uninstall.
    if *tree != values {
        return Outcome::Preserved;
    }
    if registry.commit(&before, None).is_ok() {
        Outcome::Removed
    } else {
        Outcome::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maintenance_never_accepts_profile_or_invitation_arguments() {
        let args = |tail: &[&str]| {
            std::iter::once("relay.exe")
                .chain(tail.iter().copied())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(operation(&args(&[])), Ok(None));
        assert_eq!(
            operation(&args(&["--register-invitation-protocol"])),
            Ok(Some(Operation::Register))
        );
        assert_eq!(
            operation(&args(&[
                "--register-invitation-protocol",
                "--wait-for-installer=1:2"
            ])),
            Ok(Some(Operation::Register))
        );
        assert_eq!(
            operation(&args(&["--unregister-invitation-protocol"])),
            Ok(Some(Operation::Unregister))
        );
        for tail in [
            vec!["--register-invitation-protocol=1"],
            vec![
                "--unregister-invitation-protocol",
                "--wait-for-installer=1:2",
            ],
            vec!["--register-invitation-protocol", "--profile-dir", "C:\\own"],
            vec![
                "--open-invitation",
                "private",
                "--unregister-invitation-protocol",
            ],
            vec![
                "--register-invitation-protocol",
                "--unregister-invitation-protocol",
            ],
        ] {
            assert!(operation(&args(&tail)).is_err());
        }
    }
    #[test]
    fn recovery_installations_and_explicit_profiles_never_publish_personal_links() {
        use relay_protocol::installation::{InstallationRecord, InstallationState};
        let normal = InstallationState::Ready { record: None };
        assert!(installation_allowed(true, &normal));
        assert!(!installation_allowed(false, &normal));
        let fixture = InstallationState::Ready {
            record: Some(InstallationRecord {
                app_id: "68e09dab-cada-4de3-b9d3-e2a9bdb8036c".into(),
                version: "0.10.0".into(),
                sequence: "20261015123456".into(),
                payload_sha256: "a".repeat(64),
                schema: Some(1),
            }),
        };
        assert!(!installation_allowed(true, &fixture));
    }
    #[derive(Clone)]
    struct Fake {
        state: Snapshot,
        files: BTreeMap<String, FileState>,
        unreadable: bool,
        fail_after: Option<usize>,
        race: bool,
        writes: usize,
    }
    impl Fake {
        fn empty(owner: &Owner) -> Self {
            Self {
                state: Snapshot {
                    user: None,
                    effective: None,
                    machine_present: false,
                    user_choice_present: false,
                },
                files: [(owner.executable.clone(), FileState::Present)].into(),
                unreadable: false,
                fail_after: None,
                race: false,
                writes: 0,
            }
        }
        fn installed(owner: &Owner) -> Self {
            let mut fake = Self::empty(owner);
            fake.state.user = Some(owner.values());
            fake.state.effective = fake.state.user.clone();
            fake
        }
    }
    impl Registry for Fake {
        fn snapshot(&mut self) -> Result<Snapshot, ()> {
            if self.unreadable {
                Err(())
            } else {
                Ok(self.state.clone())
            }
        }
        fn executable(&self, path: &str) -> FileState {
            *self.files.get(path).unwrap_or(&FileState::Unknown)
        }
        fn commit(&mut self, before: &Snapshot, next: Option<Tree>) -> Result<(), ()> {
            if self.race {
                self.state.user = Some(Owner::new(r"C:\other\new.exe").unwrap().values());
                self.state.effective = self.state.user.clone();
            }
            if self.state != *before {
                return Err(());
            }
            // Model transaction rollback at each value write, including commit.
            let steps = next.as_ref().map_or(1, |tree| {
                tree.values().map(BTreeMap::len).sum::<usize>() + 1
            });
            for step in 0..steps {
                if self.fail_after == Some(step) {
                    return Err(());
                }
            }
            self.state.user = next;
            self.state.effective = self.state.user.clone();
            self.writes += 1;
            Ok(())
        }
    }
    fn owner() -> Owner {
        Owner::new(r"C:\Relay App\dist\relay.exe").unwrap()
    }
    #[test]
    fn local_spelling_rejects_shares_devices_traversal_and_windows_aliases() {
        for path in [
            r"E:\relay.exe",
            r"\\server\share\relay.exe",
            r"\\?\UNC\server\relay.exe",
            r"C:relay.exe",
            r"C:/Relay/relay.exe",
            r"C:\..\relay.exe",
            r"C:\.\relay.exe",
            r"C:\bad.\relay.exe",
            r"C:\bad \relay.exe",
            r"C:\NUL\relay.exe",
            r"D:\LPT1.exe",
            r"D:\app\relay.exe:stream",
            r"C:\x\relay.exe\",
            "C:\\x\n\\relay.exe",
        ] {
            assert!(Owner::new(path).is_none(), "Accepted ambiguous path");
        }
        assert_eq!(
            Owner::new(r"\\?\c:\Relay App\relay.EXE"),
            Owner::new(r"C:\Relay App\relay.EXE")
        );
    }
    #[test]
    fn strict_marker_and_strings_are_not_publisher_or_execution_authority() {
        let owner = owner();
        let mut tree = owner.values();
        assert_eq!(Owner::from_tree(&tree), Some(owner.clone()));
        for value in [
            Value {
                kind: 2,
                bytes: Value::text("bad").bytes,
            },
            Value {
                kind: 1,
                bytes: vec![0, 0, 1],
            },
            Value::text("one\0two"),
            Value::text(r#"{"schema":1,"executable":"C:\\app\\relay.exe","path_sha256":"wrong"}"#),
        ] {
            tree.get_mut("").unwrap().insert(MARKER.into(), value);
            assert!(Owner::from_tree(&tree).is_none());
        }
    }
    #[test]
    fn absent_handler_registers_once_and_same_path_upgrade_needs_no_rewrite() {
        let owner = owner();
        let mut fake = Fake::empty(&owner);
        assert_eq!(register(&mut fake, &owner), Outcome::Ready);
        assert_eq!(fake.writes, 1);
        assert_eq!(register(&mut fake, &owner), Outcome::Ready);
        assert_eq!(fake.writes, 1);
        assert_eq!(unregister(&mut fake, &owner), Outcome::Removed);
        assert!(fake.state.user.is_none());
    }
    #[test]
    fn live_unknown_malformed_foreign_system_and_user_choice_handlers_are_preserved() {
        let owner = owner();
        let other = Owner::new(r"D:\other\relay.exe").unwrap();
        for state in [FileState::Present, FileState::Unknown] {
            let mut fake = Fake::installed(&other);
            fake.files
                .insert(owner.executable.clone(), FileState::Present);
            fake.files.insert(other.executable.clone(), state);
            let before = fake.state.clone();
            assert_eq!(register(&mut fake, &owner), Outcome::Preserved);
            assert_eq!(fake.state, before);
            assert_eq!(fake.writes, 0);
        }
        for reason in 0..6 {
            let mut fake = Fake::empty(&owner);
            match reason {
                0 => fake.state.machine_present = true,
                1 => fake.state.user_choice_present = true,
                2 => fake.state.effective = Some(other.values()),
                3 => {
                    let mut tree = other.values();
                    tree.get_mut("").unwrap().remove(MARKER);
                    fake.state.user = Some(tree.clone());
                    fake.state.effective = Some(tree);
                }
                4 => {
                    let mut tree = other.values();
                    tree.entry("shell\\open\\ddeexec".into()).or_default();
                    fake.state.user = Some(tree.clone());
                    fake.state.effective = Some(tree);
                    fake.files
                        .insert(other.executable.clone(), FileState::Missing);
                }
                _ => fake.unreadable = true,
            }
            let before = fake.state.clone();
            assert!(matches!(
                register(&mut fake, &owner),
                Outcome::Preserved | Outcome::Unavailable
            ));
            assert_eq!(fake.state, before);
            assert_eq!(fake.writes, 0);
        }
    }
    #[test]
    fn only_a_complete_known_missing_owned_handler_can_transfer() {
        let owner = owner();
        let old = Owner::new(r"D:\old\relay.exe").unwrap();
        let mut fake = Fake::installed(&old);
        fake.files
            .insert(owner.executable.clone(), FileState::Present);
        fake.files
            .insert(old.executable.clone(), FileState::Missing);
        assert_eq!(register(&mut fake, &owner), Outcome::Ready);
        assert_eq!(unregister(&mut fake, &old), Outcome::Preserved);
        assert_eq!(fake.state.user, Some(owner.values()));
    }
    #[test]
    fn cleanup_requires_marker_and_command_and_preserves_every_foreign_addition() {
        let owner = owner();
        for change in [MARKER, "command"] {
            let mut fake = Fake::installed(&owner);
            if change == MARKER {
                fake.state
                    .user
                    .as_mut()
                    .unwrap()
                    .get_mut("")
                    .unwrap()
                    .remove(MARKER);
            } else {
                fake.state
                    .user
                    .as_mut()
                    .unwrap()
                    .get_mut(COMMAND)
                    .unwrap()
                    .insert("".into(), Value::text("other.exe"));
            }
            let before = fake.state.clone();
            assert_eq!(unregister(&mut fake, &owner), Outcome::Preserved);
            assert_eq!(fake.state, before);
        }
        let mut fake = Fake::installed(&owner);
        let tree = fake.state.user.as_mut().unwrap();
        tree.get_mut("").unwrap().insert(
            "foreign".into(),
            Value {
                kind: 3,
                bytes: vec![1, 2, 3],
            },
        );
        tree.entry("extra".into())
            .or_default()
            .insert("data".into(), Value::text("keep"));
        tree.get_mut("defaulticon")
            .unwrap()
            .insert("".into(), Value::text("foreign icon"));
        let before = fake.state.clone();
        assert_eq!(unregister(&mut fake, &owner), Outcome::Preserved);
        assert_eq!(fake.state, before);
        let tree = fake.state.user.unwrap();
        assert_eq!(tree.len(), 6);
        assert!(tree[""].contains_key("foreign"));
        assert_eq!(tree["extra"]["data"], Value::text("keep"));
        assert_eq!(tree["defaulticon"][""], Value::text("foreign icon"));
    }
    #[test]
    fn atomic_failure_or_a_changed_owner_never_publishes_a_partial_handler() {
        let owner = owner();
        for failure in 0..8 {
            let mut fake = Fake::empty(&owner);
            fake.fail_after = Some(failure);
            let before = fake.state.clone();
            assert_eq!(register(&mut fake, &owner), Outcome::Unavailable);
            assert_eq!(fake.state, before);
            assert_eq!(fake.writes, 0);
        }
        let mut fake = Fake::empty(&owner);
        fake.race = true;
        assert_eq!(register(&mut fake, &owner), Outcome::Unavailable);
        assert_eq!(fake.writes, 0);
        let mut fake = Fake::installed(&owner);
        fake.fail_after = Some(0);
        let before = fake.state.clone();
        assert_eq!(unregister(&mut fake, &owner), Outcome::Unavailable);
        assert_eq!(fake.state, before);
    }
}
