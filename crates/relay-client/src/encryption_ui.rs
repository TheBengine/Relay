//! Explicit, device-bound fingerprint approval. Directory entries are never trusted by the UI.
use super::*;
use crate::e2ee::{CryptoPeerStatus, CryptoUiStatus, MessageProtection};
use crate::settings_ui as controls;
#[path = "history_backup_ui.rs"]
mod history_backup;

pub(super) fn notification_body(message: &Message, previews: bool) -> String {
    if !previews {
        "New message".into()
    } else if message.content.is_empty() {
        "Sent an attachment".into()
    } else {
        crate::message_format::plain_preview(&message.content, 240)
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct DeviceIdentity {
    user_id: String,
    device_id: String,
    fingerprint: String,
}

impl From<&CryptoPeerStatus> for DeviceIdentity {
    fn from(peer: &CryptoPeerStatus) -> Self {
        Self {
            user_id: peer.user_id.clone(),
            device_id: peer.device_id.clone(),
            fingerprint: peer.fingerprint.clone(),
        }
    }
}

#[derive(Default)]
struct Approval {
    entered: String,
    pending: Option<Instant>,
}

/// The open "Verify <name>" safety-number dialog for one device.
struct SafetyCheck {
    url: String,
    user_id: String,
    own_fingerprint: String,
    peer: DeviceIdentity,
    name: String,
    rows: [String; 3],
    mismatch: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SafetyAction {
    None,
    Match,
    Mismatch,
    Close,
}

/// What one frame of the dialog did, plus the button rectangles for layout checks.
struct SafetyOutcome {
    action: SafetyAction,
    matches: Rect,
    mismatch: Rect,
}

/// Draws the safety-number dialog. Close or Escape only closes it; nothing is
/// approved unless the person chooses "They match".
fn safety_dialog_ui(
    ctx: &egui::Context,
    name: &str,
    rows: &[String; 3],
    mismatch: bool,
) -> SafetyOutcome {
    let title = format!("Verify {name}");
    let mut outcome = SafetyOutcome {
        action: SafetyAction::None,
        matches: Rect::NOTHING,
        mismatch: Rect::NOTHING,
    };
    let window = desktop_ui::dialog(ctx, title.as_str())
        .id(egui::Id::new("safety-number"))
        .collapsible(false)
        .resizable(false)
        .default_width(420.0);
    if host_setup_ui::show_with_header(ctx, window, &title, |ui| {
        ui.add(
            egui::Label::new(format!(
                "Read these numbers to {name} on a call or in person. If they match, choose They match."
            ))
            .wrap(true),
        );
        ui.add_space(10.0);
        for row in rows {
            ui.add(
                egui::Label::new(
                    RichText::new(row)
                        .monospace()
                        .size(font_size(crate::appearance::TEXT_HEADING)),
                )
                .wrap(true),
            );
        }
        ui.add_space(10.0);
        controls::description(
            ui,
            "You both see the same numbers. Compare them on a call or in person, not in a message sent through this app or the server.",
        );
        ui.add_space(8.0);
        if mismatch {
            ui.add(
                egui::Label::new(
                    RichText::new("Do not approve this device. If it is already approved, close this dialog and remove its verification. The numbers should be identical. Ask the person to open Encryption on their PC and check that it is the right device, then compare again. If they still differ, someone may be impersonating one of you.")
                        .color(c_bad()),
                )
                .wrap(true),
            );
            if ui.add(secondary("Close")).clicked() {
                outcome.action = SafetyAction::Close;
            }
        } else {
            ui.horizontal_wrapped(|ui| {
                let matches = ui.add(primary("They match"));
                outcome.matches = matches.rect;
                if matches.clicked() {
                    outcome.action = SafetyAction::Match;
                }
                let differs = ui.add(secondary("They don't match"));
                outcome.mismatch = differs.rect;
                if differs.clicked() {
                    outcome.action = SafetyAction::Mismatch;
                }
            });
        }
    }) {
        outcome.action = SafetyAction::Close;
    }
    outcome
}

#[derive(Clone)]
enum RecoveryAction {
    Reset {
        device_id: String,
        fingerprint: String,
    },
    Revoke(DeviceIdentity),
}

struct RecoveryConfirmation {
    url: String,
    user_id: String,
    action: RecoveryAction,
    entered: String,
    pending: Option<Instant>,
}

impl RecoveryConfirmation {
    fn current(&self, url: &str, user_id: &str, status: &CryptoUiStatus) -> bool {
        if self.url != url || self.user_id != user_id {
            return false;
        }
        match &self.action {
            RecoveryAction::Reset {
                device_id,
                fingerprint,
            } => device_id == &status.own_device_id && fingerprint == &status.own_fingerprint,
            RecoveryAction::Revoke(identity) => {
                identity.user_id == self.user_id
                    && identity.device_id != status.own_device_id
                    && status
                        .own_devices
                        .iter()
                        .any(|device| DeviceIdentity::from(device) == *identity)
            }
        }
    }
    fn confirmed(&self) -> bool {
        match &self.action {
            RecoveryAction::Reset { .. } => self.entered == "RESET DEVICE",
            RecoveryAction::Revoke(identity) => self.entered == identity.device_id,
        }
    }
}

#[derive(Default)]
pub(super) struct EncryptionUi {
    pub(super) status: CryptoUiStatus,
    approvals: HashMap<DeviceIdentity, Approval>,
    expanded_people: HashSet<String>,
    capture_fixture: bool,
    recovery: Option<RecoveryConfirmation>,
    safety: Option<SafetyCheck>,
    pub(super) history: history_backup::HistoryUi,
}

#[derive(Default)]
pub(super) struct ProtectionCache {
    values: HashMap<String, (MessageProtection, u64)>,
    sequence: u64,
}

impl ProtectionCache {
    pub(super) fn insert(&mut self, message_id: String, protection: MessageProtection) {
        const MAX_ENTRIES: usize = 2048;
        self.sequence = self.sequence.wrapping_add(1);
        if self.values.len() >= MAX_ENTRIES && !self.values.contains_key(&message_id) {
            if let Some(oldest) = self
                .values
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(id, _)| id.clone())
            {
                self.values.remove(&oldest);
            }
        }
        self.values.insert(message_id, (protection, self.sequence));
    }
    pub(super) fn get(&self, message_id: &str) -> Option<&MessageProtection> {
        self.values
            .get(message_id)
            .map(|(protection, _)| protection)
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

impl EncryptionUi {
    pub(super) fn clear_session(&mut self) {
        *self = Self::default();
    }

    pub(super) fn apply_status(&mut self, status: CryptoUiStatus) {
        self.expanded_people
            .retain(|user_id| status.peers.iter().any(|peer| &peer.user_id == user_id));
        self.approvals.retain(|identity, _| {
            status
                .peers
                .iter()
                .any(|peer| DeviceIdentity::from(peer) == *identity)
        });
        for approval in self.approvals.values_mut() {
            approval.pending = None;
        }
        if self.recovery.as_ref().is_some_and(|confirmation| {
            !confirmation.current(&confirmation.url, &confirmation.user_id, &status)
        }) {
            self.recovery = None;
        }
        self.status = status;
    }
}

struct PersonDevices<'a> {
    user_id: &'a str,
    name: &'a str,
    devices: Vec<&'a CryptoPeerStatus>,
}

impl PersonDevices<'_> {
    fn needs_confirmation(&self) -> bool {
        self.devices
            .iter()
            .any(|device| !device.verified || device.changed)
    }

    fn changed(&self) -> bool {
        self.devices.iter().any(|device| device.changed)
    }
}

fn people_and_devices(peers: &[CryptoPeerStatus]) -> Vec<PersonDevices<'_>> {
    let mut people: Vec<PersonDevices<'_>> = Vec::new();
    let mut positions: HashMap<&str, usize> = HashMap::new();
    for peer in peers {
        if let Some(&index) = positions.get(peer.user_id.as_str()) {
            people[index].devices.push(peer);
        } else {
            positions.insert(peer.user_id.as_str(), people.len());
            people.push(PersonDevices {
                user_id: &peer.user_id,
                name: &peer.name,
                devices: vec![peer],
            });
        }
    }
    for person in &mut people {
        person.devices.sort_by(|a, b| {
            a.device_id
                .cmp(&b.device_id)
                .then_with(|| a.fingerprint.cmp(&b.fingerprint))
        });
    }
    people.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.user_id.cmp(b.user_id))
    });
    people
}

