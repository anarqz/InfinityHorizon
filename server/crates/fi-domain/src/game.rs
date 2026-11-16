//! What the server does differently per game (CR-S97; owner, 2026-11-07: "drawn well-defined lines
//! between behaviors on how to server should act depending on the ticket being used"). Every
//! per-game decision reads [`GameRules`] for the ticket's or the profile's game; no code compares a
//! game code anywhere else. The table is GAME_PLATFORM.md "Per-game behaviour".

use crate::{PoolName, PoolRouting, ProfileMode};

/// Forza Horizon 2's game code.
pub const FH2: &str = "fh2";

/// Forza Motorsport 5's game code (OD-206).
pub const FM5: &str = "fm5";

/// Which pool a session of the game is admitted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pools {
    /// By the account's tier ([`PoolRouting`], `FI_POOL_ROUTING`).
    ByTier,
    /// One pool named after the game, for every tier.
    Own,
}

/// Which allowlist and schema answer the game's proxied requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proxy {
    /// FH2's dispatch: free roam, the Forza RPC over FH2's schema and catalog, the universe.
    Fh2,
    /// FM5's own allowlist over FM5's schema (`fi_surfaces::game::proxy::fm5`).
    Fm5,
}

/// Which free-roam session profile answers the game's `sessiondirectory`, `gameserverds` and
/// `gameserverms` requests (`fi_surfaces::game::freeroam`, FM5_LOBBIES.md §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    /// FH2's: contract 105, its GSI set, 12 members, SmartMatch, car meets and Road Trip.
    Fh2,
    /// FM5's: contract 104, its GSI set, 16 members, private races only (CR-S99).
    Fm5,
}

/// The most seats any game's world has (the largest [`GameRules::max_players`]).
pub const MAX_SEATS: u32 = 16;

/// One game's server behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameRules {
    /// The game code.
    pub code: &'static str,
    /// Where its sessions are admitted.
    pub pools: Pools,
    /// Whether a Hardcore profile of it keeps a cloud save line (lease, revisions, lapses,
    /// `cloudLine`; OD-239: a Noob/Azimuth profile never does).
    pub save_line: bool,
    /// Whether its sessions take the content scan (CONTENT_API.md).
    pub content_scan: bool,
    /// The modes a new profile may take.
    pub modes: &'static [ProfileMode],
    /// Whether it is closed unless a pool of it accepts sessions: no profile is created and players
    /// are not shown it (`/launcher/v1/info`, `/core/v1/status`).
    pub open_with_pool: bool,
    /// The proxied requests' table.
    pub proxy: Proxy,
    /// An official world's `activity` (CR-S99, FM5_LOBBIES.md §1); a subset of
    /// `fi_protocol::matchmaking::OFFICIAL_ACTIVITY_KINDS`.
    pub official_activities: &'static [&'static str],
    /// The activities a private create (`POST /mm/v1/join {private: true}`) may name.
    pub private_activities: &'static [&'static str],
    /// The seats of one world (`maxPlayers` ceiling, the `world` checks).
    pub max_players: u32,
    /// Whether its official Worlds keep pool worlds for cheat-flagged sessions (CR-S103,
    /// WORLDS_OFFICIAL.md §14): FH2 only; a flagged session of another game has no lobby while
    /// its flag lasts.
    pub pool_worlds: bool,
    /// The free-roam session profile.
    pub session: Session,
}

/// FH2: everything the platform was built for.
pub const FH2_RULES: GameRules = GameRules {
    code: FH2,
    pools: Pools::ByTier,
    save_line: true,
    content_scan: true,
    modes: &[ProfileMode::Hardcore, ProfileMode::Noob],
    open_with_pool: false,
    proxy: Proxy::Fh2,
    official_activities: &["freeroam", "carmeet", "roadtrip"],
    private_activities: &["freeroam", "roadtrip"],
    max_players: 12,
    pool_worlds: true,
    session: Session::Fh2,
};

/// FM5 for now (OD-206 working shape for T1 and T2): Noob only, no content scan, one pool, its own
/// proxy table. OD-239 supersedes OD-219's Noob save line: FM5 has no Hardcore yet, so no line;
/// existing `FM5SAVE1` lines stay readable.
pub const FM5_RULES: GameRules = GameRules {
    code: FM5,
    pools: Pools::Own,
    save_line: false,
    content_scan: false,
    modes: &[ProfileMode::Noob],
    open_with_pool: true,
    proxy: Proxy::Fm5,
    official_activities: &["race"],
    private_activities: &["race"],
    max_players: 16,
    pool_worlds: false,
    session: Session::Fm5,
};

