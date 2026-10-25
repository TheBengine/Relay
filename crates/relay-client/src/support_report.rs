//! A short support report the person can read before deciding to share it.
//!
//! Local only: nothing is uploaded, opened in a browser, copied to the
//! clipboard or probed on the network. The report is built from fixed
//! vocabularies, so no text from the app (an error message, a name, an address,
//! a path, a token) can enter it: a `Facts` value holds only enums, bounded
//! counts and two checked identifiers, and `render` writes only words from this
//! file. `is_clean` re-checks the finished text against the same vocabulary and
//! runs again before anything is saved. Unknown stays "unknown": a service that
//! the app has no recorded status for is never reported as healthy.
use super::*;

pub(super) const MAX_BYTES: usize = 4096;
const MAX_COUNT: u16 = 999;

const TITLE: &str = "Relay support report";
const RECORDED_NOTE: &str =
    "Connection states were last recorded by this app; this report does not test the network.";
const INTRO: [&str; 3] = [
    "Made on this PC at your request. Relay did not send it anywhere.",
    "Read it first. You decide whether to share it.",
    RECORDED_NOTE,
];
const SECTIONS: [&str; 5] = ["App", "Relay account", "Community", "Friends", "Video"];
const OUTRO: [&str; 2] = [
    "Left out on purpose:",
    "messages, files, names, ids, addresses, passwords, keys and logs.",
];

// ---------------------------------------------------------------------------
// Vocabulary

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccountState {
    SignedIn,
    SignedOut,
    SigningIn,
    Expired,
    NotSetUp,
}

impl AccountState {
    const ALL: [Self; 5] = [
        Self::SignedIn,
        Self::SignedOut,
        Self::SigningIn,
        Self::Expired,
        Self::NotSetUp,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::SignedIn => "signed in",
            Self::SignedOut => "signed out",
            Self::SigningIn => "signing in",
            Self::Expired => "session expired",
            Self::NotSetUp => "not set up in this copy",
        }
    }
    fn hint(self, problem: bool) -> &'static str {
        if problem {
            return HINT_ACCOUNT_PROBLEM;
        }
        match self {
            Self::SignedIn => HINT_NONE,
            Self::SignedOut => HINT_ACCOUNT_SIGN_IN,
            Self::SigningIn => HINT_ACCOUNT_FINISH,
            Self::Expired => HINT_ACCOUNT_AGAIN,
            Self::NotSetUp => HINT_UPDATE,
        }
    }
}

/// The last connection state the app recorded for a community or for Friends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Link {
    Connected,
    Reconnecting,
    Offline,
    SignInRequired,
}

impl Link {
    const ALL: [Self; 4] = [
        Self::Connected,
        Self::Reconnecting,
        Self::Offline,
        Self::SignInRequired,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Offline => "offline",
            Self::SignInRequired => "sign-in required",
        }
    }
    fn hint(self) -> &'static str {
        match self {
            Self::Connected => HINT_NONE,
            Self::Reconnecting => HINT_WAIT,
            Self::Offline => HINT_OFFLINE,
            Self::SignInRequired => HINT_SIGN_IN_AGAIN,
        }
    }
}

