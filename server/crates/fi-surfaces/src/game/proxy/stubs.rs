//! Stub answers, the last step of the dispatch (GAME_PLATFORM.md §3a (4)): the Beta's
//! `server/responses/hosts/<host>/_default.json` files, embedded (OD-32 allows game-derived stubs
//! in this private repository; these four are the Beta's own empty answers). Every path and method
//! of a stubbed host gets the host's answer, as in the Beta. The free-roam hosts' stubs
//! (gameserverds, sessiondirectory) answer only what `crate::game::freeroam::handle` leaves
//! (`None`), as the Beta's stubs answered while its free roam was off.

/// The stubbed hosts and their JSON answers (status 200).
const STUBS: [(&str, &str); 4] = [
    (
        "achievements.xboxlive.com",
        r#"{"achievements":[],"pagingInfo":{"continuationToken":null,"totalRecords":0}}"#,
    ),
    (
        "gameserverds.xboxlive.com",
        r#"{"variants":[],"variantSchemas":[]}"#,
    ),
    (
        "music.xboxlive.com",
        r#"{"HasSubscription":false,"IsSubscriptionAvailableForPurchase":false,"Culture":"pt-BR"}"#,
    ),
    ("sessiondirectory.xboxlive.com", r#"{"results":[]}"#),
];

/// The stub JSON of `host` (lower case), if it has one.
pub fn for_host(host: &str) -> Option<&'static str> {
    STUBS
        .iter()
        .find(|(h, _)| *h == host)
        .map(|(_, body)| *body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stub_is_json_and_unknown_hosts_have_none() {
        for (host, body) in STUBS {
            assert!(
                serde_json::from_str::<serde_json::Value>(body).is_ok_and(|v| v.is_object()),
                "{host}"
            );
            assert_eq!(for_host(host), Some(body));
        }
        assert_eq!(for_host("eds.xboxlive.com"), None);
        assert_eq!(for_host("profile.xboxlive.com"), None);
    }
}
