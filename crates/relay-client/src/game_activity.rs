//! Opt-in game activity: what to announce, when, and nothing else.
//!
//! Off unless the person turns it on. While on, Relay compares the executable
//! file names of running programs with a short built-in list of games, keeps
//! only the catalog ids that match, and announces the one the person chose to
//! their friends. No process list, path, window title or command line is kept
//! or sent. The source of names is injectable so this logic is exercised with
//! generated samples only.
use relay_protocol::game_activity::{self as model, HEARTBEAT_SECS};
use serde::{Deserialize, Serialize};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

/// How often the running programs are compared with the catalog.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(15);

/// What is remembered between runs: only the two choices, both off by default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    /// Also show a Join button to friends, using a link the person pasted.
    #[serde(default)]
    pub share_join: bool,
}

/// A source of executable file names. The real one reads this PC's running
/// programs; tests give it a generated list.
pub type Names = fn() -> Vec<String>;

struct Published {
    game_id: String,
    join: Option<String>,
    at: Instant,
}

#[derive(Default)]
pub struct State {
    /// Catalog ids found at the last sample, and nothing else about it.
    pub running: Vec<&'static str>,
    /// The game the person is sharing; always one of `running`.
    pub selected: Option<String>,
    /// A join link the person pasted. Session only, never saved.
    pub join_text: String,
    sampled_at: Option<Instant>,
    sampling: Option<mpsc::Receiver<Vec<&'static str>>>,
    published: Option<Published>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    Publish {
        game_id: String,
        join: Option<String>,
    },
    Withdraw,
}

/// Compares names with the catalog and drops everything else at once.
pub fn running_games(names: Vec<String>) -> Vec<&'static str> {
    model::recognize(names.iter().map(String::as_str))
        .into_iter()
        .map(|game| game.id)
        .collect()
}

impl State {
    /// Nothing is ever sampled while the feature is off.
    pub fn should_sample(&self, settings: &Settings, now: Instant) -> bool {
        settings.enabled
            && self.sampling.is_none()
            && self
                .sampled_at
                .is_none_or(|at| now.saturating_duration_since(at) >= SAMPLE_EVERY)
    }

    /// Looks at the running programs off the UI thread.
    pub fn start_sample(&mut self, names: Names, now: Instant) {
        let (sender, receiver) = mpsc::channel();
        self.sampled_at = Some(now);
        self.sampling = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(running_games(names()));
        });
    }

    /// Takes a finished sample, if there is one.
    pub fn collect(&mut self) {
        let Some(receiver) = &self.sampling else {
            return;
        };
        match receiver.try_recv() {
            Ok(found) => {
                self.sampling = None;
                self.accept(found);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.sampling = None,
        }
    }

    /// The result of a sample: what runs now, and which of it is shared.
    pub fn accept(&mut self, found: Vec<&'static str>) {
        self.running = found;
        // A game that stopped is no longer chosen; a lone game is chosen for the
        // person, who has already turned sharing on.
        if self
            .selected
            .as_deref()
            .is_some_and(|id| !self.running.contains(&id))
        {
            self.selected = None;
        }
        if self.selected.is_none() && self.running.len() == 1 {
            self.selected = Some(self.running[0].into());
        }
    }

    /// Stop sampling immediately without forgetting a withdrawal still owed to
    /// the service. A late worker result has no receiver and is discarded.
    pub fn stop_sampling(&mut self) {
        self.sampling = None;
        self.sampled_at = None;
        self.running.clear();
        self.selected = None;
        self.join_text.clear();
    }

    /// Back to the beginning: nothing sampled, nothing remembered.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// What to tell the service now.
    pub fn decide(
        &self,
        settings: &Settings,
        connected: bool,
        offered: bool,
        now: Instant,
    ) -> Action {
        if !connected || !offered {
            // Nothing can be sent; the service forgets it shortly anyway.
            return Action::Nothing;
        }
        let game = settings
            .enabled
            .then_some(self.selected.as_deref())
            .flatten()
            .filter(|id| self.running.contains(id))
            .and_then(model::find);
        let Some(game) = game else {
            return if self.published.is_some() {
                Action::Withdraw
            } else {
                Action::Nothing
            };
        };
        let join = settings
            .share_join
            .then(|| model::join_link(game, &self.join_text))
            .flatten();
        match &self.published {
            Some(sent)
                if sent.game_id == game.id
                    && sent.join == join
                    && now.saturating_duration_since(sent.at)
                        < Duration::from_secs(HEARTBEAT_SECS) =>
            {
                Action::Nothing
            }
            _ => Action::Publish {
                game_id: game.id.into(),
                join,
            },
        }
    }

    /// Commit only an acknowledged announcement/withdrawal. In-flight requests
    /// and failures never pretend that the service changed its state.
    pub fn sent(&mut self, action: &Action, now: Instant) {
        match action {
            Action::Publish { game_id, join } => {
                self.published = Some(Published {
                    game_id: game_id.clone(),
                    join: join.clone(),
                    at: now,
                })
            }
            Action::Withdraw => self.published = None,
            Action::Nothing => {}
        }
    }

    /// The service connection ended: the announcement lapses there by itself.
    pub fn disconnected(&mut self) {
        self.published = None;
    }

    pub fn shared_game(&self, now: Instant) -> Option<&str> {
        self.published
            .as_ref()
            .filter(|p| now.saturating_duration_since(p.at) < Duration::from_secs(model::TTL_SECS))
            .map(|p| p.game_id.as_str())
    }

    pub fn is_sharing(&self) -> bool {
        self.published.is_some()
    }
}

