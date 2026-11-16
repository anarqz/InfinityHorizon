//! A world as one caller sees it, on the wire (MATCHMAKING_API.md §2.1): the one mapping of
//! `fi_db::matchmaking::Assessed` to `fi_protocol::matchmaking::WorldView`, shared by matchmaking
//! (the list, the reads, hosting, seats) and core (received invites, CORE_API.md §4.2). The code
//! goes only to the world's host; no address ever does.

use crate::views;
use fi_db::matchmaking::Assessed;
use fi_protocol::matchmaking::{
    OfficialWorldHost, World, WorldHost, WorldHostView, WorldRelay, WorldView,
};

/// `a` as its viewer sees it.
pub fn view(a: &Assessed) -> WorldView {
    let w = &a.row;
    WorldView {
        world: World {
            id: w.id.to_string(),
            game: w.game.clone(),
            name: w.name.clone(),
            // OD-188/189 (§2.9): an official world's host is its Worlds host, no player.
            host: match &w.host_vm_id {
                Some(vm) => WorldHostView::Official(OfficialWorldHost {
                    official: vm.clone(),
                    name: w.host_gamertag.clone(),
                }),
                None => WorldHost {
                    xuid: w.host_xuid.to_string(),
                    gamertag: w.host_gamertag.clone(),
                    tier: w.host_tier.clone(),
                }
                .into(),
            },
            kind: w.kind.clone(),
            mode: w.mode.clone(),
            players: u32::try_from(w.players).unwrap_or(0),
            max_players: u32::try_from(w.max_players).unwrap_or(0),
            region: w.region.clone(),
            ping_class: Some(a.estimate.class.as_str().to_owned()),
            message: w.message.clone(),
            locked: a.needs_code,
            races: w.races,
            since: views::at(w.opened_at),
        },
        audience: w.audience.clone(),
        private: w.private,
        leader: None,
        lobbies: w.lobbies.clone(),
        activity: w.activity.clone(),
        voice: w.voice.clone(),
        host_away: w.host_away,
        // Never empty on the wire (OD-141 review): `any` or the host's mode.
        modes: if fi_protocol::matchmaking::WORLD_MODES.contains(&w.open_modes.as_str()) {
            w.open_modes.clone()
        } else {
            w.mode.clone()
        },
        needs_code: a.needs_code,
        open_seats: u32::try_from(a.open_seats).unwrap_or(0),
        rtt_ms: a.estimate.rtt_ms,
        max_rtt_ms: w.max_rtt_ms.and_then(|m| u32::try_from(m).ok()),
        can_join: a.can_join(),
        why: a.why.map(|y| y.as_str().to_owned()),
        friend_host: w.friend_host,
        invited: w.invited,
        code: if a.own { w.code.clone() } else { None },
        dedicated: w.dedicated,
        modded: w.modded,
        mods: w.mods.clone(),
        members: w.members.clone(),
        transport: w.transport.clone(),
        relay: match (w.transport.as_str(), &w.relay_key) {
            ("relay", Some(key)) => Some(WorldRelay {
                public_key: key.clone(),
                name: w.relay_name.clone().unwrap_or_default(),
            }),
            _ => None,
        },
        pool: w.pool,
        official: w.host_vm_id.is_some(),
    }
}