#[derive(Debug, PartialEq, Eq)]
enum EncryptionSummary {
    NeedsAttention,
    Unsupported,
    NotReady,
    NoOtherDevices,
    Changed { people: usize },
    NeedsConfirmation { people: usize, devices: usize },
    LocallyConfirmed { people: usize, devices: usize },
}

fn encryption_summary(status: &CryptoUiStatus, compatible_server: bool) -> EncryptionSummary {
    // Cached directory approvals cannot turn a failed refresh or key-store load green.
    if status.problem.is_some() {
        return EncryptionSummary::NeedsAttention;
    }
    if !status.supported && !compatible_server {
        return EncryptionSummary::Unsupported;
    }
    if status.own_device_id.is_empty()
        || !fingerprint_matches(&status.own_fingerprint, &status.own_fingerprint)
    {
        return EncryptionSummary::NotReady;
    }
    let people = people_and_devices(&status.peers);
    let changed = people.iter().filter(|person| person.changed()).count();
    if changed > 0 {
        return EncryptionSummary::Changed { people: changed };
    }
    let unconfirmed_people = people
        .iter()
        .filter(|person| person.needs_confirmation())
        .count();
    if unconfirmed_people > 0 {
        return EncryptionSummary::NeedsConfirmation {
            people: unconfirmed_people,
            devices: status
                .peers
                .iter()
                .filter(|device| !device.verified || device.changed)
                .count(),
        };
    }
    if people.is_empty() {
        EncryptionSummary::NoOtherDevices
    } else {
        EncryptionSummary::LocallyConfirmed {
            people: people.len(),
            devices: status.peers.len(),
        }
    }
}

// Fixed public display data for the existing input-disabled native capture mode.
// It is never installed in a crypto actor or used as proof of device approval.
fn encryption_capture_status(panel: &str) -> Option<CryptoUiStatus> {
    if !matches!(
        panel,
        "encryption-pending"
            | "encryption-pending-expanded"
            | "encryption-changed"
            | "encryption-locally-confirmed"
            | "encryption-empty"
            | "encryption-problem"
    ) {
        return None;
    }
    let confirmed = matches!(
        panel,
        "encryption-locally-confirmed" | "encryption-problem" | "encryption-changed"
    );
    let mut peers = vec![
        CryptoPeerStatus {
            user_id: "synthetic-anna".into(),
            name: "Anna (synthetic)".into(),
            device_id: "synthetic-anna-desktop".into(),
            fingerprint: "1abc".repeat(16),
            verified: true,
            changed: false,
        },
        CryptoPeerStatus {
            user_id: "synthetic-anna".into(),
            name: "Anna (synthetic)".into(),
            device_id: "synthetic-anna-laptop".into(),
            fingerprint: "2def".repeat(16),
            verified: confirmed,
            changed: panel == "encryption-changed",
        },
        CryptoPeerStatus {
            user_id: "synthetic-bo".into(),
            name: "Bo (synthetic)".into(),
            device_id: "synthetic-bo-desktop".into(),
            fingerprint: "3abc".repeat(16),
            verified: confirmed,
            changed: false,
        },
    ];
    if panel == "encryption-empty" {
        peers.clear();
    }
    Some(CryptoUiStatus {
        own_device_id: "synthetic-own-device".into(),
        own_fingerprint: "0abc".repeat(16),
        peers,
        supported: true,
        status: "Synthetic layout fixture only. No real device was verified.".into(),
        problem: (panel == "encryption-problem")
            .then(|| "Synthetic failure: a protected call could not prepare its keys.".into()),
        ..Default::default()
    })
}

fn fingerprint_matches(expected: &str, entered: &str) -> bool {
    if entered.len() > 4096 {
        return false;
    }
    let entered: String = entered
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    expected.len() == 64
        && entered.len() == 64
        && expected.bytes().all(|byte| byte.is_ascii_hexdigit())
        && entered.bytes().all(|byte| byte.is_ascii_hexdigit())
        && expected.eq_ignore_ascii_case(&entered)
}

/// A verification card contains public identity only. It must still come from
/// a trusted source; a card supplied by the server is not proof of identity.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationCard {
    relay_device_check: u8,
    community: String,
    account: String,
    device: String,
    security_code: String,
}

fn approval_matches(peer: &CryptoPeerStatus, community: &str, entered: &str) -> bool {
    if entered.len() > 4096 {
        return false;
    }
    if entered.trim_start().starts_with('{') {
        serde_json::from_str::<VerificationCard>(entered).is_ok_and(|card| {
            card.relay_device_check == 1
                && !community.is_empty()
                && card.community == community
                && card.account == peer.user_id
                && card.device == peer.device_id
                && fingerprint_matches(&peer.fingerprint, &card.security_code)
        })
    } else {
        fingerprint_matches(&peer.fingerprint, entered)
    }
}

// This field contains only the public code/card supplied by the user. Keep this
// accessibility handling scoped here; passwords and private-key fields must not
// expose their contents. egui 0.27 does not handle AccessKit SetValue itself.
fn approval_code_input(
    ui: &mut egui::Ui,
    entered: &mut String,
    scope: (&str, &str, &str),
    identity: &DeviceIdentity,
) -> egui::Response {
    let id = ui.make_persistent_id(("device-approval-code", scope, identity));
    let replacement = ui
        .is_enabled()
        .then(|| {
            ui.input(|input| {
                input
                    .accesskit_action_requests(id, egui::accesskit::Action::SetValue)
                    .last()
                    .and_then(|request| match &request.data {
                        Some(egui::accesskit::ActionData::Value(value)) if value.len() <= 4096 => {
                            Some(value.to_string())
                        }
                        _ => None,
                    })
            })
        })
        .flatten();
    let changed = replacement.is_some_and(|value| {
        if *entered == value {
            return false;
        }
        *entered = value;
        // Discard an old selection/IME state without requesting keyboard focus.
        let mut state = egui::text_edit::TextEditState::default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(entered.chars().count()),
            )));
        state.store(ui.ctx(), id);
        true
    });
    let mut response = ui.add_sized(
        [ui.available_width(), 64.0],
        egui::TextEdit::multiline(entered)
            .id(id)
            .hint_text("Paste their security code or verification card here")
            .char_limit(4096)
            .font(egui::TextStyle::Monospace),
    );
    if changed {
        response.mark_changed();
    }
    ui.ctx().accesskit_node_builder(id, |builder| {
        builder.set_name("Security code or verification card");
        builder.set_value(entered.as_str());
        if response.enabled() {
            builder.clear_disabled();
            builder.add_action(egui::accesskit::Action::SetValue);
        } else {
            builder.set_disabled();
            builder.remove_action(egui::accesskit::Action::SetValue);
        }
    });
    response
}

