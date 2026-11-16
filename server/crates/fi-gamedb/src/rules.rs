//! Forza Horizon 2's own economy numbers, which the Hardcore guardrails measure with
//! ([`fi_guard::GameRules`]; OD-62, ADR 0020). Game-derived values (OD-32: this private repository
//! may hold them, they never ship), read by the RE machine from the game's files and code (register
//! CR-S24, `deprecated/beta/notes/local-docs/re/hardcore-save-fields.md` §3.1, §4, §5):
//!
//! - the level curve, `PlayerXPLevels.xml` in `media/GameTunableSettings.zip`, as the game's parser
//!   builds its table (`F_14032b900`): within a band, step `k` (from 0, restarting in each band)
//!   costs `XPPerLevel + (k + 1) x XPIncreasePerLevel`. Yellow, 14 levels of 15,000 XP; Green, Blue,
//!   Pink, Orange, Purple, Red, Turquoise, Bronze and Silver, 15 levels of 20,000 XP each; Gold, 850
//!   levels from 20,000 XP with an increase of 300 (its first step 20,300). The driver level, from
//!   0, is the number of steps whose cumulative XP is at most `Main/XP` (the game shows it plus
//!   one): level 1 at 15,000, 14 at 210,000, 149 at 2,910,000, 150 at 2,930,300, 151 at
//!   2,950,900, and the table's last, 999, at 128,412,500 ([`struct@LEVEL_CURVE`]). ProfileData
//!   `Main/Level` is **not** that level (SAURON 2026-10-31, a genuine career: `Main/Level` 0 at
//!   223,773 XP, shown in game as driver level 15, and 1 at 232,284): it reads as the steps done
//!   inside the current band, equal to the driver level only in Yellow (where the RE's earlier
//!   saves all were). So the server derives a save's level from its XP
//!   (`fi_saves::gates::with_driver_level`). A profile sync's `playerRaceHistory[0].level` is the
//!   same in-band step (0 at 228,874 XP, 1 at 230,494 and 232,194), so its level is derived from
//!   the XP too (`crate::gates::driver_level`) and no reported level is checked against the XP;
//! - the skill-token schedule (`F_1406c3260` -> `F_1406a4870` -> `F_1406afbb0`): ProfileData
//!   `Main/NumSkillTokens` is tokens earned, one at 2,000 SaveState `SkillPointsTotal` and one per
//!   100,000 after; `UnspentSkillPointsTotal` is the points since the last token (less than
//!   100,000); grants stop at the perk table's total cost, 65 tokens for 25 perks ([`struct@TOKENS`]);
//! - the level-up prize: FH2 has no wheelspins; each level-up gives one tombola spin
//!   (`CreditRewardPerLevel` is 0 in every band), whose credit prizes (gamedb `TombolaPrizes`) run
//!   from 3,000 to 500,000 (`tombolaMaxCredits`);
//! - the per-race payout limits of `GameTunableSettings.ini`, `RaceWinnings`:
//!   `MaxBaseCreditsPerRace` and `MaxExperiencePerRace`, 60,000 each.

//!
//! The numbers are the game data's [`ECONOMY_FILE`] (`FI_GAME_DATA_DIR`), never compiled in.

use fi_gamedata::Lazy;
use fi_guard::{GameRules, LevelBand, LevelCurve, TokenSchedule};
use serde::Deserialize;

/// The game data file of the economy numbers.
pub const ECONOMY_FILE: &str = "fh2/economy.json";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBand {
    steps: i64,
    xp_per_level: i64,
    increase_per_level: i64,
}

#[derive(Deserialize)]
struct RawTokens {
    first: i64,
    step: i64,
    cap: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEconomy {
    level_curve: Vec<RawBand>,
    tokens: RawTokens,
    tombola_max_credits: i64,
    max_base_credits_per_race: i64,
    max_experience_per_race: i64,
    #[serde(default)]
    #[allow(dead_code)]
    source: Option<String>,
}

/// The rules of an economy JSON (`levelCurve` bands in order, `tokens`, the tombola's largest
/// credit prize, `GameTunableSettings.ini`'s `RaceWinnings` limits). The curve's bands live as long
/// as the process (`fi_guard::LevelCurve` borrows them for `'static`): the process reads the file
/// once.
pub fn from_json(text: &str) -> Result<GameRules, String> {
    let raw: RawEconomy = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if raw.level_curve.is_empty() || raw.level_curve.iter().any(|b| b.steps <= 0) {
        return Err("levelCurve needs bands of at least one step".to_owned());
    }
    let bands: Vec<LevelBand> = raw
        .level_curve
        .into_iter()
        .map(|b| LevelBand {
            steps: b.steps,
            xp_per_level: b.xp_per_level,
            increase_per_level: b.increase_per_level,
        })
        .collect();
    Ok(GameRules {
        spin_credits: raw.tombola_max_credits,
        race_credits: raw.max_base_credits_per_race,
        race_xp: raw.max_experience_per_race,
        level_curve: Some(LevelCurve {
            bands: Box::leak(bands.into_boxed_slice()),
        }),
        tokens: Some(TokenSchedule {
            first: raw.tokens.first,
            step: raw.tokens.step,
            cap: raw.tokens.cap,
        }),
    })
}

/// FH2's rules for the guardrails: a level-up may bring one tombola spin's largest credit prize,
/// a race at most its base credits and XP limits, the level must follow [`struct@LEVEL_CURVE`] and
/// the tokens [`struct@TOKENS`]. The game data's [`ECONOMY_FILE`], read once; dereferencing panics
/// with the game data's error, which a process that checked it at start never sees.
pub static RULES: Lazy<GameRules> = Lazy::new(|| fi_gamedata::parse(ECONOMY_FILE, from_json));

/// FH2's level curve ([`struct@RULES`]).
pub static LEVEL_CURVE: Lazy<LevelCurve> = Lazy::new(|| {
    let rules = RULES.load().map_err(Clone::clone)?;
    Ok(rules.level_curve.expect("from_json always sets a curve"))
});

/// FH2's skill-token schedule ([`struct@RULES`]).
pub static TOKENS: Lazy<TokenSchedule> = Lazy::new(|| {
    let rules = RULES.load().map_err(Clone::clone)?;
    Ok(rules.tokens.expect("from_json always sets the tokens"))
});

#[cfg(test)]
mod tests {
    use super::*;
    use fi_guard::Skills;

