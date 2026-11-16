//! The game data a process checks at start (T-1115, OD-247 Q3): the directory `FI_GAME_DATA_DIR`
//! names ([`fi_gamedata`]), every file of its manifest against its SHA-256, and every table the
//! server reads from it parsed once. A process calls [`load`] before it serves or runs a job, so a
//! missing setting, file or table stops it with the file's name instead of failing a request later.

use std::path::Path;

use fi_gamedata::Error;

/// What [`load`] checked.
#[derive(Clone, Copy, Debug)]
pub struct Loaded {
    /// The directory.
    pub root: &'static Path,
    /// The files of its manifest.
    pub files: usize,
}

fn table<T: ?Sized>(loaded: Result<&T, &Error>) -> Result<(), Error> {
    loaded.map(|_| ()).map_err(Clone::clone)
}

/// The games whose files [`load`] requires: `FI_GAMES` (comma list, default `fh2`, the same
/// default as every service's configuration). The FH2 tables are always required (the game plane,
/// saves and jobs read them); FM5's web-service schema only when `fm5` is served.
pub fn games_from(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("fh2")
        .split(',')
        .map(|g| g.trim().to_owned())
        .filter(|g| !g.is_empty())
        .collect()
}

/// [`load_games`] for the process's `FI_GAMES`.
pub fn load() -> Result<Loaded, Error> {
    let games = games_from(std::env::var("FI_GAMES").ok().as_deref());
    load_games(&games)
}

/// Opens the process's game data directory, checks every file of its manifest and parses every
/// table: the web-service schemas of FH2 and (when `games` holds `fm5`) FM5, the FH2 catalog (with its car packs), parts
/// prices, leaderboards, Statistics screen, stats boards, card ids, top speeds, PI bands, economy
/// numbers, car display names, blocked cars and stats ceilings, the save's pinned DDL, the Storefront keywords, the economy settings and the guard's limits.
pub fn load_games(games: &[String]) -> Result<Loaded, Error> {
    let dir = fi_gamedata::dir()?;
    let files = dir.verify()?;
    table(fi_xtsw::Schema::try_fh2())?;
    if games.iter().any(|g| g == "fm5") {
        table(fi_xtsw::Schema::try_fm5())?;
    }
    table(fi_gamedb::Catalog::try_fh2())?;
    table(fi_gamedb::parts::PartPrices::try_fh2())?;
    table(fi_gamedb::leaderboards::Leaderboards::try_fh2())?;
    table(fi_gamedb::stats::StatsLayout::try_fh2())?;
    table(fi_gamedb::stats_boards::StatsBoards::try_fh2())?;
    table(fi_gamedb::card_ids::CardIds::try_fh2())?;
    table(fi_gamedb::topspeed::TopSpeeds::try_fh2())?;
    table(fi_gamedb::pi::try_bands())?;
    table(fi_gamedb::RULES.load())?;
    table(fi_gamedb::rules::LEVEL_CURVE.load())?;
    table(fi_gamedb::rules::TOKENS.load())?;
    table(fi_gamedb::names::NAMES.load())?;
    table(fi_gamedb::gates::BLOCKED_CARS.load())?;
    table(fi_gamedb::gates::ENFORCED_STATS_CEILINGS.load())?;
    table(fi_playerdb::schema::DDL.load())?;
    table(crate::game::proxy::universe::storefront::FILE_KEYWORDS.load())?;
    table(crate::game::proxy::forza::ECONOMY_SETTINGS.load())?;
    table(fi_guard::LIMITS.load())?;
    Ok(Loaded {
        root: dir.root(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::games_from;

    #[test]
    fn the_repository_game_data_loads() {
        // The private repository's gamedata/ (.cargo/config.toml sets FI_GAME_DATA_DIR for cargo).
        let games = games_from(Some("fh2,fm5"));
        let loaded = super::load_games(&games).unwrap();
        assert!(loaded.files >= 19, "{loaded:?}");
        assert!(super::load_games(&games_from(None)).is_ok());
    }

    #[test]
    fn fi_games_names_the_required_games() {
        assert_eq!(games_from(None), ["fh2"]);
        assert_eq!(games_from(Some("fh2, fm5")), ["fh2", "fm5"]);
        assert_eq!(games_from(Some("fh2,,")), ["fh2"]);
    }
}
