// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The stable `ev` ids of the stdout log lines (contract §2.3). Ids are never renamed; new ones
//! are added. Control-channel event and notice ids are in `core::events`.

// process
pub const DS_STARTED: &str = "ds.started";
pub const DS_STOPPED: &str = "ds.stopped";
pub const DS_CONFIG_INVALID: &str = "ds.config.invalid";
/// `worlds relay` / `worlds agent` in a build that does not carry that role yet (exit 7).
pub const DS_ROLE_UNIMPLEMENTED: &str = "ds.role.unimplemented";
pub const DS_CONTROL_UNAVAILABLE: &str = "ds.control.unavailable";
pub const DS_CONTROL_UNAUTHORIZED: &str = "ds.control.unauthorized";
pub const DS_CONTROL_PROTO_UNSUPPORTED: &str = "ds.control.proto_unsupported";
pub const DS_CONTROL_CONNECTED: &str = "ds.control.connected";
pub const DS_CONTROL_CLOSED: &str = "ds.control.closed";
pub const DS_CONTROL_STALLED: &str = "ds.control.stalled";
pub const DS_CONTROL_DIR_MODE: &str = "ds.control.dir_mode";
pub const DS_START_FAILED: &str = "ds.start.failed";
pub const DS_DRAINING: &str = "ds.draining";
pub const DS_FAULT: &str = "ds.fault";
pub const DS_PANIC: &str = "ds.panic";
pub const DS_STATS: &str = "ds.stats";
pub const DS_SIGNAL: &str = "ds.signal";
pub const DS_SOCKET_ERROR: &str = "ds.socket.error";
pub const DS_CATALOG: &str = "ds.catalog";
pub const DS_MEMORY_HIGH: &str = "ds.memory.high";
pub const DS_CAPACITY_REFUSED: &str = "ds.capacity.refused";
// worlds
pub const WORLD_OPENED: &str = "world.opened";
pub const WORLD_CLOSED: &str = "world.closed";
pub const WORLD_FAULT: &str = "world.fault";
pub const WORLD_ADMISSION: &str = "world.admission";
pub const WORLD_LINES_SUPPRESSED: &str = "world.lines_suppressed";
pub const WORLD_OPEN_CONNECTION: &str = "world.open_connection";
/// An engine's world-log line (the C# `Note`): a stable note id and its args (indexes only).
pub const WORLD_NOTE: &str = "world.note";
pub const PEER_REJECTED: &str = "peer.rejected";
pub const WATCH_REPORT_DROPPED: &str = "watch.report_dropped";
// the world agent (`worlds agent`, DEDICATED_SERVER.md §16)
pub const AGENT_STARTED: &str = "agent.started";
pub const AGENT_STOPPED: &str = "agent.stopped";
pub const AGENT_CONFIG_INVALID: &str = "agent.config.invalid";
pub const AGENT_KEY_CREATED: &str = "agent.key.created";
pub const AGENT_SIGNAL: &str = "agent.signal";
pub const AGENT_HELLO: &str = "agent.hello";
pub const AGENT_HELLO_FAILED: &str = "agent.hello.failed";
pub const AGENT_UNAUTHORIZED: &str = "agent.unauthorized";
pub const AGENT_THROTTLED: &str = "agent.throttled";
pub const AGENT_HTTP_FAILED: &str = "agent.http.failed";
pub const AGENT_ACTIVITIES: &str = "agent.activities";
pub const AGENT_CAPACITY: &str = "agent.capacity";
pub const AGENT_CATALOG: &str = "agent.catalog";
pub const AGENT_ENGINE_STARTED: &str = "agent.engine.started";
pub const AGENT_ENGINE_EXITED: &str = "agent.engine.exited";
pub const AGENT_ENGINE_STALE: &str = "agent.engine.stale";
pub const AGENT_ENGINE_REFUSED: &str = "agent.engine.refused";
pub const AGENT_WORLD_OPENED: &str = "agent.world.opened";
pub const AGENT_WORLD_REFUSED: &str = "agent.world.refused";
pub const AGENT_WORLD_LOST: &str = "agent.world.lost";
pub const AGENT_WORLD_ENDED: &str = "agent.world.ended";
pub const AGENT_WORLD_CLOSED: &str = "agent.world.closed";
pub const AGENT_WORLD_DELETED: &str = "agent.world.deleted";
pub const AGENT_WORLD_ADMISSION: &str = "agent.world.admission";
pub const AGENT_GAME_SESSION: &str = "agent.game_session";
