//! One proxied game request (GAME_PLATFORM.md §3a), after the ticket and header checks.

use crate::game::ticket::Ticket;

/// One proxied game request (GAME_PLATFORM.md §3a), after the ticket and header checks.
pub struct GameRequest<'a> {
    /// The verified ticket of a live session.
    pub ticket: &'a Ticket,
    /// The game's HTTP method.
    pub method: &'a axum::http::Method,
    /// The original host, lower case (for example `sessiondirectory.xboxlive.com`).
    pub host: &'a str,
    /// The original path, starting with `/`.
    pub path: &'a str,
    /// The original query string, without `?`.
    pub query: Option<&'a str>,
    /// The request headers as received.
    pub headers: &'a axum::http::HeaderMap,
    /// The body, byte for byte.
    pub body: &'a [u8],
    /// The caller's address as the platform sees it (`CF-Connecting-IP` behind the edge, the peer locally).
    pub client_ip: Option<std::net::IpAddr>,
}
