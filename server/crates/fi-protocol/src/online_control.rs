//! Global online availability (docs/contracts/ONLINE_CONTROL.md).
use serde::{Deserialize, Serialize};

/// Authoritative online state; closure is latched until explicit operator reopen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnlineState {
    /// Ordinary operations may continue their normal authorization checks.
    Open,
    /// Ordinary online operations are unavailable for every tier.
    Closed,
}

/// Public reason for a global closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The service's spending allowance was exhausted.
    Budget,
    /// An operator closed online access.
    Operator,
}

/// GET /core/v1/online-status and the details of a closed-service error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineStatus {
    /// Open or latched closed.
    pub state: OnlineState,
    /// Monotonic within the serving authority (platform or edge), not across authorities.
    pub revision: u64,
    /// None only while open.
    pub reason: Option<CloseReason>,
    /// First closure instant; changing its reason never renews the cleanup window.
    pub closed_at: Option<String>,
    /// Suggested status retry, not a reopening promise.
    pub retry_after_seconds: u32,
}

/// Private edge financial-stop reason; operator maintenance remains a platform control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeFinancialReason {
    /// Financial closure of every online tier.
    Budget,
}

/// Private POST /financial-stop; validates SHA-256 hex and exactly 120 seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeFinancialStopRequest {
    /// Controller-minted lowercase SHA-256 hex identifier.
    pub closure_id: String,
    /// Financial closure only.
    pub reason: EdgeFinancialReason,
    /// Exactly 120; a retry cannot renew the first closure's drain.
    pub drain_seconds: u32,
}

/// Durable acknowledgment of POST /financial-stop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeFinancialStopReceipt {
    /// Always true on this successful endpoint.
    pub closed: bool,
    /// Identifier acknowledged by this request.
    pub closure_id: String,
    /// UTC6 first closure instant, retained across subsequent close identifiers.
    pub closed_at: String,
}

/// Private GET /financial-state; null status means uninitialized, never implicitly open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeFinancialState {
    /// Edge authority's CAS revision; zero only before initialization.
    pub edge_revision: u64,
    /// Most recently acknowledged closure; null while open/uninitialized.
    pub closure_id: Option<String>,
    /// Edge state, independently of the platform's revision.
    pub status: Option<OnlineStatus>,
}

/// Explicit private POST /financial-reopen; revisions must be JavaScript-safe integers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeFinancialReopenRequest {
    /// Exact edge revision observed by the operator before readiness probes.
    pub expected_edge_revision: u64,
    /// Exact open platform revision required from every configured origin.
    pub expected_origin_revision: u64,
}

/// Acknowledgment after every origin is ready/open and the edge CAS commits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeFinancialReopenReceipt {
    /// Always false on this successful endpoint.
    pub closed: bool,
    /// Newly committed edge revision.
    pub edge_revision: u64,
    /// Platform revision proven by all configured origins.
    pub origin_revision: u64,
}