/// The games a player can create a profile of.
pub const GAMES: [GameRules; 2] = [FH2_RULES, FM5_RULES];

impl GameRules {
    /// The rules of a game a player may name, `None` for any other code.
    pub fn known(code: &str) -> Option<&'static GameRules> {
        GAMES.iter().find(|g| g.code == code)
    }

    /// The rules of a stored game: a known game's, FH2's for any other (the test titles of the
    /// suites behave as FH2 did before this table).
    pub fn of(code: &str) -> &'static GameRules {
        Self::known(code).unwrap_or(&FH2_RULES)
    }

    /// The codes of the games that are closed without an accepting pool.
    pub fn open_with_pool_codes() -> Vec<&'static str> {
        GAMES
            .iter()
            .filter(|g| g.open_with_pool)
            .map(|g| g.code)
            .collect()
    }

    /// The pool a session of this game is admitted to, for an account of `vip` tier.
    pub fn pool(&self, routing: &PoolRouting, vip: bool) -> PoolName {
        match self.pools {
            Pools::Own => PoolName::new(self.code).expect("a game code is a pool name"),
            Pools::ByTier => if vip { &routing.vip } else { &routing.free }.clone(),
        }
    }

    /// Whether an official world of this game may run `activity`.
    pub fn official_activity(&self, activity: &str) -> bool {
        self.official_activities.contains(&activity)
    }

    /// Whether a private create of this game may name `activity`.
    pub fn private_activity(&self, activity: &str) -> bool {
        self.private_activities.contains(&activity)
    }

    /// Whether a new profile may take `mode`.
    pub fn allows(&self, mode: ProfileMode) -> bool {
        self.modes.contains(&mode)
    }

    /// Whether a profile of `mode` (its stored text) keeps this game's save line (OD-239, a hard
    /// rule, any tier): the game has a line and the mode is `hardcore`. A Noob (Azimuth) profile,
    /// an unknown mode and an FM5 profile (CR-S105) keep none.
    pub fn line_mode(&self, mode: &str) -> bool {
        self.save_line
            && self.allows(ProfileMode::Hardcore)
            && mode.parse::<ProfileMode>().ok() == Some(ProfileMode::Hardcore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fh2_keeps_every_behaviour_and_unknown_codes_follow_it() {
        let r = PoolRouting::mvp();
        let fh2 = GameRules::of(FH2);
        assert_eq!(fh2.pool(&r, true), r.vip);
        assert_eq!(fh2.pool(&r, false), r.free);
        assert!(fh2.save_line && fh2.content_scan && !fh2.open_with_pool);
        assert!(fh2.allows(ProfileMode::Hardcore) && fh2.allows(ProfileMode::Noob));
        assert_eq!(fh2.proxy, Proxy::Fh2);
        assert_eq!(GameRules::of("t0123456789"), &FH2_RULES);
        assert!(fh2.official_activity("carmeet") && !fh2.official_activity("race"));
        assert!(fh2.private_activity("roadtrip") && !fh2.private_activity("carmeet"));
        assert_eq!((fh2.max_players, fh2.session), (12, Session::Fh2));
        assert!(GameRules::known("t0123456789").is_none());
    }

    #[test]
    fn fm5_has_its_own_pool_and_no_save_line() {
        let r = PoolRouting::mvp();
        let fm5 = GameRules::of(FM5);
        for vip in [true, false] {
            assert_eq!(fm5.pool(&r, vip).as_str(), FM5);
        }
        assert!(!fm5.save_line && !fm5.content_scan && fm5.open_with_pool);
        // OD-239: Hardcore only; FM5 has no Hardcore, so no line at all.
        assert!(!fm5.line_mode("noob") && !fm5.line_mode("hardcore"));
        let fh2 = GameRules::of(FH2);
        assert!(!fh2.line_mode("noob") && fh2.line_mode("hardcore") && !fh2.line_mode("x"));
        assert!(fm5.allows(ProfileMode::Noob) && !fm5.allows(ProfileMode::Hardcore));
        assert_eq!(fm5.proxy, Proxy::Fm5);
        assert_eq!(GameRules::open_with_pool_codes(), [FM5]);
        assert!(fm5.official_activity("race") && !fm5.official_activity("freeroam"));
        assert!(fm5.private_activity("race") && !fm5.private_activity("roadtrip"));
        assert_eq!((fm5.max_players, fm5.session), (16, Session::Fm5));
        assert_eq!(GAMES.iter().map(|g| g.max_players).max(), Some(MAX_SEATS));
    }
}
