//! Opt-in game activity for Friends.
//!
//! A person can choose to tell their accepted friends which recognized game
//! they are playing. What leaves the PC is one catalog id (and, only if the
//! person pastes one, a validated join link for that same game). The catalog is
//! small and fixed: names come from it, never from the PC, so a process name,
//! window title, path or command line can never be published.
use serde::{Deserialize, Serialize};

/// Advertised only by the Friends service.
pub const CAPABILITY: &str = "game-activity-v1";
/// How long one announcement stays valid. The sender repeats it more often
/// than this while the game runs, so a stopped game disappears by itself.
pub const TTL_SECS: u64 = 120;
pub const HEARTBEAT_SECS: u64 = 45;

pub struct Game {
    /// Stable public id, the only game identifier that is ever sent.
    pub id: &'static str,
    pub name: &'static str,
    /// Exact executable file names (case-insensitive, no path).
    pub processes: &'static [&'static str],
    /// Steam application id, which decides whether a Steam join link may be
    /// attached to this game.
    pub steam_app: Option<u32>,
}

const fn game(
    id: &'static str,
    name: &'static str,
    processes: &'static [&'static str],
    steam_app: Option<u32>,
) -> Game {
    Game {
        id,
        name,
        processes,
        steam_app,
    }
}

/// A small curated list. A name that many programs share (a generic Java
/// runtime, for example) is deliberately not here.
pub const CATALOG: &[Game] = &[
    game(
        "counter-strike-2",
        "Counter-Strike 2",
        &["cs2.exe"],
        Some(730),
    ),
    game("dota-2", "Dota 2", &["dota2.exe"], Some(570)),
    game(
        "team-fortress-2",
        "Team Fortress 2",
        &["tf_win64.exe"],
        Some(440),
    ),
    game(
        "left-4-dead-2",
        "Left 4 Dead 2",
        &["left4dead2.exe"],
        Some(550),
    ),
    game(
        "rocket-league",
        "Rocket League",
        &["RocketLeague.exe"],
        Some(252950),
    ),
    game(
        "apex-legends",
        "Apex Legends",
        &["r5apex.exe", "r5apex_dx12.exe"],
        Some(1172470),
    ),
    game("terraria", "Terraria", &["Terraria.exe"], Some(105600)),
    game("rust", "Rust", &["RustClient.exe"], Some(252490)),
    game(
        "helldivers-2",
        "Helldivers 2",
        &["helldivers2.exe"],
        Some(553850),
    ),
    game(
        "elden-ring",
        "Elden Ring",
        &["eldenring.exe"],
        Some(1245620),
    ),
    game(
        "baldurs-gate-3",
        "Baldur's Gate 3",
        &["bg3.exe", "bg3_dx11.exe"],
        Some(1086940),
    ),
    game(
        "lethal-company",
        "Lethal Company",
        &["Lethal Company.exe"],
        Some(1966720),
    ),
    game(
        "palworld",
        "Palworld",
        &["Palworld-Win64-Shipping.exe"],
        Some(1623730),
    ),
    game(
        "deep-rock-galactic",
        "Deep Rock Galactic",
        &["FSD-Win64-Shipping.exe"],
        Some(548430),
    ),
    game("among-us", "Among Us", &["Among Us.exe"], Some(945360)),
    game(
        "minecraft-bedrock",
        "Minecraft (Bedrock)",
        &["Minecraft.Windows.exe"],
        None,
    ),
    game(
        "fortnite",
        "Fortnite",
        &["FortniteClient-Win64-Shipping.exe"],
        None,
    ),
    game(
        "valorant",
        "VALORANT",
        &["VALORANT-Win64-Shipping.exe"],
        None,
    ),
    game(
        "league-of-legends",
        "League of Legends",
        &["League of Legends.exe"],
        None,
    ),
    game("gta-v", "Grand Theft Auto V", &["GTA5.exe"], None),
];

pub fn find(id: &str) -> Option<&'static Game> {
    CATALOG.iter().find(|game| game.id == id)
}

/// The catalog games among `names`, in catalog order and without repeats.
/// Only exact executable file names match; anything with a path separator is
/// ignored, and nothing that is not a catalog game is kept.
pub fn recognize<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'static Game> {
    let mut found = vec![false; CATALOG.len()];
    for name in names {
        if name.is_empty() || name.len() > 260 || name.contains(['/', '\\', ':']) {
            continue;
        }
        for (index, game) in CATALOG.iter().enumerate() {
            if game
                .processes
                .iter()
                .any(|process| process.eq_ignore_ascii_case(name))
            {
                found[index] = true;
            }
        }
    }
    CATALOG
        .iter()
        .zip(found)
        .filter_map(|(game, present)| present.then_some(game))
        .collect()
}

/// A plain decimal number in 1..=u64::MAX: digits only, no sign, no overflow,
/// and not zero (a zero lobby or user is not a real Steam identifier).
fn id64(part: &str) -> Option<u64> {
    if part.is_empty() || part.len() > 20 || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse::<u64>().ok().filter(|value| *value != 0)
}

/// The one join-link form accepted: `steam://joinlobby/<app>/<lobby>/<user>`,
/// with `<app>` the Steam application id of this very game and `<lobby>` and
/// `<user>` nonzero 64-bit decimal identifiers. Returns the canonical decimal
/// link (no leading zeros), or None for anything else (other schemes, other
/// Steam actions, extra path or query, other games' ids, values that are zero
/// or do not fit 64 bits, whitespace inside). The result is plain text for a
/// human to confirm, never run by Relay.
pub fn join_link(game: &Game, link: &str) -> Option<String> {
    let app = game.steam_app?;
    let link = link.trim();
    let rest = link.strip_prefix("steam://joinlobby/")?;
    let mut parts = rest.split('/');
    let (given, lobby, user) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || given.len() > 10 || id64(given)? != u64::from(app) {
        return None;
    }
    let (lobby, user) = (id64(lobby)?, id64(user)?);
    Some(format!("steam://joinlobby/{app}/{lobby}/{user}"))
}

