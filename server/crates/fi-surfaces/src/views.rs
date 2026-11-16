//! Rows to wire bodies (fi-protocol DTOs): XUIDs as decimal strings, instants with six fractional
//! digits, tiers by the database clock's verdict.

use fi_db::accounts::{AccessRow, AccountState, DeviceRow, Me, PlayerRow, ProfileRow};
use fi_domain::UnixMs;
use fi_protocol::accounts::{
    Access, BoundDevice, Career, CareerCloud, CareerProgress, Deletion, Me as MeBody, Notice,
    Player, Profile,
};
use fi_protocol::instant;
use fi_protocol::portal::Viewer;

/// A wire instant.
pub fn at(t: UnixMs) -> String {
    instant::format_unix_ms(t.millis()).unwrap_or_default()
}

/// A wire instant from Unix seconds.
pub fn at_secs(secs: i64) -> String {
    instant::format_unix_secs(secs).unwrap_or_default()
}

/// `Profile`.
pub fn profile(p: &ProfileRow) -> Profile {
    Profile {
        profile_id: p.profile_id.to_string(),
        game: p.game.clone(),
        xuid: p.xuid.to_string(),
        gamertag: p.gamertag(),
        handle: p.handle(),
        mode: p.mode.clone(),
        status: p.status.clone(),
        created_at: at(p.created_at),
        last_played_at: p.last_played_at.map(at),
        rename_notice: p.rename_notice.as_ref().map(|(from, when)| {
            fi_protocol::accounts::RenameNotice {
                previous_name: from.clone(),
                renamed_at: at(*when),
            }
        }),
        // CR-S51: set where the answer knows the account's tier ([`profile_lines`]); absent
        // means OD-52's rule to the launcher.
        cloud_line: None,
        content_required: None,
        // CR-S83: on every Profile answer.
        website_listed: Some(p.web_listed),
    }
}

/// [`profile`] with `cloudLine` and `contentRequired` (ACCOUNTS.md §2, CR-S51; OD-239: a Hardcore
/// profile of a game with a line, whatever the tier; never a Noob/Azimuth one);
/// content required = a line and `hardcore` (CONTENT_API.md §3). Explanatory: admission decides.
/// A game without a save line keeps none (CR-S97: FM5).
pub fn profile_lines(p: &ProfileRow) -> Profile {
    let cloud_line = fi_domain::GameRules::of(&p.game).line_mode(&p.mode);
    Profile {
        cloud_line: Some(cloud_line),
        content_required: Some(cloud_line && p.mode == "hardcore"),
        ..profile(p)
    }
}

/// `Player` (CR-S98, CAREERS.md §3): a career's `cloud` where its line has a head; `exportOnly`
/// when the career no longer keeps a line (OD-239: a Noob/Azimuth career's old line).
pub fn player(p: &PlayerRow) -> Player {
    Player {
        profile_id: p.profile_id.to_string(),
        gamertag: p.name.clone(),
        mode: p.mode.clone(),
        xuid: p.xuid.to_string(),
        status: p.status.clone(),
        rename_notice: p.rename_notice.as_ref().map(|(from, when)| {
            fi_protocol::accounts::RenameNotice {
                previous_name: from.clone(),
                renamed_at: at(*when),
            }
        }),
        careers: p
            .careers
            .iter()
            .map(|c| Career {
                game: c.game.clone(),
                career_id: c.career_id.to_string(),
                created_at: at(c.created_at),
                last_played_at: c.last_played_at.map(at),
                // OD-239: a career's old line stays visible, marked export-only, when the career
                // no longer keeps one (Noob/Azimuth).
                cloud: c.head.map(|(revision, saved_at)| CareerCloud {
                    revision: revision.max(0) as u64,
                    saved_at: at(saved_at),
                    export_only: (!fi_domain::GameRules::of(&c.game).line_mode(&p.mode))
                        .then_some(true),
                }),
                progress: c.progress.as_ref().map(|g| CareerProgress {
                    level: g.level,
                    credits: g.credits,
                    cars: g.cars,
                    play_seconds: Some(g.play_seconds),
                    synced_at: Some(at(g.synced_at)),
                }),
            })
            .collect(),
    }
}

/// `Access` (ACCOUNTS.md §7).
pub fn access(a: &AccessRow) -> Access {
    Access {
        tier: if a.vip { "vip" } else { "free" }.to_owned(),
        status: a.status.clone(),
        access_until: a.access_until.map(at),
        plan: a.plan.clone(),
        provider: None,
        auto_renew: a.auto_renew,
        past_due: a.past_due,
    }
}

/// `BoundDevice`.
pub fn device(d: &DeviceRow) -> BoundDevice {
    BoundDevice {
        device_id: d.device_id.to_string(),
        label: d.label.clone(),
        platform: d.platform.clone(),
        bound_at: at(d.bound_at),
        last_seen_at: at(d.last_seen_at),
    }
}

/// `{requestedAt, purgeAt}` of a pending deletion.
pub fn deletion(a: &AccountState) -> Option<Deletion> {
    a.deletion.map(|(requested, purge)| Deletion {
        requested_at: at(requested),
        purge_at: at(purge),
    })
}

/// Notices: today the pending deletion (the client branches on `kind`; the text is English).
pub fn notices(a: &AccountState) -> Vec<Notice> {
    a.deletion
        .map(|(requested, purge)| Notice {
            kind: "deletion_pending".into(),
            level: "warning".into(),
            text: format!(
                "This account will be deleted on {}; cancel the deletion to keep it.",
                at(purge)
            ),
            since: at(requested),
        })
        .into_iter()
        .collect()
}

/// `GET /launcher/v1/me`.
pub fn me(m: &Me) -> MeBody {
    MeBody {
        account_id: m.account.account_id.to_string(),
        role: m.account.role.clone(),
        profiles: m.profiles.iter().map(profile_lines).collect(),
        players: m.players.iter().map(player).collect(),
        access: access(&m.access),
        deletion: deletion(&m.account),
        devices: m.devices.iter().map(device).collect(),
        notices: notices(&m.account),
    }
}

/// The portal's `Viewer`.
pub fn viewer(m: &Me) -> Viewer {
    Viewer {
        account_id: m.account.account_id.to_string(),
        role: m.account.role.clone(),
        profiles: m.profiles.iter().map(profile).collect(),
        access: access(&m.access),
        notices: notices(&m.account),
        deletion: deletion(&m.account),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(mode: &str) -> ProfileRow {
        ProfileRow {
            profile_id: uuid::Uuid::now_v7(),
            account_id: uuid::Uuid::now_v7(),
            game: "fh2".into(),
            xuid: 2_535_000_000_000_001,
            name: "Someone".into(),
            mode: mode.into(),
            status: "active".into(),
            origin: "created".into(),
            created_at: UnixMs::from_millis(1_800_000_000_000),
            row_version: 1,
            last_played_at: None,
            rename_notice: None,
            web_listed: true,
        }
    }

    /// CR-S51 (ACCOUNTS.md §2): `/me` says per profile whether it keeps a line and needs the
    /// content scan; OD-239: every Hardcore profile, whatever the tier, never a Noob one.
    #[test]
    fn me_profiles_carry_cloud_line_and_content_required_hardcore_only() {
        let flags = |p: &Profile| (p.cloud_line, p.content_required);
        assert_eq!(
            flags(&profile_lines(&row("hardcore"))),
            (Some(true), Some(true))
        );
        assert_eq!(
            flags(&profile_lines(&row("noob"))),
            (Some(false), Some(false))
        );
        assert_eq!(flags(&profile(&row("hardcore"))), (None, None));
    }
}
