// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The agent's part of a private official world (WORLDS_OFFICIAL.md §12.2 items 5 and 10, §12.8b;
//! MATCHMAKING_API §2.10; T-L9), kept apart from the Linux-only loop so it builds and tests
//! everywhere:
//!
//!   - the server's `WorldView.private: true` on a `PUT`/announce answer makes the world private
//!     (one way: an answer without it never makes it public again);
//!   - the engine hears it as `world.context`'s `world.private: true`, sent with every context of
//!     that world from then on;
//!   - the engine's `world.state.leader` (the leader's XUID) is announced as the additive
//!     `leader` of the announce body, for a private world only.

use serde_json::{Value, json};

use crate::core::fields::parse_xuid;

/// Whether a `WorldAnnounced` answer marks its world private (`private: true`; any other value
/// or none: not private).
pub fn answer_is_private(answer: &Value) -> bool {
    answer.get("private").and_then(Value::as_bool) == Some(true)
}

/// Whether an agent's world may become private: a spare (not a car meet session's group world) of
/// free roam or Road Trip, never a car meet (§12.2 items 5 and 8), or an FM5 race spare (CR-S99:
/// private race lobbies). Anything else ignores the answer's `private` (a server fault), so its
/// engine context keeps working.
pub fn may_be_private(activity: &str, group: bool) -> bool {
    !group && matches!(activity, "freeroam" | "roadtrip" | "race")
}

/// The leader a `world.state` event names: a non-zero decimal XUID, else none.
pub fn leader_of(state: &Value) -> Option<String> {
    state
        .get("leader")
        .and_then(Value::as_str)
        .filter(|x| parse_xuid(x).is_some_and(|v| v != 0))
        .map(String::from)
}

/// The announce body (MATCHMAKING_API §2.9 `Heartbeat`): `peers`, `activity`, `voice` per game
/// (FH2 proximity, FM5 open; OD-192 item 7, owner 2026-11-08, so the server's world row says what the engine runs),
/// `inProgress` only when true (OD-194), and `leader` only for a private world that has one
/// (§12.8b).
pub fn announce_body(
    peers: &[String],
    activity: &str,
    in_progress: bool,
    private: bool,
    leader: Option<&str>,
) -> Value {
    let mut body = json!({"peers": peers, "activity": activity});
    if let Some(mode) = super::config::official_voice_mode(super::config::game_of(activity)) {
        body["voice"] = Value::String(mode.to_string());
    }
    if in_progress {
        body["inProgress"] = Value::Bool(true);
    }
    if private && let Some(l) = leader {
        body["leader"] = Value::String(l.to_string());
    }
    body
}

/// `world.context`'s fields for an official world of this VM (the host is the VM's own id, never
/// copied from the answer, contract §6.3); `private: true` once the world is private.
pub fn context_fields(
    world: &str,
    server_ms: i64,
    vm_id: &str,
    game: &str,
    private: bool,
) -> Value {
    let mut fields = json!({"worldId": world, "serverTimeMs": server_ms,
        "world": {"id": world, "game": game, "host": {"official": vm_id}}});
    if private {
        fields["world"]["private"] = Value::Bool(true);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = "4f1c2a6e-1b2d-4c3e-8f40-5a6b7c8d9e01";

    #[test]
    fn only_a_true_private_member_marks_the_world() {
        assert!(answer_is_private(&json!({"id": W, "private": true})));
        for answer in [
            json!({"id": W}),
            json!({"id": W, "private": false}),
            json!({"id": W, "private": "true"}),
            json!({"id": W, "private": null}),
            Value::Null,
        ] {
            assert!(!answer_is_private(&answer), "{answer}");
        }
    }

    #[test]
    fn only_a_free_roam_or_road_trip_spare_may_be_promoted() {
        assert!(may_be_private("freeroam", false));
        assert!(may_be_private("roadtrip", false));
        assert!(!may_be_private("carmeet", false));
        // CR-S99: FM5's race spare
        assert!(may_be_private("race", false));
        assert!(
            !may_be_private("freeroam", true),
            "a car meet session's group world"
        );
    }

    #[test]
    fn the_leader_is_a_nonzero_decimal_xuid() {
        assert_eq!(
            leader_of(&json!({"leader": "1000000000000102"})).as_deref(),
            Some("1000000000000102")
        );
        for state in [
            json!({}),
            json!({"leader": null}),
            json!({"leader": "0"}),
            json!({"leader": "-1"}),
            json!({"leader": "12a"}),
            json!({"leader": 1000000000000102u64}),
            json!({"leader": "123456789012345678901"}),
        ] {
            assert_eq!(leader_of(&state), None, "{state}");
        }
    }

    #[test]
    fn a_public_announce_is_unchanged_and_a_private_one_adds_its_leader() {
        let peers = vec!["1000000000000102".to_string()];
        assert_eq!(
            announce_body(&peers, "freeroam", false, false, Some("1000000000000102")).to_string(),
            r#"{"activity":"freeroam","peers":["1000000000000102"],"voice":"proximity"}"#,
            "public: no leader even when one is known"
        );
        assert_eq!(
            announce_body(&peers, "roadtrip", true, false, None).to_string(),
            r#"{"activity":"roadtrip","inProgress":true,"peers":["1000000000000102"],"voice":"proximity"}"#
        );
        assert_eq!(
            announce_body(&peers, "freeroam", false, true, Some("1000000000000102")).to_string(),
            r#"{"activity":"freeroam","leader":"1000000000000102","peers":["1000000000000102"],"voice":"proximity"}"#
        );
        assert_eq!(
            announce_body(&[], "freeroam", false, true, None).to_string(),
            r#"{"activity":"freeroam","peers":[],"voice":"proximity"}"#,
            "private with nobody leading: no member"
        );
        // FH2's official worlds say proximity (OD-192 item 7); FM5's say open (owner, 2026-11-08)
        assert_eq!(
            announce_body(&peers, "race", false, false, None).to_string(),
            r#"{"activity":"race","peers":["1000000000000102"],"voice":"open"}"#
        );
    }

    #[test]
    fn the_context_says_private_only_for_a_private_world() {
        assert_eq!(
            context_fields(W, 5, "sao-1", "fh2", false),
            json!({"worldId": W, "serverTimeMs": 5,
                "world": {"id": W, "game": "fh2", "host": {"official": "sao-1"}}})
        );
        assert_eq!(
            context_fields(W, 5, "sao-1", "fh2", true),
            json!({"worldId": W, "serverTimeMs": 5,
                "world": {"id": W, "game": "fh2", "host": {"official": "sao-1"}, "private": true}})
        );
        // an FM5 race world's context names its own game (the engine checks it)
        assert_eq!(
            context_fields(W, 5, "sao-1", "fm5", true)["world"]["game"],
            "fm5"
        );
    }
}