    #[test]
    fn the_curve_matches_what_the_game_stores() {
        // The RE's checkpoints and its checks over real saves (numbers only): 19,706 -> 1,
        // 100,717 -> 6, 101,927 -> 6, 110,484 -> 7 (all in Yellow, where `Main/Level` happens to be
        // the driver level).
        for (xp, level) in [
            (0, 0),
            (14_999, 0),
            (15_000, 1),
            (19_706, 1),
            (100_717, 6),
            (101_927, 6),
            (110_484, 7),
            (209_999, 13),
            (210_000, 14),
            (223_773, 14),
            (229_999, 14),
            (230_000, 15),
            (232_284, 15),
            (2_909_999, 148),
            (2_910_000, 149),
            (2_930_299, 149),
            (2_930_300, 150),
            (2_950_899, 150),
            (2_950_900, 151),
            (128_412_499, 998),
            (128_412_500, 999),
        ] {
            assert_eq!(LEVEL_CURVE.level_of(xp), level, "{xp}");
            assert!(RULES.level_fits(xp, level), "{xp}");
        }
        // The owner's career (SAURON 2026-10-31, numbers only): 223,773 XP is shown in game as
        // driver level 15, i.e. stored level 14 plus one; 232,284 XP is stored level 15 (16 in
        // game). Its `Main/Level` read 0 and 1 there: the steps done inside Green (from 210,000),
        // not the driver level.
        assert_eq!(LEVEL_CURVE.level_of(223_773) + 1, 15);
        assert_eq!(
            LEVEL_CURVE.level_of(223_773) - LEVEL_CURVE.level_of(210_000),
            0
        );
        assert_eq!(
            LEVEL_CURVE.level_of(232_284) - LEVEL_CURVE.level_of(210_000),
            1
        );
        assert_eq!(LEVEL_CURVE.top_xp(), 128_412_500);
        // The table ends at 999, the game's level limit (the gates' MAX_LEVEL).
        assert_eq!(LEVEL_CURVE.level_of(crate::gates::MAX_XP), 999);
        assert_eq!(crate::gates::MAX_LEVEL, 999);
        // An edited level: a would-block in the guardrails.
        assert!(!RULES.level_fits(100_717, 40));
        assert!(!RULES.level_fits(5_000_000, 6));
        assert!(
            !RULES.level_fits(2_930_000, 150),
            "Gold's first step is 20,300"
        );
    }

    #[test]
    fn the_tokens_match_what_the_game_grants() {
        // The RE's checks over real saves (numbers only): 262,450 -> 3 tokens, 60,450 unspent;
        // 325,922 -> 4, 23,922; 267,559 -> 3, 65,559. A fresh career: 0, 0, 0.
        for (points, tokens, unspent) in [
            (262_450, 3, 60_450),
            (325_922, 4, 23_922),
            (267_559, 3, 65_559),
            (0, 0, 0),
            (1_999, 0, 1_999),
            (2_000, 1, 0),
            (102_000, 2, 0),
        ] {
            let s = Skills {
                tokens,
                points,
                unspent,
            };
            assert!(RULES.skills_fit(&s), "{s:?}");
        }
        assert!(!RULES.skills_fit(&Skills {
            tokens: 4,
            points: 262_450,
            unspent: 60_450
        }));
        // 65 tokens at most (2,000 + 64 x 100,000 points); more points grant none.
        assert_eq!(TOKENS.tokens(6_402_000), 65);
        assert!(RULES.skills_fit(&Skills {
            tokens: 65,
            points: 9_000_000,
            unspent: 12_345
        }));
    }
}
