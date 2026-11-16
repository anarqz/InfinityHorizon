//! The player card's badge and title ids (T-PD-S5, `fh2/card-ids.json`;
//! docs/web/PLAY_STATS_BOARDS_V2_METHOD.md): `currentBadgeId` and `currentPlayerTitleId` are
//! `List_BadgeAndTitleUnlocks.UnlockId`s (one id space, 1 to 460, not contiguous): 377 rows have a
//! badge, 329 a title. The game resolves either with `WHERE UnlockId = %d` and falls back to
//! `UnlockId = 1` for a row it does not find ([`FALLBACK`]). Only the ids are read here; owning
//! one (its unlock condition) needs the profile.

use serde::Deserialize;
use std::collections::BTreeSet;

/// The game data file of the table (`FI_GAME_DATA_DIR`; never in the crate).
pub const CARD_IDS_FILE: &str = "fh2/card-ids.json";

static FH2: fi_gamedata::Lazy<CardIds> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(CARD_IDS_FILE, CardIds::from_json));

/// The id the game shows for a badge or title it cannot find.
pub const FALLBACK: u16 = 1;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    badge_ids: Vec<u16>,
    title_ids: Vec<u16>,
}

/// The valid ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardIds {
    badges: BTreeSet<u16>,
    titles: BTreeSet<u16>,
}

impl CardIds {
    /// The table of the game data ([`CARD_IDS_FILE`]), parsed once; panics with the game data's
    /// error ([`CardIds::try_fh2`]).
    pub fn fh2() -> &'static CardIds {
        &FH2
    }

    /// [`CardIds::fh2`], or why the game data has none.
    pub fn try_fh2() -> Result<&'static CardIds, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// Parses the table (`badgeIds`, `titleIds`).
    pub fn from_json(text: &str) -> Result<CardIds, String> {
        let raw: Raw = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Ok(CardIds {
            badges: raw.badge_ids.into_iter().collect(),
            titles: raw.title_ids.into_iter().collect(),
        })
    }

    /// Whether `id` is a badge.
    pub fn badge(&self, id: u16) -> bool {
        self.badges.contains(&id)
    }

    /// Whether `id` is a title.
    pub fn title(&self, id: u16) -> bool {
        self.titles.contains(&id)
    }

    /// Badges, titles.
    pub fn counts(&self) -> (usize, usize) {
        (self.badges.len(), self.titles.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_games_ids() {
        let c = CardIds::fh2();
        assert_eq!(c.counts(), (377, 329));
        assert!(c.badge(FALLBACK) && c.title(FALLBACK));
        assert!(!c.badge(0) && !c.title(0));
        assert!(!c.badge(461));
    }
}
