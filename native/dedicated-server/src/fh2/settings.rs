// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! `RelaySettings` (contract §7, `W/RelayState.cs`, client `03300b4`): the per-engine settings,
//! their profile defaults (§8) and the `world.open` `settings` overrides with the C# validation
//! (`W/RelayEngine.cs:60-72`). Unknown keys are `bad_request {field: "settings.<name>"}`.

use serde_json::{Map, Value};

use crate::{control::proto::CmdError, core::Profile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySettings {
    pub weather_seed: Option<i32>,
    pub tag_seed: Option<i32>,
    pub team_seed: Option<i32>,
    pub time_sync_ms: u64,
    pub invite_close_ms: u64,
    pub quiet_drop_ms: u64,
    pub keepalive_ms: u64,
    pub resend_after_ms: u64,
    pub race_invite_ms: u64,
    pub race_prepare_ms: u64,
    pub race_grid_ms: u64,
    pub race_finish_ms: u64,
    pub game_options_gap_ms: u64,
    pub sync_complete_ms: u64,
    pub reliable_window: u64,
    pub queue_max_messages: u64,
    pub unsynced_timeout_ms: u64,
    pub max_peers_per_ip: u64,
    pub max_unsynced: u64,
    pub handshakes_per_second: u64,
    pub takeover_quiet_ms: u64,
    pub rate_packets: u64,
    pub rate_messages: u64,
    pub rate_bytes: u64,
    pub abuse_ms: u64,
    pub abuse_ban_ms: u64,
    pub history_max_bits: u64,
    pub history_max_entries: u64,
    pub queue_max_bits: u64,
}

impl RelaySettings {
    /// The C# defaults, with the public profile's tighter queue caps (contract §8).
    pub fn for_profile(profile: Profile) -> RelaySettings {
        let mut s = RelaySettings {
            weather_seed: None,
            tag_seed: None,
            team_seed: None,
            time_sync_ms: 30_000,
            invite_close_ms: 1500,
            quiet_drop_ms: 15_000,
            keepalive_ms: 500,
            resend_after_ms: 500,
            race_invite_ms: 15_000,
            race_prepare_ms: 45_000,
            race_grid_ms: 45_000,
            race_finish_ms: 30_000,
            game_options_gap_ms: 15_000,
            sync_complete_ms: 10_000,
            reliable_window: 128,
            queue_max_messages: 4096,
            unsynced_timeout_ms: 3000,
            max_peers_per_ip: 2,
            max_unsynced: 32,
            handshakes_per_second: 4,
            takeover_quiet_ms: 3000,
            rate_packets: 300,
            rate_messages: 1000,
            rate_bytes: 256 * 1024,
            abuse_ms: 5000,
            abuse_ban_ms: 60_000,
            history_max_bits: 2 * 1024 * 1024,
            history_max_entries: 16_384,
            queue_max_bits: 8 * 1024 * 1024,
        };
        if profile == Profile::Public {
            s.queue_max_bits = 4 * 1024 * 1024;
            s.queue_max_messages = 2048;
            s.max_unsynced = 8;
        }
        s
    }

    /// Makes a join's snapshot fit the joiner's reliable queue (contract §8): the snapshot holds
    /// every other player's kept history, so with `max_players - 1` others at their history
    /// caps it must stay within `QueueMaxMessages`/`QueueMaxBits`, or one synced peer holding a
    /// legal history would doom every later joiner (QueueFull). The entry cap goes down to the
    /// queue's share (the engine trims a peer's histories down to it before dooming anybody);
    /// the queue's bit cap goes up to carry the others' history bits (the history bit cap dooms
    /// at once, so it is never lowered).
    pub fn fit_join_snapshot(&mut self, max_players: usize) {
        let others = max_players.saturating_sub(1).max(1) as u64;
        self.history_max_entries = self
            .history_max_entries
            .min((self.queue_max_messages / others).max(1));
        self.queue_max_bits = self
            .queue_max_bits
            .max(self.history_max_bits.saturating_mul(others));
    }

    /// Applies `world.open`'s `settings` object.
    pub fn apply(&mut self, overrides: &Map<String, Value>) -> Result<(), CmdError> {
        for (key, value) in overrides {
            let bad = || CmdError::bad(format!("settings.{key}"));
            if matches!(key.as_str(), "weatherSeed" | "tagSeed" | "teamSeed") {
                let seed = if value.is_null() {
                    None
                } else {
                    Some(
                        value
                            .as_u64()
                            .and_then(|n| i32::try_from(n).ok())
                            .ok_or_else(bad)?,
                    )
                };
                match key.as_str() {
                    "weatherSeed" => self.weather_seed = seed,
                    "tagSeed" => self.tag_seed = seed,
                    _ => self.team_seed = seed,
                }
                continue;
            }
            // the C# fields are `long` for times and `int` for counts; a time up to 2^62 keeps
            // every `now + time` in range
            let n = value.as_u64().filter(|n| *n <= 1 << 62).ok_or_else(bad)?;
            // (field, minimum, maximum) as the C# constructor validates
            let (slot, min, max): (&mut u64, u64, u64) = match key.as_str() {
                "timeSyncMs" => (&mut self.time_sync_ms, 1, u64::MAX),
                "inviteCloseMs" => (&mut self.invite_close_ms, 0, u64::MAX),
                "quietDropMs" => (&mut self.quiet_drop_ms, 0, u64::MAX),
                "keepaliveMs" => (&mut self.keepalive_ms, 0, u64::MAX),
                "resendAfterMs" => (&mut self.resend_after_ms, 1, u64::MAX),
                "raceInviteMs" => (&mut self.race_invite_ms, 0, u64::MAX),
                "racePrepareMs" => (&mut self.race_prepare_ms, 1, u64::MAX),
                "raceGridMs" => (&mut self.race_grid_ms, 1, u64::MAX),
                "raceFinishMs" => (&mut self.race_finish_ms, 1, u64::MAX),
                "gameOptionsGapMs" => (&mut self.game_options_gap_ms, 1, u64::MAX),
                "syncCompleteMs" => (&mut self.sync_complete_ms, 1, u64::MAX),
                "reliableWindow" => (&mut self.reliable_window, 1, 256),
                "queueMaxMessages" => (&mut self.queue_max_messages, 1, u64::MAX),
                "unsyncedTimeoutMs" => (&mut self.unsynced_timeout_ms, 0, u64::MAX),
                "maxPeersPerIp" => (&mut self.max_peers_per_ip, 1, u64::MAX),
                "maxUnsynced" => (&mut self.max_unsynced, 1, 254),
                "handshakesPerSecond" => (&mut self.handshakes_per_second, 0, u64::MAX),
                "takeoverQuietMs" => (&mut self.takeover_quiet_ms, 0, u64::MAX),
                "ratePackets" => (&mut self.rate_packets, 1, u64::MAX),
                "rateMessages" => (&mut self.rate_messages, 1, u64::MAX),
                "rateBytes" => (&mut self.rate_bytes, 1, u64::MAX),
                "abuseMs" => (&mut self.abuse_ms, 0, u64::MAX),
                "abuseBanMs" => (&mut self.abuse_ban_ms, 0, u64::MAX),
                "historyMaxBits" => (&mut self.history_max_bits, 1, u64::MAX),
                "historyMaxEntries" => (&mut self.history_max_entries, 1, u64::MAX),
                "queueMaxBits" => (&mut self.queue_max_bits, 1, u64::MAX),
                _ => return Err(bad()),
            };
            if !(min..=max).contains(&n) {
                return Err(bad());
            }
            *slot = n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_world_of_histories_fits_one_joiners_queue() {
        for profile in [Profile::Private, Profile::Public] {
            let mut s = RelaySettings::for_profile(profile);
            s.fit_join_snapshot(12);
            // eleven others at their caps: the snapshot fits the joiner's queue
            assert!(s.history_max_entries * 11 <= s.queue_max_messages);
            assert!(s.history_max_bits * 11 <= s.queue_max_bits);
            assert_eq!(s.history_max_bits, 2 * 1024 * 1024);
        }
        let mut s = RelaySettings::for_profile(Profile::Private);
        s.fit_join_snapshot(1);
        assert_eq!(s.history_max_entries, 4096);
    }
}
