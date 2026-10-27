//! "Keep a takeover copy on this computer": the co-host's side of setting up
//! takeover. Like "Keep a copy on this PC", the work runs in the installed
//! companion (`relay-server --takeover-copy DIR` and `--takeover-follow DIR`),
//! hidden: one JSON object on its standard input, which is then closed, and one
//! JSON line back. The app keeps the route to the host alive and passes the
//! resolved address; the session token travels only on that pipe.
use relay_protocol::handover::RoutePin;
use serde::Deserialize;
use serde_json::json;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: u64 = 4096;
/// A copy can be large; the companion gets this long for one run.
const COPY_DEADLINE: Duration = Duration::from_secs(30 * 60);
const MISSING: &str = "The Relay companion is missing or stopped; reinstall Relay or try again";

/// Where this computer keeps its takeover key and copy for one community.
pub(crate) fn folder(server_id: &str) -> Option<PathBuf> {
    if uuid::Uuid::parse_str(server_id).map(|v| v.to_string()).ok().as_deref() != Some(server_id) {
        return None;
    }
    let base = std::env::var_os("RELAY_PROFILE_DIR")
        .or_else(|| std::env::var_os("RELAY_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| directories::ProjectDirs::from("org", "Relay", "Relay").map(|v| v.config_dir().to_owned()))?;
    Some(base.join("takeover-copies").join(server_id))
}

/// True when this computer follows the host live (protection is on).
pub(crate) fn following(dir: &Path) -> bool {
    dir.join("feed.key").is_file()
}

pub(crate) struct Host {
    pub url: String,
    pub token: String,
    pub server_id: String,
    pub user_id: String,
    pub pin: RoutePin,
}

/// Where one run ended, as the companion reports it.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "outcome", content = "reason", rename_all = "snake_case")]
pub(crate) enum Outcome {
    WaitingForSecondCoHost,
    UpToDate,
    Following,
    RoleEnded,
    Stopped(String),
}

impl Outcome {
    pub(crate) fn text(&self) -> String {
        match self {
            Outcome::WaitingForSecondCoHost => "Joined. Waiting for a second approved co-host before the host signs the takeover agreement.".into(),
            Outcome::UpToDate => "Data copy up to date: a verified full copy is kept on this computer. Ready to take over stays off until live replication exists.".into(),
            Outcome::Following => "Following live changes: every change on the host is confirmed by this computer or the other co-host before it is accepted.".into(),
            Outcome::RoleEnded => "Your co-host role ended, so the copy on this computer was removed.".into(),
            Outcome::Stopped(why) => format!("Stopped: {why}"),
        }
    }
}

/// The companion's one line, or a plain stop when it said nothing usable.
pub(crate) fn parse(line: &[u8]) -> Outcome {
    serde_json::from_slice(line.trim_ascii()).unwrap_or_else(|_| Outcome::Stopped(MISSING.into()))
}

fn spawn(mode: &str, dir: &Path, input: &[u8]) -> Option<Child> {
    let exe = crate::local_host::server_binary()?;
    std::fs::create_dir_all(dir).ok()?;
    let mut command = Command::new(exe);
    command
        .arg(mode)
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    Some(child)
}

/// The host's address as this app reaches it right now (LAN, iroh or HTTPS).
fn resolved(url: &str) -> Option<String> {
    crate::embedded_network::ensure_route(url).ok()?;
    Some(crate::embedded_network::endpoint(url))
}

/// One complete attempt in the companion: enroll, consent, copy, install, report.
pub(crate) fn run(host: Host, dir: PathBuf) -> Outcome {
    let Some(url) = resolved(&host.url) else {
        return Outcome::Stopped("The host could not be reached; the copy continues next time".into());
    };
    let input = json!({
        "url": url,
        "token": host.token,
        "server_id": host.server_id,
        "user_id": host.user_id,
        "pin": host.pin,
    })
    .to_string();
    let Some(mut child) = spawn("--takeover-copy", &dir, input.as_bytes()) else {
        return Outcome::Stopped(MISSING.into());
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Outcome::Stopped(MISSING.into());
    };
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes);
        let _ = send.send(bytes);
    });
    let started = Instant::now();
    loop {
        if let Ok(bytes) = receive.try_recv() {
            let _ = child.wait();
            return parse(&bytes);
        }
        if started.elapsed() > COPY_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::Stopped("The copy took too long; it resumes next time".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// While protection is on, keeps the companion following live changes until
/// `stop`. A changed route or an ended companion is restarted.
pub(crate) fn follow(url: String, dir: PathBuf, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        let Some(address) = resolved(&url) else {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        };
        let input = json!({ "url": address }).to_string();
        let Some(mut child) = spawn("--takeover-follow", &dir, input.as_bytes()) else {
            std::thread::sleep(Duration::from_secs(10));
            continue;
        };
        let mut checked = Instant::now();
        loop {
            if stop.load(Ordering::Acquire) {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            if checked.elapsed() > Duration::from_secs(5) {
                checked = Instant::now();
                if resolved(&url).as_deref() != Some(address.as_str()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
            if !matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_secs(2));
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_companion_line_is_read_exactly_and_anything_else_is_a_stop() {
        assert_eq!(parse(b"{\"outcome\":\"following\"}\n"), Outcome::Following);
        assert_eq!(parse(b"{\"outcome\":\"up_to_date\"}"), Outcome::UpToDate);
        assert_eq!(
            parse(b"{\"outcome\":\"stopped\",\"reason\":\"why\"}"),
            Outcome::Stopped("why".into())
        );
        assert!(matches!(parse(b""), Outcome::Stopped(_)));
        assert!(matches!(parse(b"not json"), Outcome::Stopped(_)));
    }

    #[test]
    fn only_a_canonical_community_id_names_a_folder() {
        assert!(folder("not-a-uuid").is_none());
        let id = uuid::Uuid::new_v4().to_string();
        assert!(folder(&id).is_some_and(|p| p.ends_with(&id)));
        assert!(folder(&id.to_uppercase()).is_none());
    }
}