fn approve_device_button(ui: &mut egui::Ui, enabled: bool, field: egui::Id) -> egui::Response {
    let response = ui
        .push_id(field.with("approve-device"), |ui| {
            ui.add_enabled(enabled, primary("Approve Device"))
        })
        .inner;
    // egui 0.27 omits this disabled state from the accessibility node.
    ui.ctx().accesskit_node_builder(response.id, |builder| {
        if response.enabled() {
            builder.clear_disabled();
        } else {
            builder.set_disabled();
        }
    });
    response
}

fn fingerprint_label(ui: &mut egui::Ui, fingerprint: &str) {
    let grouped = if fingerprint.len() == 64 && fingerprint.is_ascii() {
        fingerprint
            .as_bytes()
            .chunks(8)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        "Security code unavailable".into()
    };
    ui.add(egui::Label::new(RichText::new(grouped).monospace()).wrap(true));
}

impl RelayApp {
    pub(super) fn message_protection_label(&self, ui: &mut egui::Ui, message_id: &str) {
        match self.message_protection.get(message_id) {
            Some(MessageProtection::Legacy) => {
                let open = self
                    .bootstrap
                    .as_ref()
                    .is_some_and(|b| b.protection.level == relay_protocol::ProtectionLevel::Open);
                if open {
                    ui.label(RichText::new("Not end-to-end encrypted").size(font_size(TEXT_CAPTION)).color(c_muted()))
                        .on_hover_text("This community runs without end-to-end encryption. The server host can read this message.");
                } else {
                    ui.label(RichText::new("Legacy message · Not end-to-end encrypted").size(font_size(TEXT_CAPTION)).color(c_muted()))
                        .on_hover_text("This older message was stored without end-to-end encryption. The server could read its content.");
                }
            }
            Some(MessageProtection::Unavailable(reason)) => {
                ui.add(
                    egui::Label::new(
                        RichText::new("Cannot decrypt this message")
                            .size(font_size(TEXT_CAPTION))
                            .color(c_bad()),
                    )
                    .wrap(true),
                )
                .on_hover_text(reason);
            }
            Some(MessageProtection::Protected) | None => {}
        }
    }
    pub(super) fn encryption_capture_panel(&mut self, panel: &str) -> bool {
        if self.capture_path.is_none() || self.session.is_none() || self.bootstrap.is_none() {
            return false;
        }
        let Some(status) = encryption_capture_status(panel) else {
            return false;
        };
        // This replaces display state only. Capture mode filters all UI input;
        // no command, key-store mutation or approval is performed here.
        self.encryption_ui = EncryptionUi::default();
        self.encryption_ui.status = status;
        self.encryption_ui.capture_fixture = true;
        if panel == "encryption-pending-expanded" {
            self.encryption_ui
                .expanded_people
                .insert("synthetic-anna".into());
        }
        self.settings_open = true;
        self.settings_tab = SettingsTab::Encryption;
        self.error = None;
        self.notice = None;
        true
    }

