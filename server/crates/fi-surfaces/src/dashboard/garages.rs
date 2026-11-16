//! Garages and shared creations (DASHBOARD_API.md §7, PD-5; OD-168, DB-Q1): their assembly for the
//! hourly snapshot ([`super::snapshot`]) and `GET /internal/dashboard/v1/media/creation/{id}`.
//!
//! - **Garage:** the cars the game reports each profile owns (`game_profile_table` `garageCars`:
//!   `carId`, `numRaces`; `fi_db::dashboard::players::garages_in`), named from the catalog with
//!   their stock class and display PI as a stat row's car ([`super::players`]), sorted by class
//!   (D to X, R), then PI, then name. Only the catalog's cars, one entry per car (races summed), at
//!   most `MAX_GARAGE` per profile, all in the statement: the stored table's size is the client's.
//! - **Creations:** only files published to the game's storefront
//!   (`fi_db::storefront::shared_files_in`: the storefront's own "who sees a file" condition read as
//!   no one in particular, so a file taken off, an item hidden or an owner not visible never shows,
//!   plus the owner website-listed). A privately saved livery has no storefront row and never
//!   appears. A creation's mode is its owner's. Its id is opaque ([`super::RefKey::creation`]),
//!   never the storefront id.
//! - **Previews:** served live by id and `file` (the storefront id, server-only in snapshot files):
//!   the id must be `file`'s HMAC, then one primary-key read under the shown conditions (no stored
//!   id, so no migration). PNG and JPEG previews pass through;
//!   the workspace has no image decoder, so a DDS or BMP preview (none stored today) is not served
//!   and its creation says `preview: false`.

use super::players::stat_car;
use super::{Dash, DashQuery, RefKey, game, query};
use crate::views::at;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use fi_db::storefront::{self, SharedFile};
use fi_gamedb::names;
use fi_protocol::dashboard::{
    AVATAR_MAX_BYTES, CarOwner, CarPack, CatalogCar, Creation, CreationAuthor, GarageCar,
    REF_LENGTH, SnapshotCars,
};
use std::collections::HashMap;

/// The 8-byte PNG signature.
const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
/// The JPEG start-of-image marker.
const JPEG: [u8; 3] = [0xff, 0xd8, 0xff];
/// The largest preview relayed (as an avatar's).
pub const PREVIEW_MAX_BYTES: usize = AVATAR_MAX_BYTES;

/// The content type of a preview the dashboard serves as stored (its first bytes); `None` for any
/// other format (DDS and BMP would need a decoder the workspace does not have).
pub fn servable(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&PNG) {
        Some("image/png")
    } else if head.starts_with(&JPEG) {
        Some("image/jpeg")
    } else {
        None
    }
}

/// The wire creation of a shared file; the author's ref only in snapshot files (`with_ref`).
pub fn creation(f: &SharedFile, refs: &RefKey, with_ref: bool) -> Creation {
    Creation {
        id: refs.creation(f.id),
        kind: f.kind.clone(),
        title: f.title.clone(),
        description: (!f.description.is_empty()).then(|| f.description.clone()),
        car: (f.car_id > 0).then(|| stat_car(f.car_id)),
        author: CreationAuthor {
            name: f.name.clone(),
            ref_: with_ref.then(|| refs.of(f.profile_id)),
        },
        downloads: u64::try_from(f.downloads).unwrap_or(0),
        likes: u64::try_from(f.likes).unwrap_or(0),
        uses: u64::try_from(f.uses).unwrap_or(0),
        created_at: at(f.created_at),
        preview: f.preview_head.as_deref().and_then(servable).is_some(),
        file: with_ref.then_some(f.id),
    }
}

/// The game's car ids (the base catalog's 354 and the known car packs' cars, OD-231): the only
/// cars a garage shows.
pub fn catalog_cars() -> &'static [i32] {
    static CARS: std::sync::OnceLock<Vec<i32>> = std::sync::OnceLock::new();
    CARS.get_or_init(|| {
        let catalog = fi_gamedb::Catalog::fh2();
        catalog
            .cars()
            .chain(catalog.pack_cars())
            .filter_map(|c| i32::try_from(c.id).ok())
            .collect()
    })
}