/// The executable file names of the programs running now, nothing else. Paths
/// are cut to the file name immediately and the list is dropped by the caller
/// right after it is compared with the catalog.
#[cfg(windows)]
pub fn running_names() -> Vec<String> {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::{
            ProcessStatus::EnumProcesses,
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };
    let mut ids = vec![0u32; 4096];
    let mut bytes = 0u32;
    let listed = unsafe {
        EnumProcesses(
            ids.as_mut_ptr(),
            (ids.len() * std::mem::size_of::<u32>()) as u32,
            &mut bytes,
        )
    };
    if listed.is_err() {
        return Vec::new();
    }
    let count = (bytes as usize / std::mem::size_of::<u32>()).min(ids.len());
    let mut names = Vec::new();
    for &pid in &ids[..count] {
        if pid == 0 {
            continue;
        }
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            continue;
        };
        let mut buffer = [0u16; 520];
        let mut length = buffer.len() as u32;
        let read = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        };
        let _ = unsafe { CloseHandle(process) };
        if read.is_ok() {
            let path = String::from_utf16_lossy(&buffer[..length as usize]);
            if let Some(file) = path.rsplit(['\\', '/']).next() {
                names.push(file.to_owned());
            }
        }
    }
    names
}

#[cfg(not(windows))]
pub fn running_names() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Settings {
        Settings {
            enabled: true,
            share_join: false,
        }
    }
    fn running(ids: &[&'static str]) -> State {
        let mut state = State::default();
        state.accept(ids.to_vec());
        state
    }

    #[test]
    fn it_is_off_by_default_and_then_never_samples_or_announces() {
        let now = Instant::now();
        let settings = Settings::default();
        assert!(!settings.enabled && !settings.share_join);
        let mut state = State::default();
        state.accept(vec!["terraria"]);
        assert!(!state.should_sample(&settings, now));
        assert_eq!(state.decide(&settings, true, true, now), Action::Nothing);
        // An old preferences file that never heard of it stays off.
        let saved: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(saved, settings);
    }

    #[test]
    fn a_sample_keeps_only_catalog_ids_from_generated_process_names() {
        let names = vec![
            "explorer.exe".to_string(),
            "Terraria.exe".to_string(),
            "slack.exe".to_string(),
            "C:\\Users\\someone\\Documents\\secret.exe".to_string(),
        ];
        assert_eq!(running_games(names), ["terraria"]);
        let mut state = State::default();
        let now = Instant::now();
        state.start_sample(|| vec!["cs2.exe".into(), "mail.exe".into()], now);
        let end = Instant::now() + Duration::from_secs(5);
        while state.sampling.is_some() && Instant::now() < end {
            state.collect();
            std::thread::sleep(Duration::from_millis(5));
        }
        // Only the catalog id survives; the other names are gone.
        assert_eq!(state.running, ["counter-strike-2"]);
        assert_eq!(state.selected.as_deref(), Some("counter-strike-2"));
        // Samples are spaced out, and only one runs at a time.
        assert!(!state.should_sample(&on(), now + Duration::from_secs(1)));
        assert!(state.should_sample(&on(), now + SAMPLE_EVERY));
    }

    #[test]
    fn the_chosen_game_is_announced_renewed_changed_and_withdrawn() {
        let t0 = Instant::now();
        let settings = on();
        let mut state = running(&["terraria"]);
        let publish = Action::Publish {
            game_id: "terraria".into(),
            join: None,
        };
        assert_eq!(state.decide(&settings, true, true, t0), publish);
        state.sent(&publish, t0);
        // Quiet until the heartbeat is due, then renewed.
        assert_eq!(
            state.decide(&settings, true, true, t0 + Duration::from_secs(10)),
            Action::Nothing
        );
        assert_eq!(
            state.decide(
                &settings,
                true,
                true,
                t0 + Duration::from_secs(HEARTBEAT_SECS)
            ),
            publish
        );
        // Two games running: nothing is chosen for the person, then they pick one.
        let mut two = running(&["terraria", "counter-strike-2"]);
        assert!(two.selected.is_none());
        assert_eq!(two.decide(&settings, true, true, t0), Action::Nothing);
        two.selected = Some("counter-strike-2".into());
        assert!(matches!(
            two.decide(&settings, true, true, t0),
            Action::Publish { ref game_id, .. } if game_id == "counter-strike-2"
        ));
        // The game stops: the announcement is withdrawn, and a new run is not reused.
        state.accept(vec![]);
        assert!(state.selected.is_none());
        assert_eq!(state.decide(&settings, true, true, t0), Action::Withdraw);
        state.sent(&Action::Withdraw, t0);
        assert_eq!(state.decide(&settings, true, true, t0), Action::Nothing);
        assert!(!state.is_sharing());
    }

    #[test]
    fn turning_it_off_or_losing_the_service_stops_it() {
        let now = Instant::now();
        let mut state = running(&["terraria"]);
        let publish = state.decide(&on(), true, true, now);
        state.sent(&publish, now);
        assert!(state.is_sharing());
        // Switched off while connected: withdrawn immediately.
        assert_eq!(
            state.decide(&Settings::default(), true, true, now),
            Action::Withdraw
        );
        // An older Friends service, or no Friends connection, gets nothing at all.
        assert_eq!(state.decide(&on(), true, false, now), Action::Nothing);
        assert_eq!(state.decide(&on(), false, true, now), Action::Nothing);
        state.disconnected();
        assert!(!state.is_sharing());
        assert_eq!(
            state.decide(&Settings::default(), true, true, now),
            Action::Nothing
        );
        state.clear();
        assert!(state.running.is_empty() && state.selected.is_none());
    }

    #[test]
    fn a_join_link_is_shared_only_when_allowed_valid_and_for_this_game() {
        let now = Instant::now();
        let good = "steam://joinlobby/730/109775241234567890/76561198000000000";
        let mut state = running(&["counter-strike-2"]);
        state.join_text = good.into();
        // Not allowed: no link, even though one is pasted.
        assert_eq!(
            state.decide(&on(), true, true, now),
            Action::Publish {
                game_id: "counter-strike-2".into(),
                join: None
            }
        );
        let with_link = Settings {
            enabled: true,
            share_join: true,
        };
        assert_eq!(
            state.decide(&with_link, true, true, now),
            Action::Publish {
                game_id: "counter-strike-2".into(),
                join: Some(good.into())
            }
        );
        // A link for another game, or something else entirely, is never sent.
        for bad in [
            "steam://joinlobby/570/1/2",
            "steam://run/730",
            "https://example.com",
            "",
        ] {
            state.join_text = bad.into();
            assert!(matches!(
                state.decide(&with_link, true, true, now),
                Action::Publish { join: None, .. }
            ));
        }
        // Changing the link republishes at once, even before the heartbeat.
        state.join_text = good.into();
        let first = state.decide(&with_link, true, true, now);
        state.sent(&first, now);
        state.join_text = "steam://joinlobby/730/9/9".into();
        assert!(matches!(
            state.decide(&with_link, true, true, now),
            Action::Publish { join: Some(_), .. }
        ));
    }
}
