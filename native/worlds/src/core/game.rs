// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The boundary between the game-agnostic core and a game module (OD-186): the core owns the
//! transport, the control channel, admission, limits, relay legs and logs; a game module owns
//! everything game-specific (its wire codec, engines, side channels, game-scoped settings, the
//! anti-cheat watch, catalogs). `world.open` names the game (`game`, e.g. `"fh2"`); the core
//! finds its `Game` and asks it for the world's `GameWorld`, then drives that through this trait
//! only. A new game is a new module registered with the server; the core does not change.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use serde_json::{Map, Value};

use super::{Profile, fields::Obj, rng::Rng};
use crate::control::proto::{CmdError, CmdResult};

/// A datagram a game world sends; the core maps it to the world's socket (or a relay leg).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Send {
    pub to: SocketAddr,
    pub data: Vec<u8>,
}

/// A notice a game module raises (`notice` event, contract §4.7).
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub id: &'static str,
    pub level: &'static str,
    pub args: Option<Value>,
}

/// The world's admission as a game's engines ask it per packet (contract §6.2): the core owns
/// it (grants, lease, FIUP); `mono` is the world's monotonic ms the engine runs on.
pub trait Admit: std::marker::Send + Sync {
    /// The decision for `xuid` at `from`: `kind` 0 the game socket, 1 the hook's.
    fn allowed(&self, xuid: u64, from: SocketAddr, kind: u8, mono: u64) -> bool;
    /// `xuid` holds a live grant its endpoint can still prove (a held OpenConnection waits).
    fn proof_expected(&self, xuid: u64, mono: u64) -> bool;
}

/// What a game world tells the core (control events, contract §4.5).
#[derive(Debug, Clone, PartialEq)]
pub enum GameEvent {
    /// A peer finished its join in an engine.
    PeerSynced {
        xuid: u64,
        engine: &'static str,
        peer: u8,
    },
    /// A synced peer left an engine; `reason` is the contract's.
    PeerLeft {
        xuid: u64,
        engine: &'static str,
        peer: u8,
        reason: &'static str,
    },
    /// A game event the core forwards as it is (`world.gameSession.request`, `watch.report`,
    /// `catalog.need`, `notice`): `world` adds the world's `worldId` to the body.
    Custom {
        ev: &'static str,
        world: bool,
        body: serde_json::Map<String, Value>,
    },
    /// A validation refusal (`peer.rejected`; the core throttles it).
    Rejected {
        xuid: u64,
        /// The sender's engine index when it is seated (stdout names the peer by it).
        peer: Option<u8>,
        kind: &'static str,
        message: Option<u32>,
        router: Option<u32>,
        value: Option<u32>,
    },
}

/// What `world.open` tells the game besides its own fields.
pub struct OpenInfo<'a> {
    /// The whole `world.open` request: the game reads its game-scoped fields from it.
    pub request: Obj<'a>,
    pub profile: Profile,
    pub host_xuid: u64,
    pub is_player: bool,
    pub max_players: u8,
    /// The world's random source (seeds the game draws at creation).
    pub rng: &'a mut Rng,
    /// The world's admission, installed in every engine the world makes.
    pub admit: Arc<dyn Admit>,
}

/// What a game world command may use.
pub struct GameCtx {
    pub profile: Profile,
    pub now: u64,
    /// Notices raised by the command (the core sends them).
    pub notices: Vec<Notice>,
}

/// A game module (one per supported game).
pub trait Game: std::marker::Send + Sync {
    /// The `game` id of `world.open` and of the world context (`"fh2"`).
    fn id(&self) -> &'static str;

    /// Validates `world.open`'s game-scoped fields and makes the world's game side. Errors are
    /// `bad_request` naming the field. Also returns the notices of the open (catalog missing...).
    fn open(&self, info: OpenInfo) -> Result<(Box<dyn GameWorld>, Vec<Notice>), CmdError>;

    /// A process-level command of this game (`catalog.set`, `catalog.parts`); `None` when the
    /// game has no such command.
    fn command(&self, _op: &str, _request: Obj) -> Option<CmdResult> {
        None
    }
}

/// One world's game side: its engines and game state. The core calls it inside the world's
/// fault boundary; a panic here closes only this world.
pub trait GameWorld: std::marker::Send {
    /// The normalized game-scoped part of `world.open` (idempotency: the same value answers
    /// `ok`, another `exists`).
    fn spec(&self) -> Value;

    /// The lobbies this world serves besides its base activity (`world.open`'s `lobbies`).
    fn lobbies(&self) -> Vec<&'static str>;

    /// A game-scoped world command (`world.voice`, `world.voiceBlocks`, `world.watch`,
    /// `world.gameSession.answer`, ...); `None` when the game has no such command.
    fn command(&mut self, op: &str, request: Obj, ctx: &mut GameCtx) -> Option<CmdResult>;

    /// A datagram from `from` (canonical endpoint) the core routed to this world.
    fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>);

    /// The game's timers (engine ticks). Called only when `next_tick` asked for it.
    fn tick(&mut self, now: u64, out: &mut Vec<Send>);

    /// When the game next needs `tick`; `None` when idle (no tick for an idle world).
    fn next_tick(&self, profile: Profile, now: u64) -> Option<u64>;

    /// Synced players (the host's own game included when it is in).
    fn synced(&self) -> usize;

    /// Admitted players not synced yet, and held opens.
    fn handshaking(&self) -> usize;

    /// Drops every peer of `xuid`; the count dropped.
    fn kick(&mut self, xuid: u64, now: u64, out: &mut Vec<Send>) -> usize;

    /// Sends every peer the game's Close (drain, shutdown).
    fn close_all(&mut self, out: &mut Vec<Send>);

    /// `WorldState`'s game-dependent fields: `peers`, `lobbyPlayers`, `activity`, `engines`,
    /// `inProgress` (OD-194: `true` while the activity takes no newcomers, else absent),
    /// `hostInWorld` (contract §4.5); a private official world adds `private: true` and, while
    /// somebody leads, `leader` (the leader's XUID, a decimal string).
    fn state(&self) -> Map<String, Value>;

    /// Datagrams the game dropped so far (stats).
    fn dropped(&self) -> u64;

    /// Peers (synced or handshaking) of any XUID but `except` (0: everybody).
    fn occupants(&self, except: u64) -> usize {
        let _ = except;
        self.synced() + self.handshaking()
    }

    /// The events since the last call, in order.
    fn take_events(&mut self) -> Vec<GameEvent> {
        Vec::new()
    }

    /// CR-S67: the grant source of the voice blocks (each XUID's signed instant, Unix seconds).
    fn set_grant_voice_blocks(&mut self, _blocks: &BTreeMap<u64, i64>) {}

    /// Whether this world may become a private official world (WORLDS_OFFICIAL.md §12.2 item 8:
    /// free roam or Road Trip, never a car meet).
    fn supports_private(&self) -> bool {
        false
    }

    /// The world became a private official world (`world.context`'s `world.private`; one way,
    /// §12.8b): the game's session privacy and its leader, sent at once.
    fn set_private(&mut self, _out: &mut Vec<Send>) {}
}