/// `<game>/<mode>/cars.json` (§7.1): every catalog car, base game first, then pack cars, each by
/// class, PI and name, with the mode's listed owners (`owners`, by car id) most races first.
pub fn catalog_file(mut owners: HashMap<i32, Vec<CarOwner>>) -> SnapshotCars {
    let catalog = fi_gamedb::Catalog::fh2();
    let mut rows: Vec<(bool, i64, CatalogCar)> = catalog
        .cars()
        .chain(catalog.pack_cars())
        .filter_map(|c| {
            let id = i32::try_from(c.id).ok()?;
            let mut list = owners.remove(&id).unwrap_or_default();
            list.sort_by(|a, b| b.races.cmp(&a.races).then_with(|| a.name.cmp(&b.name)));
            let kph = (c.top_speed_mps * 3.6).round();
            Some((
                c.pack.is_some(),
                c.class_id,
                CatalogCar {
                    car: stat_car(id),
                    make: names::make_of(c.id, &c.make).to_string(),
                    year: c.year,
                    top_speed_kph: if kph.is_finite() && kph > 0.0 {
                        kph.min(f64::from(u32::MAX)) as u32
                    } else {
                        0
                    },
                    base_cost: c.base_cost,
                    pack: c.pack.as_ref().map(|p| CarPack {
                        id: p.id.clone(),
                        game: p.game.clone(),
                    }),
                    owners: list,
                },
            ))
        })
        .collect();
    rows.sort_by(|(pa, ca, a), (pb, cb, b)| {
        pa.cmp(pb)
            .then(ca.cmp(cb))
            .then(a.car.pi.cmp(&b.car.pi))
            .then_with(|| a.car.name.cmp(&b.car.name))
            .then(a.car.id.cmp(&b.car.id))
    });
    SnapshotCars {
        cars: rows.into_iter().map(|(_, _, c)| c).collect(),
    }
}

/// A garage's cars from its `(car id, races)` rows (one per car, as `garages_in` merges them; a
/// car the catalog lacks is dropped here too), sorted by class (catalog order D to X, R), then PI,
/// then name.
pub fn garage_cars(rows: &[(i64, i64)]) -> Vec<GarageCar> {
    let catalog = fi_gamedb::Catalog::fh2();
    let mut cars: Vec<(i64, GarageCar)> = rows
        .iter()
        .filter_map(|(car, races)| {
            let id = i32::try_from(*car).ok()?;
            let class = catalog.car(*car)?.class_id;
            Some((
                class,
                GarageCar {
                    car: stat_car(id),
                    races: u64::try_from(*races).unwrap_or(0),
                },
            ))
        })
        .collect();
    cars.sort_by(|(ca, a), (cb, b)| {
        ca.cmp(cb)
            .then(a.car.pi.cmp(&b.car.pi))
            .then_with(|| a.car.name.cmp(&b.car.name))
            .then(a.car.id.cmp(&b.car.id))
    });
    cars.into_iter().map(|(_, c)| c).collect()
}