    pub(super) fn encryption_page(&mut self, ui: &mut egui::Ui) {
        let open = self
            .bootstrap
            .as_ref()
            .is_some_and(|b| b.protection.level == relay_protocol::ProtectionLevel::Open);
        controls::page_heading(
            ui,
            "Encryption",
            if open {
                "Review this server's choice and your encrypted history."
            } else {
                "Check the devices you trust with private messages and calls."
            },
        );
        if self.capture_path.is_some() && self.encryption_ui.capture_fixture {
            ui.label(RichText::new("Synthetic layout fixture · no real device approvals").strong());
            ui.add_space(10.0);
        }
        if self.session.is_none() {
            egui::Frame::none()
                .fill(c_card())
                .rounding(8.0)
                .inner_margin(Margin::same(16.0))
                .show(ui, |ui| {
                    ui.label(RichText::new("Connect to check your devices").strong());
                    controls::description(
                        ui,
                        "Join a community to confirm people and devices. You can still open a saved history backup below.",
                    );
                });
            controls::divider(ui);
            egui::CollapsingHeader::new("Backup and recovery")
                .id_source("encryption-backup-and-recovery")
                .default_open(false)
                .show(ui, |ui| self.history_backup_page(ui));
            return;
        }

        if open {
            self.protection_section(ui);
            controls::divider(ui);
            controls::section_heading(ui, "Protected conversations and history",
                "Device checks below apply to end-to-end encrypted messages and history. Open-mode calls do not need device approval.");
        }
        let status = self.encryption_ui.status.clone();
        let community = self
            .bootstrap
            .as_ref()
            .map(|b| b.server_id.clone())
            .unwrap_or_default();
        let approval_account = self.session.as_ref().unwrap().user.id.clone();
        let people = people_and_devices(&status.peers);
        let summary = encryption_summary(&status, self.has_chat_capability("e2ee_mls_v1"));
        let mut review_people = false;
        let mut show_own_code = false;
        egui::Frame::none()
            .fill(c_card())
            .rounding(8.0)
            .inner_margin(Margin::same(16.0))
            .show(ui, |ui| {
                let (title, description, color) = match &summary {
                    EncryptionSummary::NeedsAttention => (
                        "Encryption needs attention".to_owned(),
                        if open { "For a protected conversation, open Advanced for details. For an open-mode call, follow the message in the call and use Try again; device approval is not required." }
                        else { "Retry the affected conversation or call. If it still cannot connect, open Advanced for details and the next step." }.to_owned(),
                        c_bad(),
                    ),
                    EncryptionSummary::Unsupported => (
                        if open { "Protected conversations are unavailable" } else { "Encryption is unavailable" }.to_owned(),
                        if open { "Protected messages need a compatible Relay version. Open-mode calls use the server's choice above and do not need device approval." }
                        else { "This server needs a compatible Relay version. Sending messages and starting calls stay unavailable until encryption is ready." }.to_owned(),
                        c_bad(),
                    ),
                    EncryptionSummary::NotReady => (
                        if open { "No protected device loaded" } else { "Encryption is not ready" }.to_owned(),
                        if open { "You can accept this server's open mode and use its calls without a protected device. Earlier encrypted messages still need their original keys." }
                        else { "Your device is not ready for protected conversations yet. Refresh to check again; details are available under Advanced." }.to_owned(),
                        c_muted(),
                    ),
                    EncryptionSummary::NoOtherDevices => (
                        "No other devices to check yet".to_owned(),
                        "When another person joins, their devices will appear here. There is nothing to approve yet.".to_owned(),
                        c_muted(),
                    ),
                    EncryptionSummary::Changed { people } => (
                        format!("Changed devices to review · {people} people"),
                        "Compare the new safety number with each person on a call or in person. A previous approval does not cover a replacement device.".to_owned(),
                        c_bad(),
                    ),
                    EncryptionSummary::NeedsConfirmation { people, devices } => (
                        format!("Devices to confirm · {people} people, {devices} devices"),
                        "Open Verify below and compare the safety number on a call or in person. Choose They match only after checking all three rows.".to_owned(),
                        c_muted(),
                    ),
                    EncryptionSummary::LocallyConfirmed { people, devices } => (
                        "Listed devices confirmed on this PC".to_owned(),
                        format!("You have confirmed {devices} listed devices belonging to {people} people. This shows whom this PC trusts; it does not show whether they have confirmed you or whether each conversation is ready."),
                        c_good(),
                    ),
                };
                ui.add(egui::Label::new(RichText::new(title).strong().color(color)).wrap(true));
                controls::description(ui, &description);
                if let Some(problem) = &status.problem {
                    ui.add(egui::Label::new(RichText::new(problem).color(c_bad())).wrap(true));
                }
                ui.horizontal_wrapped(|ui| {
                    if matches!(summary, EncryptionSummary::Changed { .. } | EncryptionSummary::NeedsConfirmation { .. }) {
                        review_people = ui.add(primary("Review devices")).clicked();
                    }
                    if ui.add(secondary("Refresh Devices")).clicked() {
                        self.send(Command::CryptoRefresh);
                    }
                    if matches!(summary, EncryptionSummary::Changed { .. } | EncryptionSummary::NeedsConfirmation { .. } | EncryptionSummary::LocallyConfirmed { .. }) {
                        show_own_code = ui.add(secondary("Show how to confirm me")).clicked();
                    }
                });
            });
        ui.add_space(10.0);
        controls::description(
            ui,
            "Each device needs its own check. The other person must also confirm your device before you can share protected conversations.",
        );

        if !open {
            self.protection_section(ui);
        }
        controls::divider(ui);
        controls::section_heading(
            ui,
            "People and devices",
            "Verify opens the devices for one person. It never approves all of their devices at once.",
        );
        if people.is_empty() {
            controls::description(ui, "No other devices are registered in this community yet.");
        }
        let mut verify = None;
        let mut unverify = None;
        let mut safety_open = None;
        for person in people {
            if review_people && person.needs_confirmation() {
                self.encryption_ui
                    .expanded_people
                    .insert(person.user_id.to_owned());
            }
            egui::Frame::none()
                .fill(c_card())
                .rounding(8.0)
                .inner_margin(Margin::same(16.0))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        avatar(ui, person.name, 36.0, false);
                        ui.vertical(|ui| {
                            ui.add(egui::Label::new(RichText::new(person.name).strong()).wrap(true));
                            let (label, color) = if person.changed() {
                                ("Changed device · check again", c_bad())
                            } else if person.needs_confirmation() {
                                ("Devices need confirmation", c_muted())
                            } else {
                                (if person.devices.iter().any(|peer| status.via_invitation.contains(&peer.device_id)) {
                                    "Verified on this PC · invitation used"
                                } else {
                                    "Verified on this PC"
                                }, c_good())
                            };
                            ui.label(RichText::new(label).color(color));
                            controls::description(ui, &match person.devices.len() {
                                1 => "1 registered device".to_owned(),
                                n => format!("{n} registered devices"),
                            });
                        });
                        let label = if person.needs_confirmation() { "Verify" } else { "Review" };
                        if ui.push_id(("review-person-devices", &self.login.url, &approval_account, &community, person.user_id), |ui| ui.add(secondary(label))).inner.clicked()
                            && !self.encryption_ui.expanded_people.remove(person.user_id)
                        {
                            self.encryption_ui.expanded_people.insert(person.user_id.to_owned());
                        }
                    });
                    if !self.encryption_ui.expanded_people.contains(person.user_id) {
                        return;
                    }
                    for (index, peer) in person.devices.iter().enumerate() {
                        ui.add_space(14.0);
                        ui.label(RichText::new(format!("Device {}", index + 1)).strong());
                        let identity = DeviceIdentity::from(*peer);
                        let approval = self.encryption_ui.approvals.entry(identity.clone()).or_default();
                        if approval.pending.is_some_and(|at| at.elapsed() >= Duration::from_secs(10)) {
                            approval.pending = None;
                        }
                        let via_invitation = status.via_invitation.contains(&peer.device_id);
                        ui.label(RichText::new(if peer.changed {
                            "Security code changed · check with this person again"
                        } else if peer.verified && via_invitation {
                            "Verified via invitation · approved on this PC"
                        } else if peer.verified {
                            "Device approved on this PC"
                        } else {
                            "Needs your approval"
                        }).color(if peer.changed { c_bad() } else if peer.verified { c_good() } else { c_muted() }));
                        if peer.verified && !peer.changed && via_invitation {
                            controls::description(ui, "Approved through the one-time invitation you shared or received. Compare the safety number any time to confirm it yourself.");
                        }
                        if !peer.verified && status.invitation_unconfirmed.contains(&peer.device_id) {
                            ui.add(egui::Label::new(RichText::new("The invitation proof for this device could not be confirmed. Verify it manually.").color(c_bad())).wrap(true));
                        }
                        fingerprint_label(ui, &peer.fingerprint);
                        if ui.add_enabled(approval.pending.is_none(), secondary("Compare safety number")).clicked() {
                            safety_open = Some((identity.clone(), person.name.to_owned()));
                        }
                        if peer.verified && !peer.changed {
                            if ui.add_enabled(approval.pending.is_none(), secondary("Remove Verification")).clicked() {
                                approval.pending = Some(Instant::now());
                                unverify = Some(identity.clone());
                            }
                        } else {
                            if peer.changed {
                                controls::description(ui, "This may be a new PC or a reinstalled app. Check the new code with the person before sharing protected content with this device.");
                                if ui.add_enabled(approval.pending.is_none(), secondary("Remove Previous Verification")).clicked() {
                                    approval.pending = Some(Instant::now());
                                    unverify = Some(identity.clone());
                                }
                            }
                            ui.push_id(("legacy-approval", &identity), |ui| ui.collapsing("Advanced · paste a code or card", |ui| {
                            controls::description(ui, "Or paste the full code or card they sent you. Do not copy the code from this device list.");
                            let approval_input = approval_code_input(
                                ui,
                                &mut approval.entered,
                                (&self.login.url, &approval_account, &community),
                                &identity,
                            );
                            let matches = approval_matches(peer, &community, &approval.entered);
                            if !approval.entered.trim().is_empty() {
                                ui.add(egui::Label::new(RichText::new(if matches {
                                    "Matches this device. Approve only if it came from the person you trust."
                                } else {
                                    "No match yet. Ask them to copy their full code or card and check that this is the right device."
                                }).color(if matches { c_good() } else { c_bad() })).wrap(true));
                            }
                            if approve_device_button(ui, matches && approval.pending.is_none(), approval_input.id).clicked() {
                                approval.pending = Some(Instant::now());
                                verify = Some(identity.clone());
                            }
                            }));
                        }
                        if let Some(at) = approval.pending {
                            ui.horizontal(|ui| { ui.spinner(); ui.label("Updating verification…"); });
                            ui.ctx().request_repaint_after(Duration::from_secs(10).saturating_sub(at.elapsed()));
                        }
                    }
                });
            ui.add_space(12.0);
        }

        controls::divider(ui);
        egui::CollapsingHeader::new("Your security code")
            .id_source(("your-security-code", &self.login.url, &approval_account, &community))
            .default_open(false)
            .open(show_own_code.then_some(true))
            .show(ui, |ui| {
                controls::description(ui, "Send your code or verification card through a contact method you already trust. Ask the other person to open your device in Encryption, paste it and choose Approve Device. These contain public information only.");
                fingerprint_label(ui, &status.own_fingerprint);
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(
                        fingerprint_matches(&status.own_fingerprint, &status.own_fingerprint),
                        secondary("Copy My Security Code"),
                    ).clicked() {
                        ui.output_mut(|output| output.copied_text = status.own_fingerprint.clone());
                    }
                    if ui.add_enabled(
                        !community.is_empty() && fingerprint_matches(&status.own_fingerprint, &status.own_fingerprint),
                        secondary("Copy Verification Card"),
                    ).clicked() {
                        let card = VerificationCard {
                            relay_device_check: 1,
                            community: community.clone(),
                            account: approval_account.clone(),
                            device: status.own_device_id.clone(),
                            security_code: status.own_fingerprint.clone(),
                        };
                        if let Ok(text) = serde_json::to_string_pretty(&card) {
                            ui.output_mut(|output| output.copied_text = text);
                        }
                    }
                });
                controls::description(ui, "An existing group may also need the device that started its protected conversation to be online before a new device can join.");
            });

        ui.add_space(12.0);
        egui::CollapsingHeader::new("Backup and recovery")
            .id_source("encryption-backup-and-recovery")
            .default_open(false)
            .show(ui, |ui| self.history_backup_page(ui));

        ui.add_space(12.0);
        egui::CollapsingHeader::new("Advanced")
            .id_source(("encryption-advanced", &self.login.url, &approval_account, &community))
            .default_open(false)
            .show(ui, |ui| {
                if let Some(problem) = &status.problem {
                    ui.add(egui::Label::new(RichText::new(problem).color(c_bad())).wrap(true));
                }
                if !status.status.is_empty() {
                    ui.add(egui::Label::new(&status.status).wrap(true));
                }
                controls::description(ui, &format!("Current device ID: {}", status.own_device_id));
                self.history_backup_advanced(ui);
                controls::section_heading(ui, "Your registered devices", "Revoke a device you no longer use. This does not approve other people's devices.");
                for device in &status.own_devices {
                    if device.user_id != approval_account {
                        continue;
                    }
                    let current = device.device_id == status.own_device_id;
                    ui.label(RichText::new(if current { "This Device" } else { "Another Device on Your Account" }).strong());
                    controls::description(ui, &format!("Device ID: {}", device.device_id));
                    fingerprint_label(ui, &device.fingerprint);
                    if !current && ui.add(secondary("Revoke This Device")).clicked() {
                        self.open_encryption_recovery(RecoveryAction::Revoke(DeviceIdentity::from(device)));
                    }
                    ui.add_space(10.0);
                }
                egui::CollapsingHeader::new("People's device identities")
                    .default_open(false)
                    .show(ui, |ui| {
                        for person in people_and_devices(&status.peers) {
                            for (index, peer) in person.devices.iter().enumerate() {
                                controls::description(ui, &format!(
                                    "{} · Device {} · Account ID: {} · Device ID: {}",
                                    person.name, index + 1, peer.user_id, peer.device_id,
                                ));
                                fingerprint_label(ui, &peer.fingerprint);
                            }
                        }
                    });
                controls::divider(ui);
                controls::description(ui, "Resetting creates a new device identity. It does not recover old encrypted messages; keep your existing device keys.");
                if ui.add(secondary("Reset This Encryption Device")).clicked() {
                    self.open_encryption_recovery(RecoveryAction::Reset {
                        device_id: status.own_device_id.clone(),
                        fingerprint: status.own_fingerprint.clone(),
                    });
                }
                self.encryption_recovery_confirmation(ui, &status);
                controls::description(ui, "New or changed devices must be approved before group keys are shared with them. There is no server-side recovery for encryption keys. The server can still see account, channel, membership, timing and traffic metadata.");
            });

        if let Some((identity, name)) = safety_open {
            self.open_safety_check(identity, name, &status);
        }
        if let Some(identity) = verify {
            let Some(origin) = self.command_origin() else {
                self.error = Some("Connect to a community before verifying its devices.".into());
                return;
            };
            self.send(Command::CryptoVerify {
                origin,
                user_id: identity.user_id,
                device_id: identity.device_id,
                fingerprint: identity.fingerprint,
            });
        }
        if let Some(identity) = unverify {
            let Some(origin) = self.command_origin() else {
                self.error =
                    Some("Connect to a community before changing device verification.".into());
                return;
            };
            let had_voice = self.voice_channel.is_some() || self.joining_voice.is_some();
            self.stop_media();
            self.video.clear();
            if had_voice {
                self.send(Command::LeaveVoice);
            }
            self.send(Command::CryptoUnverify {
                origin,
                user_id: identity.user_id,
                device_id: identity.device_id,
            });
        }
    }

    fn open_safety_check(&mut self, peer: DeviceIdentity, name: String, status: &CryptoUiStatus) {
        let Some(user_id) = self.session.as_ref().map(|session| session.user.id.clone()) else {
            return;
        };
        let Some(number) =
            crate::safety_number::safety_number(&status.own_fingerprint, &peer.fingerprint)
        else {
            self.error = Some(
                "The safety number is unavailable for this device. Use its security code instead."
                    .into(),
            );
            return;
        };
        self.encryption_ui.safety = Some(SafetyCheck {
            url: self.login.url.clone(),
            user_id,
            own_fingerprint: status.own_fingerprint.clone(),
            peer,
            name,
            rows: crate::safety_number::rows(&number),
            mismatch: false,
        });
    }

    /// True while `safety_number_dialog` is drawn (its header then owns Escape).
    pub(super) fn safety_dialog_shown(&self) -> bool {
        self.encryption_ui.safety.is_some()
    }

    /// The safety-number dialog. It closes by itself when the account,
    /// community, own device or the other device no longer matches what was
    /// compared. Already-approved devices may be compared again.
    pub(super) fn safety_number_dialog(&mut self, ctx: &egui::Context) {
        let Some(check) = self.encryption_ui.safety.as_ref() else {
            return;
        };
        let status = &self.encryption_ui.status;
        let current = self.login.url == check.url
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.user.id == check.user_id)
            && status.own_fingerprint == check.own_fingerprint
            && status
                .peers
                .iter()
                .any(|peer| DeviceIdentity::from(peer) == check.peer);
        if !current {
            self.encryption_ui.safety = None;
            return;
        }
        let (name, rows, mismatch, peer) = (
            check.name.clone(),
            check.rows.clone(),
            check.mismatch,
            check.peer.clone(),
        );
        match safety_dialog_ui(ctx, &name, &rows, mismatch).action {
            SafetyAction::None => {}
            SafetyAction::Close => self.encryption_ui.safety = None,
            SafetyAction::Mismatch => {
                if let Some(check) = self.encryption_ui.safety.as_mut() {
                    check.mismatch = true;
                }
            }
            SafetyAction::Match => {
                self.encryption_ui.safety = None;
                let Some(origin) = self.command_origin() else {
                    self.error =
                        Some("Connect to a community before verifying its devices.".into());
                    return;
                };
                self.encryption_ui
                    .approvals
                    .entry(peer.clone())
                    .or_default()
                    .pending = Some(Instant::now());
                // The same approval as pasting the code: this exact device and
                // fingerprint, checked again by the encryption worker.
                self.send(Command::CryptoVerify {
                    origin,
                    user_id: peer.user_id,
                    device_id: peer.device_id,
                    fingerprint: peer.fingerprint,
                });
            }
        }
    }

    fn open_encryption_recovery(&mut self, action: RecoveryAction) {
        let Some(session) = &self.session else { return };
        self.encryption_ui.recovery = Some(RecoveryConfirmation {
            url: self.login.url.clone(),
            user_id: session.user.id.clone(),
            action,
            entered: String::new(),
            pending: None,
        });
    }

    fn encryption_recovery_confirmation(&mut self, ui: &mut egui::Ui, status: &CryptoUiStatus) {
        let Some(user_id) = self.session.as_ref().map(|session| session.user.id.clone()) else {
            return;
        };
        if self
            .encryption_ui
            .recovery
            .as_ref()
            .is_some_and(|confirmation| !confirmation.current(&self.login.url, &user_id, status))
        {
            self.encryption_ui.recovery = None;
        }
        let Some(confirmation) = self.encryption_ui.recovery.as_mut() else {
            return;
        };
        if confirmation
            .pending
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(10))
        {
            confirmation.pending = None;
        }
        let mut submit = false;
        let mut cancel = false;
        ui.add_space(10.0);
        egui::Frame::none().fill(c_panel()).stroke(Stroke::new(1.0_f32,c_bad())).rounding(8.0).inner_margin(Margin::same(16.0)).show(ui,|ui| {
            match &confirmation.action {
                RecoveryAction::Reset {device_id,..} => {
                    controls::section_heading(ui,"Reset This Device?","This creates a new device and fingerprint. People must verify the new fingerprint before protected conversations and calls can resume.");
                    controls::description(ui,"Previous encrypted history may become unreadable on this new device. The server cannot recover its keys. Relay keeps a Windows-protected archive of old local state, but automatic key restore is not available.");
                    if !device_id.is_empty() {controls::description(ui,&format!("Current device: {device_id}"));}
                    controls::description(ui,"Type RESET DEVICE to confirm.");
                },
                RecoveryAction::Revoke(identity) => {
                    controls::section_heading(ui,"Revoke Your Other Device?","This removes this device's server registration. Conversations must rotate their keys before that device can receive new protected content.");
                    fingerprint_label(ui,&identity.fingerprint);
                    controls::description(ui,&format!("Type this exact device ID to confirm: {}",identity.device_id));
                },
            }
            ui.add_sized([ui.available_width(),34.0],egui::TextEdit::singleline(&mut confirmation.entered)
                .font(egui::TextStyle::Monospace).char_limit(256).hint_text("Exact confirmation text"));
            ui.horizontal_wrapped(|ui| {
                submit=ui.add_enabled(confirmation.confirmed() && confirmation.pending.is_none(),
                    egui::Button::new(RichText::new(match &confirmation.action {RecoveryAction::Reset {..}=>"Reset Device",RecoveryAction::Revoke(_)=>"Revoke Device"}).color(c_bad()))).clicked();
                cancel=ui.add_enabled(confirmation.pending.is_none(),secondary("Cancel")).clicked();
                if confirmation.pending.is_some() {ui.label("Waiting for confirmation…");}
            });
        });
        if submit {
            confirmation.pending = Some(Instant::now());
            let url = confirmation.url.clone();
            let user_id = confirmation.user_id.clone();
            let action = confirmation.action.clone();
            let entered = confirmation.entered.clone();
            let had_voice = self.voice_channel.is_some() || self.joining_voice.is_some();
            self.stop_media();
            self.video.clear();
            if had_voice {
                self.send(Command::LeaveVoice);
            }
            match action {
                RecoveryAction::Reset { device_id, .. } => self.send(Command::CryptoResetDevice {
                    confirmation: entered,
                    url,
                    user_id,
                    expected_device_id: device_id,
                }),
                RecoveryAction::Revoke(identity) => self.send(Command::CryptoRevokeOwnDevice {
                    device_id: identity.device_id,
                    url,
                    user_id,
                }),
            }
        }
        if cancel {
            self.encryption_ui.recovery = None;
        }
    }
}