impl From<&ConnectionStatus> for Link {
    fn from(status: &ConnectionStatus) -> Self {
        match status {
            ConnectionStatus::Connected => Self::Connected,
            ConnectionStatus::Reconnecting => Self::Reconnecting,
            ConnectionStatus::Offline => Self::Offline,
            ConnectionStatus::SignInRequired => Self::SignInRequired,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CurrentCommunity {
    NoneOpen,
    Link(Link),
}

impl CurrentCommunity {
    fn label(self) -> &'static str {
        match self {
            Self::NoneOpen => "none open",
            Self::Link(link) => link.label(),
        }
    }
    fn hint(self) -> &'static str {
        match self {
            Self::NoneOpen => HINT_OPEN_COMMUNITY,
            Self::Link(link) => link.hint(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FriendsState {
    NotSetUp,
    NeedsAccount,
    /// No status has been recorded. This is not "fine" and not "down".
    Unknown,
    Link(Link),
}

impl FriendsState {
    fn label(self) -> &'static str {
        match self {
            Self::NotSetUp => "not set up in this copy",
            Self::NeedsAccount => "needs a Relay account sign-in",
            Self::Unknown => "unknown",
            Self::Link(link) => link.label(),
        }
    }
    fn hint(self) -> &'static str {
        match self {
            Self::NotSetUp => HINT_UPDATE,
            Self::NeedsAccount => HINT_ACCOUNT_SIGN_IN,
            Self::Unknown => HINT_OPEN_FRIENDS,
            Self::Link(link) => link.hint(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VideoState {
    NotIncluded,
    NotInUse,
    Starting,
    Active,
    Reconnecting,
}

impl VideoState {
    fn label(self) -> &'static str {
        match self {
            Self::NotIncluded => "not included in this copy",
            Self::NotInUse => "not in use",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Reconnecting => "reconnecting",
        }
    }
    fn hint(self) -> &'static str {
        match self {
            Self::NotIncluded => HINT_UPDATE,
            Self::NotInUse => HINT_VIDEO_JOIN,
            Self::Starting | Self::Reconnecting => HINT_WAIT,
            Self::Active => HINT_NONE,
        }
    }
}

const HINT_NONE: &str = "Nothing to do.";
const HINT_UPDATE: &str = "Install the latest full version of Relay.";
const HINT_WAIT: &str = "Wait a moment; Relay is connecting.";
const HINT_OFFLINE: &str = "Check your internet connection and that the host's Relay is running.";
const HINT_SIGN_IN_AGAIN: &str = "Sign in to this community again.";
const HINT_OPEN_COMMUNITY: &str = "Open a community from the list on the left.";
const HINT_OPEN_FRIENDS: &str = "Open Friends once so Relay can check it.";
const HINT_ACCOUNT_SIGN_IN: &str = "Sign in to your Relay account.";
const HINT_ACCOUNT_FINISH: &str = "Finish signing in, in the sign-in window or your browser.";
const HINT_ACCOUNT_AGAIN: &str = "Sign in to your Relay account again.";
const HINT_ACCOUNT_PROBLEM: &str =
    "Try signing in again. If it keeps failing, share this report with support.";
const HINT_VIDEO_JOIN: &str = "Video starts when you join a call.";

const HINTS: [&str; 12] = [
    HINT_NONE,
    HINT_UPDATE,
    HINT_WAIT,
    HINT_OFFLINE,
    HINT_SIGN_IN_AGAIN,
    HINT_OPEN_COMMUNITY,
    HINT_OPEN_FRIENDS,
    HINT_ACCOUNT_SIGN_IN,
    HINT_ACCOUNT_FINISH,
    HINT_ACCOUNT_AGAIN,
    HINT_ACCOUNT_PROBLEM,
    HINT_VIDEO_JOIN,
];

/// How many saved communities are in each recorded state. Counts only, never names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub connected: u16,
    pub reconnecting: u16,
    pub offline: u16,
    pub sign_in_required: u16,
    /// No status has been recorded for these yet.
    pub not_yet_checked: u16,
}

impl Counts {
    pub(super) fn add(&mut self, link: Option<Link>) {
        let slot = match link {
            Some(Link::Connected) => &mut self.connected,
            Some(Link::Reconnecting) => &mut self.reconnecting,
            Some(Link::Offline) => &mut self.offline,
            Some(Link::SignInRequired) => &mut self.sign_in_required,
            None => &mut self.not_yet_checked,
        };
        *slot = slot.saturating_add(1).min(MAX_COUNT);
    }

    fn total(&self) -> u32 {
        [
            self.connected,
            self.reconnecting,
            self.offline,
            self.sign_in_required,
            self.not_yet_checked,
        ]
        .into_iter()
        .map(u32::from)
        .sum()
    }

    fn line(&self) -> String {
        format!(
            "{} (connected {}, reconnecting {}, offline {}, sign-in required {}, not yet checked {})",
            self.total(),
            self.connected,
            self.reconnecting,
            self.offline,
            self.sign_in_required,
            self.not_yet_checked
        )
    }
}

// ---------------------------------------------------------------------------
// The checked identifiers

fn valid_version(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

fn valid_build(value: &str) -> bool {
    let Some((sequence, digest)) = value.split_once('-') else {
        return false;
    };
    (1..=20).contains(&sequence.len())
        && sequence.bytes().all(|b| b.is_ascii_digit())
        && digest.len() == 12
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

fn os_label(os: &str) -> &'static str {
    match os {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "macos",
        _ => "other",
    }
}

fn arch_label(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "other",
    }
}

/// Everything the report can say. Nothing here is free text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Facts {
    version: String,
    build: String,
    os: &'static str,
    arch: &'static str,
    date: String,
    pub account: AccountState,
    pub account_problem: bool,
    pub current: CurrentCommunity,
    pub saved: Counts,
    pub friends: FriendsState,
    pub video: VideoState,
}

impl Facts {
    /// Identifiers that do not match their strict pattern become "unknown"
    /// instead of being copied.
    pub(super) fn new(
        version: &str,
        build: &str,
        os: &str,
        arch: &str,
        date: &str,
        rest: Rest,
    ) -> Self {
        Self {
            version: if valid_version(version) {
                version
            } else {
                "unknown"
            }
            .into(),
            build: if valid_build(build) { build } else { "unknown" }.into(),
            os: os_label(os),
            arch: arch_label(arch),
            date: if valid_date(date) { date } else { "unknown" }.into(),
            account: rest.account,
            account_problem: rest.account_problem,
            current: rest.current,
            saved: rest.saved,
            friends: rest.friends,
            video: rest.video,
        }
    }
}

/// The states, apart from the identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rest {
    pub account: AccountState,
    pub account_problem: bool,
    pub current: CurrentCommunity,
    pub saved: Counts,
    pub friends: FriendsState,
    pub video: VideoState,
}

pub(super) fn account_state(
    provider_configured: bool,
    signed_in_until: Option<u64>,
    waiting: bool,
    now: u64,
) -> AccountState {
    if !provider_configured {
        AccountState::NotSetUp
    } else if let Some(until) = signed_in_until {
        if until > now {
            AccountState::SignedIn
        } else {
            AccountState::Expired
        }
    } else if waiting {
        AccountState::SigningIn
    } else {
        AccountState::SignedOut
    }
}

pub(super) fn friends_state(
    configured: bool,
    account_signed_in: bool,
    recorded: Option<Link>,
) -> FriendsState {
    if !configured {
        FriendsState::NotSetUp
    } else if !account_signed_in {
        FriendsState::NeedsAccount
    } else {
        recorded.map_or(FriendsState::Unknown, FriendsState::Link)
    }
}

pub(super) fn video_state(
    included: bool,
    active: bool,
    starting: bool,
    reconnecting: bool,
) -> VideoState {
    if !included {
        VideoState::NotIncluded
    } else if reconnecting {
        VideoState::Reconnecting
    } else if starting {
        VideoState::Starting
    } else if active {
        VideoState::Active
    } else {
        VideoState::NotInUse
    }
}

// ---------------------------------------------------------------------------
// The text

pub(super) fn render(facts: &Facts) -> String {
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line(TITLE);
    for text in INTRO {
        line(text);
    }
    line("");
    line(SECTIONS[0]);
    line(&format!("Version: {}", facts.version));
    line(&format!("Build: {}", facts.build));
    line(&format!("Platform: {} {}", facts.os, facts.arch));
    line(&format!("Report date: {}", facts.date));
    line("");
    line(SECTIONS[1]);
    line(&format!("Account status: {}", facts.account.label()));
    line(&format!(
        "Problem noted: {}",
        if facts.account_problem { "yes" } else { "none" }
    ));
    line(&format!(
        "Next step: {}",
        facts.account.hint(facts.account_problem)
    ));
    line("");
    line(SECTIONS[2]);
    line(&format!("Current community: {}", facts.current.label()));
    line(&format!("Saved communities: {}", facts.saved.line()));
    line(&format!("Next step: {}", facts.current.hint()));
    line("");
    line(SECTIONS[3]);
    line(&format!("Friends status: {}", facts.friends.label()));
    line(&format!("Next step: {}", facts.friends.hint()));
    line("");
    line(SECTIONS[4]);
    line(&format!("Video session: {}", facts.video.label()));
    line("Video service health: unknown");
    line(&format!("Next step: {}", facts.video.hint()));
    line("");
    for text in OUTRO {
        line(text);
    }
    out
}

fn parse_counts(value: &str) -> Option<Counts> {
    let (total, rest) = value.split_once(" (")?;
    let inside = rest.strip_suffix(')')?;
    let mut numbers = Vec::new();
    for (part, name) in inside.split(", ").zip([
        "connected ",
        "reconnecting ",
        "offline ",
        "sign-in required ",
        "not yet checked ",
    ]) {
        let number = part.strip_prefix(name)?;
        if number.is_empty() || number.len() > 3 || !number.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        numbers.push(number.parse::<u16>().ok()?);
    }
    if numbers.len() != 5 || inside.split(", ").count() != 5 {
        return None;
    }
    let counts = Counts {
        connected: numbers[0],
        reconnecting: numbers[1],
        offline: numbers[2],
        sign_in_required: numbers[3],
        not_yet_checked: numbers[4],
    };
    (numbers.iter().all(|n| *n <= MAX_COUNT) && total == counts.total().to_string())
        .then_some(counts)
}

fn value_ok(key: &str, value: &str) -> bool {
    match key {
        "Version" => value == "unknown" || valid_version(value),
        "Build" => value == "unknown" || valid_build(value),
        "Platform" => value.split_once(' ').is_some_and(|(os, arch)| {
            ["windows", "linux", "macos", "other"].contains(&os)
                && ["x86_64", "aarch64", "other"].contains(&arch)
        }),
        "Report date" => value == "unknown" || valid_date(value),
        "Account status" => AccountState::ALL.iter().any(|s| s.label() == value),
        "Problem noted" => value == "none" || value == "yes",
        "Current community" => value == "none open" || Link::ALL.iter().any(|l| l.label() == value),
        "Saved communities" => parse_counts(value).is_some_and(|c| c.line() == value),
        "Friends status" => {
            [
                "not set up in this copy",
                "needs a Relay account sign-in",
                "unknown",
            ]
            .contains(&value)
                || Link::ALL.iter().any(|l| l.label() == value)
        }
        "Video session" => [
            VideoState::NotIncluded,
            VideoState::NotInUse,
            VideoState::Starting,
            VideoState::Active,
            VideoState::Reconnecting,
        ]
        .iter()
        .any(|s| s.label() == value),
        "Video service health" => value == "unknown",
        "Next step" => HINTS.contains(&value),
        _ => false,
    }
}

/// True only if every line is a fixed sentence or `Key: value` with a value from
/// that key's own vocabulary, in plain ASCII and within the size limit. Run on
/// the finished text, so a bug elsewhere cannot widen what is saved.
pub(super) fn is_clean(text: &str) -> bool {
    if text.is_empty()
        || text.len() > MAX_BYTES
        || !text.ends_with('\n')
        || !text.lines().any(|line| line == RECORDED_NOTE)
        || !text
            .chars()
            .all(|c| c == '\n' || (c.is_ascii() && !c.is_ascii_control()))
    {
        return false;
    }
    let fixed = |line: &str| {
        line.is_empty()
            || line == TITLE
            || INTRO.contains(&line)
            || SECTIONS.contains(&line)
            || OUTRO.contains(&line)
    };
    text.lines().all(|line| {
        fixed(line)
            || line
                .split_once(": ")
                .is_some_and(|(key, value)| value_ok(key, value))
    })
}

// ---------------------------------------------------------------------------
// Saving exactly what was previewed

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Saved,
    AlreadyExists,
    Failed,
    Refused,
}

impl Outcome {
    fn text(self) -> &'static str {
        match self {
            Self::Saved => "Saved. Relay did not open or send the file.",
            Self::AlreadyExists => {
                "That file already exists. Relay never replaces a file; choose a new name."
            }
            Self::Failed => "Relay could not save the report there. Choose another place.",
            Self::Refused => {
                "Relay did not save the report because it failed its own safety check."
            }
        }
    }
}

