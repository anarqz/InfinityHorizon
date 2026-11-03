// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The stable `ev` ids of the stdout log lines (contract §2.3). Ids are never renamed; new ones
//! are added. Control-channel event and notice ids are in `core::events`.

// process
pub const DS_STARTED: &str = "ds.started";
pub const DS_STOPPED: &str = "ds.stopped";
pub const DS_CONFIG_INVALID: &str = "ds.config.invalid";
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