#[cfg(test)]
#[path = "approval_accessibility_tests.rs"]
mod approval_accessibility_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(fingerprint: &str) -> CryptoPeerStatus {
        CryptoPeerStatus {
            user_id: "person".into(),
            name: "Person".into(),
            device_id: "device".into(),
            fingerprint: fingerprint.into(),
            verified: false,
            changed: false,
        }
    }

    fn ready_status(peers: Vec<CryptoPeerStatus>) -> CryptoUiStatus {
        CryptoUiStatus {
            own_device_id: "own-device".into(),
            own_fingerprint: "0abc".repeat(16),
            supported: true,
            peers,
            ..Default::default()
        }
    }

    #[test]
    fn fixed_capture_cases_cover_grouping_and_failure_without_claiming_real_verification() {
        assert!(encryption_capture_status("normal").is_none());
        assert!(encryption_capture_status("profile-card").is_none());
        for panel in ["encryption-pending", "encryption-pending-expanded"] {
            let status = encryption_capture_status(panel).unwrap();
            assert_eq!(
                encryption_summary(&status, false),
                EncryptionSummary::NeedsConfirmation {
                    people: 2,
                    devices: 2
                }
            );
            let groups = people_and_devices(&status.peers);
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0].devices.len(), 2);
            assert!(status.status.contains("Synthetic layout fixture only"));
        }
        for (panel, expected) in [
            (
                "encryption-changed",
                EncryptionSummary::Changed { people: 1 },
            ),
            (
                "encryption-locally-confirmed",
                EncryptionSummary::LocallyConfirmed {
                    people: 2,
                    devices: 3,
                },
            ),
            ("encryption-empty", EncryptionSummary::NoOtherDevices),
            ("encryption-problem", EncryptionSummary::NeedsAttention),
        ] {
            let status = encryption_capture_status(panel).unwrap();
            assert_eq!(encryption_summary(&status, false), expected);
            assert!(status.status.contains("Synthetic layout fixture only"));
        }
        let failed = encryption_capture_status("encryption-problem").unwrap();
        assert!(failed.peers.iter().all(|peer| peer.verified));
        assert!(failed.problem.is_some());
    }

    #[test]
    fn page_summary_never_treats_an_empty_list_or_unready_device_as_protected() {
        assert_eq!(
            encryption_summary(&CryptoUiStatus::default(), false),
            EncryptionSummary::Unsupported
        );
        assert_eq!(
            encryption_summary(&CryptoUiStatus::default(), true),
            EncryptionSummary::NotReady
        );
        let empty = ready_status(vec![]);
        assert_eq!(
            encryption_summary(&empty, false),
            EncryptionSummary::NoOtherDevices
        );
        for own_fingerprint in [String::new(), "a".repeat(63), "x".repeat(64)] {
            let status = CryptoUiStatus {
                own_fingerprint,
                ..empty.clone()
            };
            assert_eq!(
                encryption_summary(&status, true),
                EncryptionSummary::NotReady
            );
        }
        assert_eq!(
            encryption_summary(
                &CryptoUiStatus {
                    own_device_id: String::new(),
                    ..empty
                },
                true
            ),
            EncryptionSummary::NotReady
        );
    }

    #[test]
    fn people_are_grouped_by_account_and_each_device_stays_visible_under_its_person() {
        let mut first = peer(&"a".repeat(64));
        first.device_id = "laptop".into();
        first.verified = true;
        let mut second = first.clone();
        second.device_id = "desktop".into();
        second.fingerprint = "b".repeat(64);
        second.verified = false;
        let mut same_name_other_account = first.clone();
        same_name_other_account.user_id = "someone-else".into();
        let status = ready_status(vec![
            first.clone(),
            second.clone(),
            same_name_other_account.clone(),
        ]);
        let people = people_and_devices(&status.peers);
        assert_eq!(
            people.len(),
            2,
            "equal display names must not merge separate people"
        );
        let owner = people
            .iter()
            .find(|person| person.user_id == first.user_id)
            .unwrap();
        assert_eq!(owner.devices.len(), 2);
        assert_eq!(owner.devices[0].device_id, "desktop");
        assert_eq!(owner.devices[1].device_id, "laptop");
        assert!(
            owner.needs_confirmation(),
            "one approved device cannot approve another"
        );
        assert_eq!(
            encryption_summary(&status, true),
            EncryptionSummary::NeedsConfirmation {
                people: 1,
                devices: 1
            }
        );
        let mut second_pending = same_name_other_account;
        second_pending.verified = false;
        assert_eq!(
            encryption_summary(&ready_status(vec![first, second, second_pending]), true),
            EncryptionSummary::NeedsConfirmation {
                people: 2,
                devices: 2
            }
        );
    }

    #[test]
    fn changed_devices_and_failures_override_cached_local_approvals() {
        let mut approved = peer(&"a".repeat(64));
        approved.verified = true;
        let clean = ready_status(vec![approved.clone()]);
        assert_eq!(
            encryption_summary(&clean, true),
            EncryptionSummary::LocallyConfirmed {
                people: 1,
                devices: 1
            }
        );
        let mut another_device = approved.clone();
        another_device.device_id = "another-device".into();
        another_device.fingerprint = "c".repeat(64);
        let mut another_person = approved.clone();
        another_person.user_id = "another-account".into();
        assert_eq!(
            encryption_summary(
                &ready_status(vec![approved.clone(), another_device, another_person]),
                true
            ),
            EncryptionSummary::LocallyConfirmed {
                people: 2,
                devices: 3
            }
        );
        let mut changed = approved.clone();
        changed.device_id = "replacement".into();
        changed.fingerprint = "b".repeat(64);
        changed.changed = true;
        let mut changed_again = changed.clone();
        changed_again.device_id = "other-replacement".into();
        assert_eq!(
            encryption_summary(&ready_status(vec![approved, changed, changed_again]), true),
            EncryptionSummary::Changed { people: 1 }
        );
        for problem in ["Could not read protected storage", "Refresh failed", ""] {
            assert_eq!(
                encryption_summary(
                    &CryptoUiStatus {
                        problem: Some(problem.into()),
                        ..clean.clone()
                    },
                    true
                ),
                EncryptionSummary::NeedsAttention,
                "even cached approvals must not hide an explicit failure"
            );
        }
    }

    #[test]
    fn reset_confirmation_cannot_follow_an_account_community_or_device_change() {
        let status = CryptoUiStatus {
            own_device_id: "current".into(),
            own_fingerprint: "a".repeat(64),
            ..Default::default()
        };
        let mut confirmation = RecoveryConfirmation {
            url: "http://community".into(),
            user_id: "own-user".into(),
            action: RecoveryAction::Reset {
                device_id: status.own_device_id.clone(),
                fingerprint: status.own_fingerprint.clone(),
            },
            entered: "RESET DEVICE".into(),
            pending: None,
        };
        assert!(confirmation.current("http://community", "own-user", &status));
        assert!(confirmation.confirmed());
        assert!(!confirmation.current("http://other", "own-user", &status));
        assert!(!confirmation.current("http://community", "other-user", &status));
        let changed = CryptoUiStatus {
            own_device_id: "replacement".into(),
            ..status.clone()
        };
        assert!(!confirmation.current("http://community", "own-user", &changed));
        for wrong in [
            "reset device",
            "RESET DEVICE ",
            "RESET DEVICE EXTRA",
            "ＲＥＳＥＴ DEVICE",
        ] {
            confirmation.entered = wrong.into();
            assert!(!confirmation.confirmed());
        }
        let failed = RecoveryConfirmation {
            url: "http://community".into(),
            user_id: "own-user".into(),
            action: RecoveryAction::Reset {
                device_id: String::new(),
                fingerprint: String::new(),
            },
            entered: "RESET DEVICE".into(),
            pending: None,
        };
        assert!(
            failed.current("http://community", "own-user", &CryptoUiStatus::default()),
            "a failed protected store can still be recovered explicitly"
        );
    }

    #[test]
    fn revocation_requires_exact_own_device_identity_and_never_revokes_current_or_peer_devices() {
        let mut own = peer(&"a".repeat(64));
        own.user_id = "own-user".into();
        let status = CryptoUiStatus {
            own_device_id: "current".into(),
            own_devices: vec![own.clone()],
            ..Default::default()
        };
        let confirmation = RecoveryConfirmation {
            url: "http://community".into(),
            user_id: "own-user".into(),
            action: RecoveryAction::Revoke(DeviceIdentity::from(&own)),
            entered: own.device_id.clone(),
            pending: None,
        };
        assert!(confirmation.current("http://community", "own-user", &status));
        assert!(confirmation.confirmed());
        let mut changed = own.clone();
        changed.fingerprint = "b".repeat(64);
        assert!(!confirmation.current(
            "http://community",
            "own-user",
            &CryptoUiStatus {
                own_devices: vec![changed],
                ..status.clone()
            }
        ));
        assert!(!confirmation.current(
            "http://community",
            "own-user",
            &CryptoUiStatus {
                own_device_id: own.device_id.clone(),
                ..status.clone()
            }
        ));
        assert!(!confirmation.current(
            "http://community",
            "own-user",
            &CryptoUiStatus {
                own_devices: vec![],
                peers: vec![own],
                ..status
            }
        ));
    }

    #[test]
    fn approval_requires_all_sixty_four_hex_characters_from_the_expected_fingerprint() {
        let fingerprint = "12ab".repeat(16);
        assert!(fingerprint_matches(&fingerprint, &"12ab12ab ".repeat(8)));
        assert!(fingerprint_matches(&fingerprint, &"12ab12ab\n".repeat(8)));
        assert!(fingerprint_matches(
            &fingerprint,
            &fingerprint.to_uppercase()
        ));
        for entered in [
            "12ab".repeat(15),
            format!("{fingerprint}0"),
            "12ac".repeat(16),
            "１２ａｂ".repeat(16),
            "zzab".repeat(16),
        ] {
            assert!(!fingerprint_matches(&fingerprint, &entered));
        }
        assert!(!fingerprint_matches("", ""));
        assert!(!fingerprint_matches(&"x".repeat(64), &"x".repeat(64)));
    }

    #[test]
    fn pasted_verification_card_is_bound_to_the_exact_community_account_device_and_code() {
        let peer = peer(&"12ab".repeat(16));
        let card = VerificationCard {
            relay_device_check: 1,
            community: "community-id".into(),
            account: peer.user_id.clone(),
            device: peer.device_id.clone(),
            security_code: peer.fingerprint.clone(),
        };
        let valid = serde_json::to_string_pretty(&card).unwrap();
        assert!(approval_matches(&peer, "community-id", &valid));
        assert!(!approval_matches(&peer, "different-community", &valid));
        assert!(!approval_matches(&peer, "", &valid));
        for (field, value) in [
            ("community", serde_json::json!("other")),
            ("account", serde_json::json!("other")),
            ("device", serde_json::json!("other")),
            ("security_code", serde_json::json!("12ac".repeat(16))),
            ("relay_device_check", serde_json::json!(2)),
            ("private_key", serde_json::json!("unexpected-field")),
        ] {
            let mut altered = serde_json::to_value(&card).unwrap();
            altered[field] = value;
            assert!(
                !approval_matches(&peer, "community-id", &altered.to_string()),
                "accepted changed {field}"
            );
        }
        assert!(!approval_matches(
            &peer,
            "community-id",
            &valid[..valid.len() - 1]
        ));
        assert!(!approval_matches(
            &peer,
            "community-id",
            &format!("{}{}", " ".repeat(4096), peer.fingerprint)
        ));
        assert!(approval_matches(&peer, "community-id", &peer.fingerprint));
    }

    #[test]
    fn a_changed_device_account_or_fingerprint_cannot_reuse_an_entered_approval() {
        let old = peer(&"a".repeat(64));
        let mut ui = EncryptionUi::default();
        ui.approvals.insert(
            DeviceIdentity::from(&old),
            Approval {
                entered: old.fingerprint.clone(),
                pending: Some(Instant::now()),
            },
        );
        let mut changed = old.clone();
        changed.fingerprint = "b".repeat(64);
        changed.changed = true;
        ui.apply_status(CryptoUiStatus {
            peers: vec![changed.clone()],
            ..Default::default()
        });
        assert!(ui.approvals.is_empty());
        ui.approvals
            .insert(DeviceIdentity::from(&changed), Approval::default());
        changed.user_id = "another-person".into();
        ui.apply_status(CryptoUiStatus {
            peers: vec![changed],
            ..Default::default()
        });
        assert!(ui.approvals.is_empty());
        ui.clear_session();
        assert!(ui.status.peers.is_empty());
        assert!(ui.status.own_fingerprint.is_empty());
    }

    #[test]
    fn protection_results_can_precede_rows_are_bounded_and_clear_at_account_boundaries() {
        let mut cache = ProtectionCache::default();
        cache.insert(
            "pending-row".into(),
            MessageProtection::Unavailable("Missing key".into()),
        );
        assert!(matches!(
            cache.get("pending-row"),
            Some(MessageProtection::Unavailable(_))
        ));
        cache.insert("pending-row".into(), MessageProtection::Protected);
        assert!(matches!(
            cache.get("pending-row"),
            Some(MessageProtection::Protected)
        ));
        for index in 0..2050 {
            cache.insert(index.to_string(), MessageProtection::Legacy);
        }
        assert_eq!(cache.values.len(), 2048);
        assert!(cache.get("pending-row").is_none());
        cache.clear();
        assert!(cache.get("2049").is_none());
    }

    #[test]
    fn private_notifications_do_not_disclose_message_text_unless_previews_are_explicit() {
        let message = Message {
            content: "A private decrypted message".into(),
            ..Default::default()
        };
        assert_eq!(notification_body(&message, false), "New message");
        assert_eq!(notification_body(&message, true), message.content);
        let long = Message {
            content: "ø".repeat(241),
            ..Default::default()
        };
        assert_eq!(notification_body(&long, true).chars().count(), 240);
        let attachment = Message::default();
        assert_eq!(notification_body(&attachment, false), "New message");
        assert_eq!(notification_body(&attachment, true), "Sent an attachment");
    }
}