/// Write the previewed bytes to a file that must not exist yet. An existing
/// file is never replaced, and text that fails the vocabulary check is never written.
pub(super) fn save_new(path: &std::path::Path, text: &str) -> Outcome {
    use std::io::Write;
    if !is_clean(text) {
        return Outcome::Refused;
    }
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Outcome::AlreadyExists
        }
        Err(_) => return Outcome::Failed,
    };
    if file
        .write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .is_err()
    {
        drop(file);
        // Do not leave a half-written report behind.
        let _ = std::fs::remove_file(path);
        return Outcome::Failed;
    }
    Outcome::Saved
}

#[derive(Default)]
pub(super) struct State {
    preview: Option<String>,
    outcome: Option<Outcome>,
}

/// The open session counts as a community only if it is not the Friends
/// service, which is reported on its own line.
pub(super) fn current_community(
    session_open: bool,
    session_is_friends: bool,
    live: Link,
) -> CurrentCommunity {
    if session_open && !session_is_friends {
        CurrentCommunity::Link(live)
    } else {
        CurrentCommunity::NoneOpen
    }
}

// ---------------------------------------------------------------------------
// Connection status on the Help page

const STATUS_NOTE: &str = "Last recorded by this app on this PC. This is not a network test.";
const VIDEO_NOTE: &str = "Video service health is unknown. A video session state does not show it.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StatusRow {
    pub name: &'static str,
    pub state: &'static str,
    pub next: Option<&'static str>,
    pub note: Option<String>,
    /// Offer the existing Account page (a settings tab; nothing is started).
    pub open_account: bool,
}

