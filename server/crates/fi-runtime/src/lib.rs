//! Runtime plumbing shared by every ForzaInfinity backend service (ADR 0002 §4).
//!
//! - [`config`]: typed environment variables; a missing or malformed one fails startup loudly.
//! - [`telemetry`]: logs as Cloud Logging structured JSON (`severity`, `message`, trace
//!   correlation), one line per event (docs/server/OBSERVABILITY.md §2).
//! - [`http`]: request context (request id, W3C/Cloud trace ids), the access log line, health
//!   endpoints, the edge's shared-secret check (ADR 0903 §1) and serving with graceful shutdown on
//!   Cloud Run's SIGTERM.
//! - [`ratelimit`]: in-process token buckets per route class and subject (ADR 0903 §3).

pub mod config;
pub mod http;
pub mod ratelimit;
pub mod telemetry;

pub use config::{ConfigError, ServiceInfo};
