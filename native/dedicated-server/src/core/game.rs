// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The boundary between the game-agnostic core and a game module (OD-186): the core owns the
//! transport, the control channel, admission, limits, relay legs and logs; a game module owns
//! everything game-specific (its wire codec, engines, side channels, game-scoped settings, the
//! anti-cheat watch, catalogs). `world.open` names the game (`game`, e.g. `"fh2"`); the core
//! finds its `Game` and asks it for the world's `GameWorld`, then drives that through this trait
//! only. A new game is a new module registered with the server; the core does not change.

use std::net::SocketAddr;

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
    /// `hostInWorld` (contract §4.5).
    fn state(&self) -> Map<String, Value>;

    /// Datagrams the game dropped so far (stats).
    fn dropped(&self) -> u64;
}