#[cfg(test)]
mod safety_dialog_tests {
    use super::*;

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn click(at: Pos2) -> Vec<egui::Event> {
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![egui::Event::PointerMoved(at), press(true), press(false)]
    }

    fn rows() -> [String; 3] {
        crate::safety_number::rows(
            &crate::safety_number::safety_number(&"a1".repeat(32), &"b2".repeat(32)).unwrap(),
        )
    }

    fn context(font_scale: f32) -> egui::Context {
        let ctx = egui::Context::default();
        configure(&ctx);
        crate::appearance::AppearanceSettings {
            font_scale,
            ..Default::default()
        }
        .apply(&ctx, None);
        ctx
    }

    fn frame(
        ctx: &egui::Context,
        size: Vec2,
        mismatch: bool,
        events: Vec<egui::Event>,
    ) -> SafetyOutcome {
        let mut outcome = None;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                focused: true,
                events,
                ..Default::default()
            },
            |ctx| outcome = Some(safety_dialog_ui(ctx, "Anna", &rows(), mismatch)),
        );
        outcome.unwrap()
    }

    fn settle(ctx: &egui::Context, size: Vec2, mismatch: bool) -> SafetyOutcome {
        let mut last = frame(ctx, size, mismatch, vec![]);
        for _ in 0..3 {
            last = frame(ctx, size, mismatch, vec![]);
        }
        last
    }

    fn window(ctx: &egui::Context) -> Rect {
        ctx.memory(|memory| memory.area_rect(egui::Id::new("safety-number")))
            .unwrap()
    }

    /// Scrolls the dialog's body until `target` is inside the window (small
    /// windows with large text scroll their body under the fixed header).
    fn reveal(ctx: &egui::Context, size: Vec2, pick: impl Fn(&SafetyOutcome) -> Rect) -> Rect {
        let mut outcome = settle(ctx, size, false);
        for _ in 0..12 {
            if window(ctx).contains_rect(pick(&outcome)) {
                break;
            }
            outcome = frame(
                ctx,
                size,
                false,
                vec![
                    egui::Event::PointerMoved(window(ctx).center()),
                    egui::Event::Scroll(Vec2::new(0.0, -300.0)),
                ],
            );
        }
        let rect = pick(&frame(ctx, size, false, vec![]));
        assert!(
            window(ctx).contains_rect(rect),
            "button reachable: {rect:?} in {:?}",
            window(ctx)
        );
        rect
    }

    const CASES: [(Vec2, f32); 3] = [
        (Vec2::new(940.0, 650.0), 1.0),
        (Vec2::new(360.0, 420.0), 1.0),
        (Vec2::new(360.0, 420.0), 1.4),
    ];

    #[test]
    fn the_dialog_fits_the_screen_and_both_answers_are_reachable_and_distinct() {
        for (size, scale) in CASES {
            let case = format!("{size:?} x{scale}");
            let ctx = context(scale);
            let settled = settle(&ctx, size, false);
            assert_eq!(
                settled.action,
                SafetyAction::None,
                "{case}: idle frames do nothing"
            );
            assert!(
                Rect::from_min_size(Pos2::ZERO, size).contains_rect(window(&ctx)),
                "{case}"
            );
            let matches = reveal(&ctx, size, |o| o.matches);
            let clicked = frame(&ctx, size, false, click(matches.center()));
            assert_eq!(clicked.action, SafetyAction::Match, "{case}: They match");
            let ctx = context(scale);
            settle(&ctx, size, false);
            let differs = reveal(&ctx, size, |o| o.mismatch);
            let clicked = frame(&ctx, size, false, click(differs.center()));
            assert_eq!(
                clicked.action,
                SafetyAction::Mismatch,
                "{case}: They don't match"
            );
            assert_ne!(matches, differs, "{case}: two separate buttons");
        }
    }

    #[test]
    fn escape_closes_the_dialog_and_other_keys_never_approve() {
        for (size, scale) in CASES {
            let case = format!("{size:?} x{scale}");
            let ctx = context(scale);
            settle(&ctx, size, false);
            for other in [egui::Key::Enter, egui::Key::Space, egui::Key::Tab] {
                let outcome = frame(&ctx, size, false, vec![key(other)]);
                assert_ne!(
                    outcome.action,
                    SafetyAction::Match,
                    "{case}: {other:?} must not approve"
                );
                assert_ne!(outcome.action, SafetyAction::Close, "{case}: {other:?}");
            }
            let escaped = frame(&ctx, size, false, vec![key(egui::Key::Escape)]);
            assert_eq!(escaped.action, SafetyAction::Close, "{case}: Escape closes");
        }
    }

    #[test]
    fn after_a_mismatch_there_is_no_way_to_approve() {
        let size = Vec2::new(940.0, 650.0);
        let ctx = context(1.0);
        let outcome = settle(&ctx, size, true);
        assert_eq!(
            outcome.matches,
            Rect::NOTHING,
            "no They match button after a mismatch"
        );
        assert_eq!(outcome.mismatch, Rect::NOTHING);
        // Escape still closes without approving.
        let escaped = frame(&ctx, size, true, vec![key(egui::Key::Escape)]);
        assert_eq!(escaped.action, SafetyAction::Close);
    }
}