fn next_step(hint: &'static str) -> Option<&'static str> {
    (hint != HINT_NONE).then_some(hint)
}

/// One row per service, kept separate, from the same closed enums the report uses.
pub(super) fn status_rows(rest: &Rest) -> [StatusRow; 4] {
    let account_needs_you = matches!(
        rest.account,
        AccountState::SignedOut | AccountState::SigningIn | AccountState::Expired
    ) || (rest.account_problem && rest.account != AccountState::NotSetUp);
    [
        StatusRow {
            name: "Relay account",
            state: rest.account.label(),
            next: next_step(rest.account.hint(rest.account_problem)),
            note: None,
            open_account: account_needs_you,
        },
        StatusRow {
            name: "Current community",
            state: rest.current.label(),
            next: next_step(rest.current.hint()),
            note: Some(format!("Saved communities: {}", rest.saved.line())),
            open_account: false,
        },
        StatusRow {
            name: "Friends",
            state: rest.friends.label(),
            next: next_step(rest.friends.hint()),
            note: None,
            open_account: rest.friends == FriendsState::NeedsAccount,
        },
        StatusRow {
            name: "Video",
            state: rest.video.label(),
            next: next_step(rest.video.hint()),
            note: Some(VIDEO_NOTE.into()),
            open_account: false,
        },
    ]
}

/// Draws the rows. Returns true when an Open Account button was clicked.
pub(super) fn status_content(ui: &mut egui::Ui, rows: &[StatusRow; 4]) -> bool {
    crate::settings_ui::divider(ui);
    crate::settings_ui::section_heading(ui, "Connection status", STATUS_NOTE);
    let mut open = false;
    for row in rows {
        ui.push_id(row.name, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(row.name).strong());
                ui.label(row.state);
            });
            let small = |text: String| {
                egui::Label::new(
                    RichText::new(text)
                        .size(font_size(TEXT_CAPTION))
                        .color(c_muted()),
                )
                .wrap(true)
            };
            if let Some(next) = row.next {
                ui.add(small(format!("Next step: {next}")));
            }
            if let Some(note) = &row.note {
                ui.add(small(note.clone()));
            }
            if row.open_account && ui.add(secondary("Open Account")).clicked() {
                open = true;
            }
            ui.add_space(8.0);
        });
    }
    open
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl RelayApp {
    /// Read the facts from state the app already holds. No new probe, no log,
    /// no file and no network is touched, and no free-form text is read.
    fn support_rest(&self) -> Rest {
        let now = now_secs();
        let people = self.people_url();
        let signed_in_until = self.central_account.account.as_ref().map(|a| a.expires_at);
        let account = account_state(
            self.central_account.provider.is_some(),
            signed_in_until,
            self.central_account.waiting(),
            now,
        );
        let signed_in = account == AccountState::SignedIn;
        // The signed-in community's live state, otherwise the last recorded one.
        // A community without a recorded state stays unknown.
        let recorded = |url: &str| -> Option<Link> {
            if url == self.login.url && self.session.is_some() {
                Some(Link::from(&self.connection_status))
            } else {
                self.community_statuses.get(url).map(Link::from)
            }
        };
        let mut saved = Counts::default();
        for community in &self.prefs.communities {
            if people.as_deref() == Some(community.url.as_str()) {
                continue;
            }
            saved.add(recorded(&community.url));
        }
        let current = current_community(
            self.session.is_some(),
            people.as_deref() == Some(self.login.url.as_str()),
            Link::from(&self.connection_status),
        );
        let friends = friends_state(
            people.is_some(),
            signed_in,
            people.as_deref().and_then(recorded),
        );
        let video = video_state(
            cfg!(feature = "native-rtc"),
            self.rtc_active(),
            self.rtc_starting(),
            self.rtc_reconnecting(),
        );
        Rest {
            account,
            account_problem: self.central_account.error.is_some(),
            current,
            saved,
            friends,
            video,
        }
    }

    fn support_facts(&self) -> Facts {
        let build = crate::build_metadata::current();
        Facts::new(
            build.version,
            &build.build_id(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            &chrono::Utc::now().format("%Y-%m-%d").to_string(),
            self.support_rest(),
        )
    }

    /// The Help page's compact status list. It reads the same recorded state as
    /// the report (`support_rest`), so there is one collector, no probe and no
    /// polling: it is drawn from what the app already holds.
    pub(super) fn connection_status_section(&mut self, ui: &mut egui::Ui) {
        let rows = status_rows(&self.support_rest());
        if status_content(ui, &rows) {
            self.settings_tab = SettingsTab::Account;
        }
    }

    /// The Help page section: preview first, then an explicit save.
    pub(super) fn support_report_section(&mut self, ui: &mut egui::Ui) {
        ui.add_space(18.0);
        settings_heading(
            ui,
            "Support report",
            "A short summary you can read before you decide to share it. Relay never sends it anywhere.",
        );
        let mut make = false;
        let mut save = false;
        let mut close = false;
        ui.horizontal_wrapped(|ui| {
            let has_preview = self.support_report.preview.is_some();
            make = ui
                .add(secondary(if has_preview {
                    "Refresh preview"
                } else {
                    "Preview report"
                }))
                .clicked();
            if has_preview {
                save = ui.add(primary("Save report…")).clicked();
                close = ui.add(secondary("Close preview")).clicked();
            }
        });
        if make {
            let text = render(&self.support_facts());
            let clean = is_clean(&text);
            self.support_report.preview = clean.then_some(text);
            self.support_report.outcome = (!clean).then_some(Outcome::Refused);
        }
        if close {
            self.support_report.preview = None;
            self.support_report.outcome = None;
        }
        if save {
            if let Some(text) = &self.support_report.preview {
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name("Relay-support-report.txt")
                    .add_filter("Text file", &["txt"])
                    .save_file()
                {
                    self.support_report.outcome = Some(save_new(&path, text));
                }
            }
        }
        if let Some(text) = &self.support_report.preview {
            let mut shown: &str = text.as_str();
            ui.add(
                egui::TextEdit::multiline(&mut shown)
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(16)
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{} bytes. Save writes exactly this text, and only if you choose a new file name.",
                        text.len()
                    ))
                    .size(font_size(TEXT_CAPTION))
                    .color(c_muted()),
                )
                .wrap(true),
            );
        }
        if let Some(outcome) = self.support_report.outcome {
            let color = if outcome == Outcome::Saved {
                c_good()
            } else {
                c_bad()
            };
            ui.add(egui::Label::new(RichText::new(outcome.text()).color(color)).wrap(true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rest() -> Rest {
        Rest {
            account: AccountState::SignedIn,
            account_problem: false,
            current: CurrentCommunity::Link(Link::Connected),
            saved: Counts::default(),
            friends: FriendsState::Link(Link::Connected),
            video: VideoState::NotInUse,
        }
    }

    fn facts() -> Facts {
        Facts::new(
            "0.10.0",
            "20261023000001-4f2a9c1b3d7e",
            "windows",
            "x86_64",
            "2026-10-23",
            rest(),
        )
    }

    #[test]
    fn a_normal_report_is_clean_small_and_ends_with_what_is_left_out() {
        let text = render(&facts());
        assert!(is_clean(&text), "{text}");
        assert!(text.len() < MAX_BYTES / 2, "{}", text.len());
        assert!(text.starts_with("Relay support report\n"));
        assert!(text.contains("Version: 0.10.0\n"));
        assert!(text.contains("Build: 20261023000001-4f2a9c1b3d7e\n"));
        assert!(text.contains("Platform: windows x86_64\n"));
        assert!(text.contains("Left out on purpose:\n"));
        // The four services are separate sections with their own lines.
        for section in ["Relay account\n", "Community\n", "Friends\n", "Video\n"] {
            assert!(text.contains(section), "{section}");
        }
        assert!(text.contains("Account status: signed in\n"));
        assert!(text.contains("Current community: connected\n"));
        assert!(text.contains("Friends status: connected\n"));
        assert!(text.contains("Video session: not in use\n"));
    }

    #[test]
    fn hostile_strings_cannot_enter_through_the_only_free_fields() {
        let hostile = [
            "Bearer abc.def.ghi",
            "token=secret123",
            "https://host.example/path?token=secret#frag",
            r"C:\Users\someone\AppData\Relay",
            "user@example.com",
            "4be44aef-8c98-4db6-a9d5-81c2eb311330",
            "0.10.0\nPassword: hunter2",
            "ünïcode",
            &"9".repeat(500),
        ];
        for bad in hostile {
            let facts = Facts::new(bad, bad, bad, bad, bad, rest());
            let text = render(&facts);
            assert!(is_clean(&text), "{bad}: {text}");
            assert!(text.contains("Version: unknown\n"), "{bad}");
            assert!(text.contains("Build: unknown\n"), "{bad}");
            assert!(text.contains("Platform: other other\n"), "{bad}");
            assert!(text.contains("Report date: unknown\n"), "{bad}");
            for piece in [
                "secret", "Bearer", "hunter2", "example", "Users", "4be44aef",
            ] {
                assert!(!text.contains(piece), "{piece} leaked for {bad}");
            }
        }
    }

    #[test]
    fn identifiers_must_match_their_pattern_exactly() {
        assert!(valid_version("0.10.0") && valid_version("1.2.3-beta+4"));
        assert!(!valid_version("") && !valid_version("a b") && !valid_version(&"1".repeat(33)));
        assert!(valid_build("20261023000001-4f2a9c1b3d7e"));
        for bad in [
            "20261023000001-4F2A9C1B3D7E",
            "20261023000001-4f2a9c1b3d7",
            "x-4f2a9c1b3d7e",
            "20261023000001",
            "-4f2a9c1b3d7e",
            "20261023000001-4f2a9c1b3d7e-extra",
        ] {
            assert!(!valid_build(bad), "{bad}");
        }
        assert!(valid_date("2026-10-23") && !valid_date("2026/10/23") && !valid_date("26-10-23"));
    }

    #[test]
    fn unknown_stays_unknown_and_is_never_reported_as_healthy() {
        // No status was ever recorded for Friends or for a saved community.
        assert_eq!(friends_state(true, true, None), FriendsState::Unknown);
        let mut counts = Counts::default();
        counts.add(None);
        counts.add(None);
        counts.add(Some(Link::Connected));
        assert_eq!((counts.not_yet_checked, counts.connected), (2, 1));
        let text = render(&Facts::new(
            "0.10.0",
            "20261023000001-4f2a9c1b3d7e",
            "windows",
            "x86_64",
            "2026-10-23",
            Rest {
                friends: FriendsState::Unknown,
                saved: counts,
                ..rest()
            },
        ));
        assert!(is_clean(&text));
        assert!(text.contains("Friends status: unknown\n"));
        assert!(text.contains("Saved communities: 3 (connected 1, reconnecting 0, offline 0, sign-in required 0, not yet checked 2)\n"));
        // The app cannot know the video service's health, whatever the session does.
        for state in [
            VideoState::NotIncluded,
            VideoState::NotInUse,
            VideoState::Starting,
            VideoState::Active,
            VideoState::Reconnecting,
        ] {
            let text = render(&Facts::new(
                "0.10.0",
                "20261023000001-4f2a9c1b3d7e",
                "windows",
                "x86_64",
                "2026-10-23",
                Rest {
                    video: state,
                    ..rest()
                },
            ));
            assert!(text.contains("Video service health: unknown\n"));
        }
    }

    #[test]
    fn states_map_from_what_the_app_already_knows() {
        use AccountState::*;
        assert_eq!(account_state(false, None, false, 100), NotSetUp);
        assert_eq!(account_state(false, Some(500), true, 100), NotSetUp);
        assert_eq!(account_state(true, Some(500), false, 100), SignedIn);
        assert_eq!(account_state(true, Some(100), false, 100), Expired);
        assert_eq!(account_state(true, Some(50), true, 100), Expired);
        assert_eq!(account_state(true, None, true, 100), SigningIn);
        assert_eq!(account_state(true, None, false, 100), SignedOut);
        assert_eq!(friends_state(false, true, None), FriendsState::NotSetUp);
        assert_eq!(
            friends_state(true, false, Some(Link::Connected)),
            FriendsState::NeedsAccount
        );
        assert_eq!(
            friends_state(true, true, Some(Link::Offline)),
            FriendsState::Link(Link::Offline)
        );
        assert_eq!(
            video_state(false, true, true, true),
            VideoState::NotIncluded
        );
        assert_eq!(video_state(true, false, false, false), VideoState::NotInUse);
        assert_eq!(video_state(true, false, true, false), VideoState::Starting);
        assert_eq!(video_state(true, true, false, false), VideoState::Active);
        assert_eq!(
            video_state(true, true, false, true),
            VideoState::Reconnecting
        );
        assert_eq!(
            Link::from(&ConnectionStatus::SignInRequired),
            Link::SignInRequired
        );
        assert_eq!(Link::from(&ConnectionStatus::Offline), Link::Offline);
    }

    #[test]
    fn every_combination_renders_clean_and_within_the_limit() {
        let mut largest = 0;
        for account in AccountState::ALL {
            for problem in [false, true] {
                for current in std::iter::once(CurrentCommunity::NoneOpen)
                    .chain(Link::ALL.map(CurrentCommunity::Link))
                {
                    for friends in [
                        FriendsState::NotSetUp,
                        FriendsState::NeedsAccount,
                        FriendsState::Unknown,
                    ]
                    .into_iter()
                    .chain(Link::ALL.map(FriendsState::Link))
                    {
                        for video in [
                            VideoState::NotIncluded,
                            VideoState::NotInUse,
                            VideoState::Starting,
                            VideoState::Active,
                            VideoState::Reconnecting,
                        ] {
                            let saved = Counts {
                                connected: MAX_COUNT,
                                reconnecting: MAX_COUNT,
                                offline: MAX_COUNT,
                                sign_in_required: MAX_COUNT,
                                not_yet_checked: MAX_COUNT,
                            };
                            let text = render(&Facts::new(
                                &"9".repeat(32),
                                "99999999999999999999-ffffffffffff",
                                "windows",
                                "aarch64",
                                "9999-99-99",
                                Rest {
                                    account,
                                    account_problem: problem,
                                    current,
                                    saved,
                                    friends,
                                    video,
                                },
                            ));
                            assert!(is_clean(&text), "{text}");
                            largest = largest.max(text.len());
                        }
                    }
                }
            }
        }
        assert!(largest < MAX_BYTES, "{largest}");
    }

    #[test]
    fn counts_saturate_instead_of_growing_without_bound() {
        let mut counts = Counts::default();
        for _ in 0..(MAX_COUNT as usize + 50) {
            counts.add(Some(Link::Offline));
        }
        assert_eq!(counts.offline, MAX_COUNT);
        assert!(is_clean(&render(&Facts::new(
            "0.10.0",
            "20261023000001-4f2a9c1b3d7e",
            "windows",
            "x86_64",
            "2026-10-23",
            Rest {
                saved: counts,
                ..rest()
            },
        ))));
    }

    #[test]
    fn anything_outside_the_vocabulary_is_refused() {
        let good = render(&facts());
        assert!(is_clean(&good));
        let tampered = [
            format!("{good}Token: abc\n"),
            format!("{good}Server: 4be44aef-8c98-4db6-a9d5-81c2eb311330\n"),
            good.replace(
                "Account status: signed in",
                "Account status: signed in as bo",
            ),
            good.replace(
                "Current community: connected",
                "Current community: Friends of Bo",
            ),
            good.replace(
                "Next step: Nothing to do.",
                "Next step: open C:\\Users\\bo\\log.txt",
            ),
            good.replace("Version: 0.10.0", "Version: 0.10.0 https://x.example/?t=1"),
            good.replace("windows x86_64", "windows x86_64 extra"),
            good.replace(
                "Video service health: unknown",
                "Video service health: healthy",
            ),
            good.replace("(connected 0,", "(connected 5,"),
            good.replace('\n', "\r\n"),
            good.replace("Relay", "Rélay"),
            good.trim_end().to_string(),
            "x".repeat(MAX_BYTES + 1),
            String::new(),
            format!("{good}{good}{good}{good}{good}{good}{good}{good}{good}{good}{good}"),
        ];
        for text in &tampered {
            assert!(!is_clean(text), "{text:?}");
        }
    }

    #[test]
    fn what_was_previewed_is_exactly_what_is_saved_and_nothing_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let text = render(&facts());
        let path = dir.path().join("report.txt");
        assert_eq!(save_new(&path, &text), Outcome::Saved);
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
        // A second save to the same name must not change the first file.
        let other = render(&Facts::new(
            "0.10.0",
            "20261023000001-4f2a9c1b3d7e",
            "windows",
            "x86_64",
            "2026-10-24",
            rest(),
        ));
        assert_eq!(save_new(&path, &other), Outcome::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
        // A folder, or a place that does not exist, fails without creating anything.
        assert_eq!(save_new(dir.path(), &text), Outcome::Failed);
        let missing = dir.path().join("no-such-folder").join("report.txt");
        assert_eq!(save_new(&missing, &text), Outcome::Failed);
        assert!(!missing.exists());
        // Text that fails the vocabulary check is never written.
        let refused = dir.path().join("refused.txt");
        assert_eq!(
            save_new(&refused, &format!("{text}Token: abc\n")),
            Outcome::Refused
        );
        assert!(!refused.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_report_says_states_are_last_recorded_and_refuses_text_without_it() {
        let good = render(&facts());
        assert!(good.contains(RECORDED_NOTE));
        assert!(
            RECORDED_NOTE.contains("last recorded")
                && RECORDED_NOTE.contains("does not test the network")
        );
        assert!(is_clean(&good));
        let without = good.replace(&format!("{RECORDED_NOTE}\n"), "");
        assert_ne!(without, good);
        assert!(!is_clean(&without));
    }

    #[test]
    fn a_session_on_the_friends_service_is_friends_not_a_current_community() {
        assert_eq!(
            current_community(true, true, Link::Connected),
            CurrentCommunity::NoneOpen
        );
        assert_eq!(
            current_community(true, false, Link::Offline),
            CurrentCommunity::Link(Link::Offline)
        );
        assert_eq!(
            current_community(false, false, Link::Connected),
            CurrentCommunity::NoneOpen
        );
        assert_eq!(
            current_community(false, true, Link::Connected),
            CurrentCommunity::NoneOpen
        );
    }

    #[test]
    fn status_rows_keep_four_services_separate_and_say_only_what_is_known() {
        let rows = status_rows(&rest());
        let names: Vec<_> = rows.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            ["Relay account", "Current community", "Friends", "Video"]
        );
        // A healthy account with nothing to do has no next step and no button.
        assert_eq!(rows[0].next, None);
        assert!(rows.iter().all(|r| !r.open_account));
        // Unknown Friends stays unknown; it is neither connected nor offline.
        let unknown = status_rows(&Rest {
            friends: FriendsState::Unknown,
            ..rest()
        });
        assert_eq!(unknown[2].state, "unknown");
        assert_eq!(unknown[2].next, Some(HINT_OPEN_FRIENDS));
        // Video never claims service health, whatever the session is doing.
        for video in [
            VideoState::NotIncluded,
            VideoState::NotInUse,
            VideoState::Starting,
            VideoState::Active,
            VideoState::Reconnecting,
        ] {
            let row = &status_rows(&Rest { video, ..rest() })[3];
            let note = row.note.as_deref().unwrap();
            assert!(note.contains("health is unknown"), "{video:?}");
            for claim in ["healthy", "reachable", "working", "online"] {
                assert!(!format!("{} {}", row.state, note)
                    .to_lowercase()
                    .contains(claim));
            }
        }
        assert!(
            STATUS_NOTE.contains("Last recorded") && STATUS_NOTE.contains("not a network test")
        );
    }

    #[test]
    fn open_account_is_offered_only_where_signing_in_is_the_next_step() {
        use AccountState::*;
        for (account, problem, offered) in [
            (SignedIn, false, false),
            (SignedIn, true, true),
            (SignedOut, false, true),
            (SigningIn, false, true),
            (Expired, false, true),
            (NotSetUp, false, false),
            (NotSetUp, true, false),
        ] {
            let rows = status_rows(&Rest {
                account,
                account_problem: problem,
                ..rest()
            });
            assert_eq!(rows[0].open_account, offered, "{account:?} {problem}");
        }
        let friends = status_rows(&Rest {
            friends: FriendsState::NeedsAccount,
            ..rest()
        });
        assert!(friends[2].open_account && !friends[1].open_account && !friends[3].open_account);
        // Every state text and next step comes from the report's closed vocabulary.
        let saved = Counts::default();
        for account in AccountState::ALL {
            let rows = status_rows(&Rest {
                account,
                saved,
                ..rest()
            });
            assert!(rows[0].next.is_none_or(|n| HINTS.contains(&n)));
        }
    }

    fn drawn(ctx: &egui::Context, rows: &[StatusRow; 4], events: Vec<egui::Event>) -> (bool, Rect) {
        let mut out = (false, Rect::NOTHING);
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 1200.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let clicked = status_content(ui, rows);
                    out = (clicked, ui.min_rect());
                });
            },
        );
        out
    }

    #[test]
    fn drawing_the_status_never_acts_and_only_the_button_asks_for_the_account_page() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let rows = status_rows(&Rest {
            account: AccountState::SignedOut,
            ..rest()
        });
        for _ in 0..3 {
            assert!(
                !drawn(&ctx, &rows, Vec::new()).0,
                "rendering alone never acts"
            );
        }
        // Find the one Open Account button by clicking down the page.
        let mut hit = false;
        let mut y = 20.0;
        while y < 700.0 && !hit {
            let at = Pos2::new(40.0, y);
            let button = |pressed| egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            hit = drawn(
                &ctx,
                &rows,
                vec![egui::Event::PointerMoved(at), button(true), button(false)],
            )
            .0;
            drawn(&ctx, &rows, Vec::new());
            y += 6.0;
        }
        assert!(hit, "the Open Account button is on the page and clickable");
    }

    #[test]
    fn the_messages_shown_after_saving_say_what_happened() {
        assert!(Outcome::Saved.text().contains("did not open or send"));
        assert!(Outcome::AlreadyExists.text().contains("never replaces"));
        assert!(Outcome::Failed.text().contains("could not save"));
        assert!(Outcome::Refused.text().contains("safety check"));
        for outcome in [
            Outcome::Saved,
            Outcome::AlreadyExists,
            Outcome::Failed,
            Outcome::Refused,
        ] {
            // No outcome message can carry a path or an operating-system error.
            assert!(!outcome.text().contains(['\\', '/', ':']), "{outcome:?}");
        }
    }
}