fn bare_404() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// An opaque id as the URL may carry one (22 base64url characters).
fn opaque(id: &str) -> bool {
    id.len() == REF_LENGTH
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `GET /internal/dashboard/v1/media/creation/{id}?game&file`: the preview bytes (`image/png` or
/// `image/jpeg`), or a bare 404 when `file` (the storefront id the snapshot carries) is missing or
/// `id` is not its HMAC, the creation is not shown (taken off, hidden, its author not
/// website-listed), it has no preview or its format is not served. One primary-key read.
pub(super) async fn preview(
    State(s): State<Dash>,
    Path(id): Path<String>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Response {
    let Ok(q) = query(q) else {
        return bare_404();
    };
    let Some(refs) = s.refs.clone() else {
        return bare_404();
    };
    // `file`: the storefront id the snapshot carries (server-only); the id must be its HMAC.
    let file = q
        .file
        .as_deref()
        .and_then(|f| f.parse::<i32>().ok())
        .filter(|f| *f > 0);
    let Some(file) = file else {
        return bare_404();
    };
    if !opaque(&id) || !fi_auth::ct_eq(refs.creation(file).as_bytes(), id.as_bytes()) {
        return bare_404();
    }
    let game = game(&q);
    let Ok(mut conn) = s.pool.acquire().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // One primary-key read under the shown conditions.
    let Ok(Some(bytes)) = storefront::shared_preview_in(&mut conn, &game, file).await else {
        return bare_404();
    };
    match servable(&bytes) {
        Some(kind) if bytes.len() <= PREVIEW_MAX_BYTES => {
            ([(CONTENT_TYPE, HeaderValue::from_static(kind))], bytes).into_response()
        }
        _ => bare_404(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_ids_are_opaque_stable_and_keyed() {
        let a = RefKey::new("k").unwrap();
        let id = a.creation(42);
        assert_eq!(id.len(), REF_LENGTH);
        assert!(opaque(&id));
        assert_eq!(id, a.creation(42));
        assert_ne!(id, a.creation(43));
        assert_ne!(id, RefKey::new("other").unwrap().creation(42));
        assert!(!id.contains("42"));
        // Not a profile ref of the same number's text either (another domain).
        // The definition (v2): HMAC under the creation sub-key, never the raw key.
        let creation_key = fi_auth::hmac_sha256(b"k", b"fi-dashboard-subkey-v2:creation");
        let mac = fi_auth::hmac_sha256(&creation_key, b"fi-dashboard-creation-v2:42");
        assert_eq!(id, fi_auth::b64::url_encode(&mac)[..22]);
        let v1 = fi_auth::hmac_sha256(b"k", b"fi-dashboard-creation-v1:42");
        assert_ne!(id, fi_auth::b64::url_encode(&v1)[..22]);
        for bad in ["", "short", &"a".repeat(23), &format!("{}!", &id[..21])] {
            assert!(!opaque(bad), "{bad}");
        }
    }

    #[test]
    fn only_png_and_jpeg_are_served() {
        assert_eq!(servable(&PNG), Some("image/png"));
        assert_eq!(servable(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(servable(b"DDS |\0\0\0"), None);
        assert_eq!(servable(b"BM6\0\0\0"), None);
        assert_eq!(servable(b""), None);
    }

    #[test]
    fn garages_sort_by_class_then_pi_then_name_and_show_catalog_cars_only() {
        // 247: Toyota 2000 GT (D, 330); 1652: Caterham R500; 30000: not in the catalog.
        let cars = garage_cars(&[(30_000, 1), (1652, 4), (247, 9)]);
        let ids: Vec<i32> = cars.iter().map(|c| c.car.id).collect();
        assert_eq!(ids, vec![247, 1652], "unknown cars dropped");
        assert_eq!((cars[0].car.class.as_str(), cars[0].races), ("D", 9));
        let catalog = fi_gamedb::Catalog::fh2();
        let known = catalog.cars().count() + catalog.pack_cars().count();
        assert_eq!(catalog.cars().count(), 354);
        assert_eq!(catalog_cars().len(), known, "base and pack cars");
        assert!(catalog_cars().contains(&247) && !catalog_cars().contains(&30_000));
        let all: Vec<(i64, i64)> = catalog_cars().iter().map(|c| (i64::from(*c), 1)).collect();
        let shown = garage_cars(&all);
        assert_eq!(shown.len(), known);
        let classes: Vec<i64> = shown
            .iter()
            .map(|c| {
                fi_gamedb::Catalog::fh2()
                    .car(i64::from(c.car.id))
                    .unwrap()
                    .class_id
            })
            .collect();
        assert!(classes.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn the_cars_file_lists_every_car_base_first_with_owners_most_races_first() {
        let owner = |name: &str, races| CarOwner {
            name: name.into(),
            races,
            ref_: Some("r".into()),
        };
        let file = catalog_file(HashMap::from([(
            247,
            vec![owner("Bee", 2), owner("Ann", 2), owner("Cid", 9)],
        )]));
        let catalog = fi_gamedb::Catalog::fh2();
        assert_eq!(
            file.cars.len(),
            catalog.cars().count() + catalog.pack_cars().count()
        );
        let first_pack = file.cars.iter().position(|c| c.pack.is_some());
        if let Some(i) = first_pack {
            assert!(
                file.cars[i..].iter().all(|c| c.pack.is_some()),
                "packs last"
            );
        }
        let toyota = file.cars.iter().find(|c| c.car.id == 247).unwrap();
        let names: Vec<&str> = toyota.owners.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["Cid", "Ann", "Bee"]);
        assert!(toyota.top_speed_kph > 0 && toyota.pack.is_none());
        assert!(
            file.cars
                .iter()
                .filter(|c| c.car.id != 247)
                .all(|c| c.owners.is_empty())
        );
    }
}