/// What a friend sees. `name` always comes from the catalog.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GameActivity {
    pub game_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<String>,
    pub expires_at: u64,
}

/// What a person sends to share, and nothing else.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    pub game_id: String,
    #[serde(default)]
    pub join: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_catalog_executables_are_recognized_from_a_generated_sample() {
        let sample = [
            "explorer.exe",
            "CS2.EXE",
            "notepad.exe",
            "javaw.exe",
            "Terraria.exe",
            "C:\\Games\\Terraria\\Terraria.exe",
            "cs2.exe.bak",
            "xcs2.exe",
            "",
        ];
        let ids: Vec<_> = recognize(sample).iter().map(|g| g.id).collect();
        assert_eq!(ids, ["counter-strike-2", "terraria"]);
        // Repeats and order do not matter: one entry per game, catalog order.
        let twice: Vec<_> = recognize(["terraria.exe", "cs2.exe", "terraria.exe"])
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(twice, ["counter-strike-2", "terraria"]);
        assert!(recognize(["unknown-game.exe", "javaw.exe"]).is_empty());
        assert!(recognize(std::iter::empty()).is_empty());
    }

    #[test]
    fn the_catalog_is_unambiguous_and_bounded() {
        let mut seen = std::collections::HashSet::new();
        for game in CATALOG {
            assert!(game
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()));
            assert!(game.name.len() <= 40 && !game.name.chars().any(char::is_control));
            assert!(!game.processes.is_empty());
            for process in game.processes {
                assert!(process.ends_with(".exe") && !process.contains(['/', '\\']));
                assert!(seen.insert(process.to_ascii_lowercase()), "{process}");
            }
        }
        assert!(CATALOG.len() <= 32);
        assert_eq!(find("terraria").map(|g| g.name), Some("Terraria"));
        assert!(find("../terraria").is_none());
    }

    #[test]
    fn a_join_link_is_one_explicit_steam_lobby_form_for_the_same_game() {
        let cs = find("counter-strike-2").unwrap();
        let good = "steam://joinlobby/730/109775241234567890/76561198000000000";
        assert_eq!(join_link(cs, good).as_deref(), Some(good));
        assert_eq!(join_link(cs, &format!("  {good}\n")).as_deref(), Some(good));
        for bad in [
            "",
            "steam://joinlobby/570/1/2",
            "steam://joinlobby/730/1",
            "steam://joinlobby/730/1/2/3",
            "steam://joinlobby/730/1/2?x=1",
            "steam://joinlobby/730/1/2#frag",
            "steam://joinlobby/730/a/2",
            "steam://joinlobby/0730/1/2x",
            "steam://run/730",
            "steam://connect/1.2.3.4:27015",
            "steam://rungameid/730//-console",
            "STEAM://joinlobby/730/1/2",
            "https://example.com/join",
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "cmd.exe /c calc",
            "steam://joinlobby/730/1/2 steam://run/1",
            "steam://joinlobby/99999999999/1/2",
        ] {
            assert!(join_link(cs, bad).is_none(), "{bad}");
        }
        // A game without a Steam id cannot carry a link at all.
        assert!(join_link(find("fortnite").unwrap(), good).is_none());
        // Digits only, bounded: no overlong lobby or user number.
        assert!(join_link(cs, &format!("steam://joinlobby/730/{}/2", "1".repeat(21))).is_none());
        assert!(join_link(cs, &format!("steam://joinlobby/730/1/{}", "1".repeat(21))).is_none());
        // Identifiers are nonzero 64-bit numbers: zero and anything above u64::MAX fail.
        let max = u64::MAX;
        assert_eq!(
            join_link(cs, &format!("steam://joinlobby/730/{max}/{max}")).as_deref(),
            Some(format!("steam://joinlobby/730/{max}/{max}").as_str())
        );
        for bad in [
            "steam://joinlobby/730/0/2",
            "steam://joinlobby/730/1/0",
            "steam://joinlobby/730/000/2",
            "steam://joinlobby/730/18446744073709551616/2",
            "steam://joinlobby/730/1/18446744073709551616",
            "steam://joinlobby/730/99999999999999999999/2",
            "steam://joinlobby/730/1/-5",
            "steam://joinlobby/730/+1/2",
            "steam://joinlobby/4294967296/1/2",
            "steam://joinlobby/0/1/2",
        ] {
            assert!(join_link(cs, bad).is_none(), "{bad}");
        }
        // Leading zeros are normalised, so the stored link is always canonical.
        assert_eq!(
            join_link(cs, "steam://joinlobby/0730/0001/0002").as_deref(),
            Some("steam://joinlobby/730/1/2")
        );
    }

    #[test]
    fn a_share_carries_only_a_game_and_a_link() {
        let ok: Share = serde_json::from_str(r#"{"game_id":"terraria"}"#).unwrap();
        assert!(ok.join.is_none());
        for extra in [
            r#"{"game_id":"terraria","name":"Anything"}"#,
            r#"{"game_id":"terraria","process":"x.exe"}"#,
            r#"{"game_id":"terraria","window_title":"x"}"#,
        ] {
            assert!(serde_json::from_str::<Share>(extra).is_err(), "{extra}");
        }
    }
}
