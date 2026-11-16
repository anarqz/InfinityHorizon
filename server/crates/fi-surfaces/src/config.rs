//! Identity-plane configuration of the launcher and portal surfaces (ACCOUNTS.md §4, §5;
//! docs/server/ARCHITECTURE.md §8). Read once at startup; a revision that could reach real GCP or
//! prod with local settings refuses to start:
//!
//! - emulator mode (`FIREBASE_AUTH_EMULATOR_HOST` set) needs a `demo-` project in
//!   `FI_IDP_PROJECT_ID` and `GCP_PROJECT_ID`, and is refused with `APP_ENV=prod` or `FI_ENV=prod`;
//! - `/launcher/v1/info`'s `idp` (OD-43): `FI_IDP_API_KEY` is required outside emulator mode (and
//!   `demo-api-key` is refused in prod); `FI_IDP_EMULATOR_PUBLIC_URL` only in emulator mode;
//! - the signing keys, the pepper and the portal assertion secret are required where the deployable
//!   uses them ([`IdentityNeeds`]), and refused in prod when they are the well-known local values of
//!   docker-compose.yml / .env.example;
//! - launcher access tokens (ADR 0014 §1, PV1-Q2): `FI_PLATFORM_SIGNING_KEYS` on the deployable that
//!   signs them (fi-core), `FI_PLATFORM_VERIFY_KEYS` (public keys) on the ones that only verify
//!   (fi-gate); locally, with neither set, the one ring `FI_TOKEN_SIGNING_KEYS` signs access tokens
//!   and tickets alike (compose); in prod one of the two platform rings is required;
//! - the portal assertion is `local-shared-secret` (local only) or `google-oidc` (not implemented
//!   yet: startup fails rather than accept unverified portal calls);
//! - outside the emulator, rotating Google JWKS are fetched on demand with strict bounded expiry;
//!   static `FI_IDP_JWKS` is accepted only for local tests, never production;
//! - `FI_AUTH_BACKEND` (ADR 0023 §9, OD-122): `firebase` (default; everything above) or `local`,
//!   the instance's own e-mail and password provider: no `FI_IDP_*` but `FI_IDP_PROVIDERS` is
//!   read, `FI_LOCAL_IDP_SIGNING_KEYS` signs its ID tokens (refused in prod when it is the
//!   well-known local value or shares a key with another ring), and `local` is refused with
//!   `APP_ENV`/`FI_ENV=prod` unless `FI_AUTH_LOCAL_IN_PROD=true`. A `local` instance also reads its
//!   mail and Firebase-import settings ([`crate::mail`]; `smtp` required in prod).
//!
//! - `FI_PUBLISH_TRUST_KEYS` (CR-S49, OD-122 local-first; the deployable that serves `/info`):
//!   `false` (default) or `true`, anything else fails startup. `true` publishes `/info`'s `trust`:
//!   the public halves of the launch-ticket ring (a deployable without it refuses to start) and of
//!   the relay-grant ring, read from `FI_RELAY_GRANT_VERIFY_KEYS`, else derived from
//!   `FI_RELAY_GRANT_SIGNING_KEYS`, else empty (no worlds).
//!
//! [`PlatformConfig`] adds what play sessions, saves, the game plane and the internal surface read
//! (local environment v0): pool routing, `FI_POOLS_JSON`, the lease TTL knob, `FI_LOCAL_ADMIN`,
//! the instance id, the session-token key ring and the saves bucket.

use fi_auth::access::{self, AccessError, Audience};
use fi_auth::idtoken::{self, Mode, StaticKeys, Verifier};
use fi_auth::keys::{self, SigningKeys};
use fi_auth::localidp::LocalIdp;
use fi_auth::pepper::Pepper;
use fi_auth::relay_grant::PublicKeys;
use fi_db::admission::PoolSpec;
use fi_domain::{PoolName, PoolRouting};
use fi_protocol::accounts::{
    CheckpointPolicy, IdpConfig, IdpKind, ServerInfo, ServiceMap, TrustKeys,
};
use fi_runtime::config::{self, ConfigError};
use fi_saves::fresh::{FreshStartLimits, HardcoreStart};
use fi_storage::{Blobs, GcsBlobStore};
use std::sync::Arc;

/// How the portal BFF proves itself (`X-Fi-Client-Assertion`, ACCOUNTS.md §6).
#[derive(Clone)]
pub enum PortalAssertion {
    /// Local stacks only: the header carries a shared secret (`FI_PORTAL_ASSERTION_SECRET`).
    LocalSharedSecret(Vec<u8>),
}

impl std::fmt::Debug for PortalAssertion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PortalAssertion::LocalSharedSecret(***)")
    }
}

/// The launcher access-token ring of a deployable (ADR 0014 §1).
#[derive(Clone, Debug)]
pub enum AccessKeys {
    /// Signs and verifies: fi-core (`FI_PLATFORM_SIGNING_KEYS`) and the local single ring.
    Signing(SigningKeys),
    /// Verifies only: the public keys of core's ring (`FI_PLATFORM_VERIFY_KEYS`; fi-gate).
    Verify(PublicKeys),
}

impl AccessKeys {
    /// Verifies a launcher or portal access token.
    pub fn verify(
        &self,
        token: &str,
        audience: Audience,
        now: i64,
    ) -> Result<access::Verified, AccessError> {
        match self {
            AccessKeys::Signing(ring) => access::verify(ring, token, audience, now),
            AccessKeys::Verify(keys) => {
                crate::game::ticket::verify_access(keys, token, audience, now)
            }
        }
    }

    /// The signing ring, when this deployable signs access tokens.
    pub fn signing(&self) -> Option<&SigningKeys> {
        match self {
            AccessKeys::Signing(ring) => Some(ring),
            AccessKeys::Verify(_) => None,
        }
    }

    /// The public keys (what verify-only deployables are configured with).
    pub fn public(&self) -> PublicKeys {
        match self {
            AccessKeys::Signing(ring) => PublicKeys::of(ring),
            AccessKeys::Verify(keys) => keys.clone(),
        }
    }
}

/// What a deployable's identity configuration must hold; anything else is not read, so a secret
/// it does not use is never required on it (ADR 0014 key placement).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdentityNeeds {
    /// The `/portal/v1` BFF surface (`FI_PORTAL_ASSERTION_MODE`).
    pub portal: bool,
    /// Verifies Identity Platform ID tokens (`FI_IDP_PROJECT_ID`, the emulator or `FI_IDP_JWKS`).
    pub id_tokens: bool,
    /// Signs launcher access tokens (`FI_PLATFORM_SIGNING_KEYS`); otherwise verifies them only.
    pub sign_access: bool,
    /// Signs launch tickets (`FI_LAUNCH_TICKET_SIGNING_KEYS`).
    pub launch_tickets: bool,
    /// Hashes site keys and rate-limit subjects (`FI_SITE_KEY_PEPPER`).
    pub pepper: bool,
    /// Serves `GET /launcher/v1/info` (`FI_REALM`, required in prod).
    pub info: bool,
}

impl IdentityNeeds {
    /// Everything: the local environment v0's fi-api, which serves every surface.
    pub const fn combined(portal: bool) -> Self {
        Self {
            portal,
            id_tokens: true,
            sign_access: true,
            launch_tickets: true,
            pepper: true,
            info: true,
        }
    }

    /// fi-core: sign-in, accounts, info; no play sessions (no launch tickets).
    pub const CORE: Self = Self {
        portal: false,
        id_tokens: true,
        sign_access: true,
        launch_tickets: false,
        pepper: true,
        info: true,
    };

    /// fi-gate: verifies access tokens and signs launch tickets; no sign-in, no site keys.
    pub const GATE: Self = Self {
        portal: false,
        id_tokens: false,
        sign_access: false,
        launch_tickets: true,
        pepper: false,
        info: false,
    };
}

/// Everything the launcher and portal surfaces need.
#[derive(Clone, Debug)]
pub struct IdentityConfig {
    /// Identity Platform ID-token verification (deployables that sign in on a `firebase`
    /// instance).
    pub verifier: Option<Verifier>,
    /// The instance's own provider (`FI_AUTH_BACKEND=local`, deployables that sign in): signs and
    /// verifies its ID tokens; `Some` exactly when the `local` routes are served.
    pub local_idp: Option<LocalIdp>,
    /// The `local` provider's mail ([`crate::mail`], CR-S53): `Some` exactly with `local_idp`.
    pub mail: Option<crate::mail::MailConfig>,
    /// The Firebase project's password hash parameters for imported users (`local` only).
    pub firebase_hash: Option<fi_auth::firebase_scrypt::HashConfig>,
    /// Rotating Google keys; absent for emulator/static local fixtures.
    pub(crate) jwks: Option<Arc<crate::jwks::RotatingVerifier>>,
    /// Access-token ring ([`AccessKeys`]).
    pub access_keys: AccessKeys,
    /// Launch-ticket key ring (`FI_LAUNCH_TICKET_SIGNING_KEYS`; deployables that admit): checked at
    /// startup so a prod revision never starts with the local key; play sessions sign their
    /// tickets with it.
    pub ticket_keys: Option<SigningKeys>,
    /// The site-key pepper (`FI_SITE_KEY_PEPPER`; deployables that sign in).
    pub pepper: Option<Pepper>,
    /// The portal's assertion check (portal surface only).
    pub portal_assertion: Option<PortalAssertion>,
    /// `GET /launcher/v1/info`.
    pub info: ServerInfo,
    /// Multiplier for local sign-in and
    /// account-creation limits ([`fi_db::limits`]) of a local stack, where every launcher behind
    /// the SSH tunnel or the ingress shares one client network (register CR-L24).
    pub local_limit_scale: i32,
    /// The per-account request buckets of `/launcher/v1` (ACCOUNTS.md §9 "everything else",
    /// OD-215).
    pub accounts: Arc<AccountLimiter>,
}

/// The per-account buckets of `/launcher/v1` (OD-215), shared by every extractor that
/// authenticates a launcher access token ([`crate::extract::LauncherAuth`] and the save line's
/// ticket-config branch). Derived, per-instance and bounded (ADR 0903 §3): a lost bucket only
/// starts full again.
#[derive(Debug, Default)]
pub struct AccountLimiter {
    /// The limits.
    pub rates: AccountRates,
    /// The buckets.
    pub buckets: fi_runtime::ratelimit::TokenBuckets,
}

impl AccountLimiter {
    /// The contract's limits times `scale`.
    pub fn scaled(scale: u32) -> Self {
        Self {
            rates: AccountRates::scaled(scale),
            buckets: Default::default(),
        }
    }

    /// `FI_LAUNCHER_ACCOUNT_SCALE` (1-1000, default 1; refused above 1 in prod).
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        let scale: u32 = number(env, "FI_LAUNCHER_ACCOUNT_SCALE", 1)?;
        if !(1..=1000).contains(&scale) {
            return Err(invalid("FI_LAUNCHER_ACCOUNT_SCALE", "1 to 1000"));
        }
        if is_prod(env) && scale != 1 {
            return Err(invalid(
                "FI_LAUNCHER_ACCOUNT_SCALE",
                "per-account rate scaling is refused when APP_ENV or FI_ENV is prod",
            ));
        }
        Ok(Self::scaled(scale))
    }
}

impl Default for AccountRates {
    fn default() -> Self {
        Self::scaled(1)
    }
}

/// The per-account limits of every authenticated `/launcher/v1` request (ACCOUNTS.md §9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountRates {
    /// Every request: 10 per second with a burst of 50.
    pub any: fi_runtime::ratelimit::Rate,
    /// Writes (every method but `GET` and `HEAD`) also take one from this: 60 per minute.
    pub write: fi_runtime::ratelimit::Rate,
}

impl AccountRates {
    /// The contract's numbers, each multiplied by `scale` (`FI_LAUNCHER_ACCOUNT_SCALE`, 1 in prod).
    pub fn scaled(scale: u32) -> Self {
        use fi_runtime::ratelimit::Rate;
        use std::time::Duration;
        Self {
            any: Rate {
                capacity: 50 * scale,
                period: Duration::from_secs(5),
            },
            write: Rate::per_minute(60 * scale),
        }
    }
}

fn not_configured(what: &str) -> crate::errors::ApiError {
    tracing::error!(what, "not configured on this deployment");
    crate::errors::ApiError::new(
        fi_protocol::ErrorCode::ServiceUnavailable,
        "service unavailable",
    )
}

impl IdentityConfig {
    /// Whether this instance signs in with its own provider (`FI_AUTH_BACKEND=local`).
    pub fn is_local(&self) -> bool {
        self.local_idp.is_some()
    }

    /// The ID-token verifier, or `SERVICE.UNAVAILABLE` where none is configured.
    pub fn verifier(&self) -> Result<&Verifier, crate::errors::ApiError> {
        self.verifier
            .as_ref()
            .ok_or_else(|| not_configured("the ID-token verifier"))
    }

    /// The pepper, or `SERVICE.UNAVAILABLE` where none is configured.
    pub fn pepper(&self) -> Result<&Pepper, crate::errors::ApiError> {
        self.pepper
            .as_ref()
            .ok_or_else(|| not_configured("the site-key pepper"))
    }

    /// The launch-ticket ring, or `SERVICE.UNAVAILABLE` where none is configured.
    pub fn ticket_keys(&self) -> Result<&SigningKeys, crate::errors::ApiError> {
        self.ticket_keys
            .as_ref()
            .ok_or_else(|| not_configured("the launch-ticket keys"))
    }

    /// The access-token signing ring, or `SERVICE.UNAVAILABLE` on a verify-only deployable.
    pub fn signing_keys(&self) -> Result<&SigningKeys, crate::errors::ApiError> {
        self.access_keys
            .signing()
            .ok_or_else(|| not_configured("the access-token signing keys"))
    }
}

/// The environment, read once (tests pass their own).
pub trait Env {
    /// A variable, `None` when unset or blank.
    fn get(&self, name: &'static str) -> Option<String>;
}

/// The process environment.
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &'static str) -> Option<String> {
        config::optional(name)
    }
}

/// `inner` backed by the keys directory of a standalone instance (`FI_KEYS_DIR`, ADR 0023 §9): a
/// variable `inner` does not set is read from `<FI_KEYS_DIR>/<NAME>` (`fi-platform keys init`
/// writes one file per secret, mode 0600). A set variable always wins, so the instance's `.env`
/// can still override a generated key. No `FI_KEYS_DIR`: exactly `inner`.
pub struct KeysDirEnv<'a> {
    inner: &'a dyn Env,
}

impl<'a> KeysDirEnv<'a> {
    /// `inner` with its keys directory.
    pub fn new(inner: &'a dyn Env) -> Self {
        Self { inner }
    }
}

impl Env for KeysDirEnv<'_> {
    fn get(&self, name: &'static str) -> Option<String> {
        self.inner.get(name).or_else(|| {
            let dir = self.inner.get(config::KEYS_DIR)?;
            config::key_file(std::path::Path::new(dir.trim()), name)
        })
    }
}

impl<const N: usize> Env for [(&'static str, &'static str); N] {
    fn get(&self, name: &'static str) -> Option<String> {
        self.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string())
            .filter(|v| !v.trim().is_empty())
    }
}

impl Env for Vec<(&'static str, String)> {
    fn get(&self, name: &'static str) -> Option<String> {
        self.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty())
    }
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

fn required(env: &dyn Env, name: &'static str) -> Result<String, ConfigError> {
    env.get(name).ok_or(ConfigError::Missing(name))
}

fn number<T: std::str::FromStr>(
    env: &dyn Env,
    name: &'static str,
    default: T,
) -> Result<T, ConfigError> {
    match env.get(name) {
        None => Ok(default),
        Some(v) => v.trim().parse().map_err(|_| invalid(name, "not a number")),
    }
}

/// Whether the environment names production (`APP_ENV=prod` or `FI_ENV=prod`).
pub fn is_prod(env: &dyn Env) -> bool {
    ["APP_ENV", "FI_ENV"].iter().any(|n| {
        env.get(n)
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("prod"))
    })
}

/// `FI_EDGE_SECRET`, the origin lock of a public deployable (ADR 0903 §1): optional locally,
/// required when `APP_ENV` or `FI_ENV` is prod, so no prod origin answers without the edge.
pub fn edge_secret(
    env: &dyn Env,
) -> Result<Option<Arc<fi_runtime::http::EdgeSecret>>, ConfigError> {
    let edge = env
        .get("FI_EDGE_SECRET")
        .map(|v| fi_runtime::http::EdgeSecret::parse(&v))
        .transpose()?;
    if is_prod(env) && edge.is_none() {
        return Err(ConfigError::Missing("FI_EDGE_SECRET"));
    }
    Ok(edge.map(Arc::new))
}

/// An `http(s)://host[:port]` origin without a path, from `name` (`None` when unset).
fn origin(env: &dyn Env, name: &'static str) -> Result<Option<String>, ConfigError> {
    let Some(v) = env.get(name) else {
        return Ok(None);
    };
    let v = v.trim().trim_end_matches('/').to_owned();
    let rest = v
        .strip_prefix("https://")
        .or_else(|| v.strip_prefix("http://"))
        .unwrap_or_default();
    if rest.is_empty()
        || rest.contains(['/', '?', '#', '@', ' '])
        || !rest.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(invalid(
            name,
            "an http(s)://host[:port] origin without a path",
        ));
    }
    Ok(Some(v))
}

/// The origins `GET /launcher/v1/info` names (CORE_API.md §2.1): `FI_PUBLIC_CORE_URL`,
/// `FI_PUBLIC_GATE_URL`, `FI_PUBLIC_SAVES_URL`, `FI_PUBLIC_MATCHMAKING_URL`; each an
/// `http(s)://host[:port]` origin without a path, empty when unset.
pub fn service_map(env: &dyn Env) -> Result<ServiceMap, ConfigError> {
    let origin = |name| origin(env, name).map(Option::unwrap_or_default);
    Ok(ServiceMap {
        core: origin("FI_PUBLIC_CORE_URL")?,
        gate: origin("FI_PUBLIC_GATE_URL")?,
        saves: origin("FI_PUBLIC_SAVES_URL")?,
        matchmaking: origin("FI_PUBLIC_MATCHMAKING_URL")?,
    })
}

/// The sign-in backend of an instance (`FI_AUTH_BACKEND`, ADR 0023 §9).
pub fn auth_backend(env: &dyn Env) -> Result<IdpKind, ConfigError> {
    let kind = match env.get("FI_AUTH_BACKEND").as_deref().map(str::trim) {
        None | Some("firebase") => IdpKind::Firebase,
        Some("local") => IdpKind::Local,
        Some(_) => return Err(invalid("FI_AUTH_BACKEND", "firebase or local")),
    };
    let allowed_in_prod = match env.get("FI_AUTH_LOCAL_IN_PROD").as_deref().map(str::trim) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(invalid("FI_AUTH_LOCAL_IN_PROD", "true or false")),
    };
    if kind == IdpKind::Local && is_prod(env) && !allowed_in_prod {
        return Err(invalid(
            "FI_AUTH_BACKEND",
            "local is refused when APP_ENV or FI_ENV is prod unless FI_AUTH_LOCAL_IN_PROD=true",
        ));
    }
    Ok(kind)
}

/// `FI_IDP_PROVIDERS`: the allowlist of sign-in providers, `password` by default (OD-42).
fn idp_providers(env: &dyn Env) -> Vec<String> {
    env.get("FI_IDP_PROVIDERS")
        .unwrap_or_else(|| "password".into())
        .split(',')
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The Web API key the Auth emulator takes locally (it accepts any; the SPA sends the same).
pub const LOCAL_IDP_API_KEY: &str = "demo-api-key";

/// `ServerInfo.idp` (ACCOUNTS.md §5, OD-43), read by the deployables that serve `/info`:
/// - `FI_IDP_API_KEY`, the public Web API key: required outside emulator mode (so in prod),
///   `demo-api-key` by default in emulator mode; prod refuses that local value;
/// - `FI_IDP_EMULATOR_PUBLIC_URL`, the Auth emulator's origin as devices reach it
///   (`http://127.0.0.1:9399`, the LAN address in LAN mode): emulator mode only, refused in prod,
///   never derived from `FIREBASE_AUTH_EMULATOR_HOST` (compose's internal `auth-emulator:9399`).
///   Unset: `/info` carries no `emulatorUrl`.
fn idp_config(env: &dyn Env, prod: bool) -> Result<IdpConfig, ConfigError> {
    let emulator = env.get("FIREBASE_AUTH_EMULATOR_HOST").is_some();
    let api_key = match env.get("FI_IDP_API_KEY") {
        Some(k) => k.trim().to_owned(),
        None if emulator && !prod => LOCAL_IDP_API_KEY.to_owned(),
        None => return Err(ConfigError::Missing("FI_IDP_API_KEY")),
    };
    if api_key.len() > 128
        || !api_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid(
            "FI_IDP_API_KEY",
            "a Web API key (letters, digits, - and _)",
        ));
    }
    if prod && api_key == LOCAL_IDP_API_KEY {
        return Err(invalid(
            "FI_IDP_API_KEY",
            "the local emulator key is refused when APP_ENV or FI_ENV is prod",
        ));
    }
    let emulator_url = origin(env, "FI_IDP_EMULATOR_PUBLIC_URL")?;
    if emulator_url.is_some() {
        if prod {
            return Err(invalid(
                "FI_IDP_EMULATOR_PUBLIC_URL",
                "the Auth emulator is refused when APP_ENV or FI_ENV is prod",
            ));
        }
        if !emulator {
            return Err(invalid(
                "FI_IDP_EMULATOR_PUBLIC_URL",
                "only in emulator mode (FIREBASE_AUTH_EMULATOR_HOST)",
            ));
        }
    }
    Ok(IdpConfig {
        kind: fi_protocol::accounts::IdpKind::Firebase,
        api_key,
        project_id: required(env, "FI_IDP_PROJECT_ID")?,
        emulator_url,
    })
}

/// `kid:hex` of each `(kid, raw 32-byte Ed25519 public key)` (ACCOUNTS.md §5 `trust`).
fn kid_hex(keys: Vec<(String, [u8; 32])>) -> Vec<String> {
    keys.into_iter()
        .map(|(kid, key)| {
            let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
            format!("{kid}:{hex}")
        })
        .collect()
}

/// `/info`'s `trust` (CR-S49): `None` unless `FI_PUBLISH_TRUST_KEYS=true`.
fn trust_keys(
    env: &dyn Env,
    ticket_keys: Option<&SigningKeys>,
    key_ring: &dyn Fn(&'static str, &str) -> Result<SigningKeys, ConfigError>,
) -> Result<Option<TrustKeys>, ConfigError> {
    match env.get("FI_PUBLISH_TRUST_KEYS").as_deref() {
        None | Some("false") => return Ok(None),
        Some("true") => {}
        Some(_) => return Err(invalid("FI_PUBLISH_TRUST_KEYS", "true or false")),
    }
    let ticket = ticket_keys.ok_or_else(|| {
        invalid(
            "FI_PUBLISH_TRUST_KEYS",
            "this deployable signs no launch tickets, so it has none of their keys to publish",
        )
    })?;
    let relay = match (
        env.get("FI_RELAY_GRANT_VERIFY_KEYS"),
        env.get("FI_RELAY_GRANT_SIGNING_KEYS"),
    ) {
        (Some(text), _) => PublicKeys::parse("FI_RELAY_GRANT_VERIFY_KEYS", &text)
            .map_err(|e| invalid("FI_RELAY_GRANT_VERIFY_KEYS", e.reason))?
            .public_keys(),
        (None, Some(text)) => key_ring("FI_RELAY_GRANT_SIGNING_KEYS", &text)?.public_keys(),
        // A deployment without worlds: nothing signs relay grants.
        (None, None) => Vec::new(),
    };
    Ok(Some(TrustKeys {
        launch_ticket: kid_hex(ticket.public_keys()),
        relay_grant: kid_hex(relay),
    }))
}

impl IdentityConfig {
    /// Reads and checks the identity settings of the combined fi-api (every need); `portal` says
    /// whether the portal surface is served.
    pub fn from_env(env: &dyn Env, portal: bool) -> Result<Self, ConfigError> {
        Self::from_env_for(env, IdentityNeeds::combined(portal))
    }

    /// Reads and checks what `needs` names; nothing else is read.
    pub fn from_env_for(env: &dyn Env, needs: IdentityNeeds) -> Result<Self, ConfigError> {
        let prod = is_prod(env);
        let portal = needs.portal;
        let backend = auth_backend(env)?;
        let local = backend == IdpKind::Local;
        let (verifier, jwks) = if needs.id_tokens && !local {
            let (verifier, remote) = id_token_verifier(env, prod)?;
            (Some(verifier), remote)
        } else {
            (None, None)
        };
        let key_ring = |name: &'static str, text: &str| -> Result<SigningKeys, ConfigError> {
            let ring = SigningKeys::parse(name, text).map_err(|e| invalid(name, e.reason))?;
            if prod && ring.has_local_dev_key() {
                return Err(invalid(
                    name,
                    "a well-known local development key is refused in prod",
                ));
            }
            Ok(ring)
        };
        let access_keys = match (
            env.get("FI_PLATFORM_SIGNING_KEYS"),
            env.get("FI_PLATFORM_VERIFY_KEYS"),
        ) {
            (Some(_), _) if !needs.sign_access => {
                return Err(invalid(
                    "FI_PLATFORM_SIGNING_KEYS",
                    "this deployable verifies access tokens and never signs them: configure FI_PLATFORM_VERIFY_KEYS (public keys) only",
                ));
            }
            (Some(text), _) => AccessKeys::Signing(key_ring("FI_PLATFORM_SIGNING_KEYS", &text)?),
            (None, Some(_)) if needs.sign_access => {
                return Err(ConfigError::Missing("FI_PLATFORM_SIGNING_KEYS"));
            }
            (None, Some(text)) => AccessKeys::Verify(
                PublicKeys::parse("FI_PLATFORM_VERIFY_KEYS", &text)
                    .map_err(|e| invalid("FI_PLATFORM_VERIFY_KEYS", e.reason))?,
            ),
            (None, None) if prod => {
                return Err(ConfigError::Missing(if needs.sign_access {
                    "FI_PLATFORM_SIGNING_KEYS"
                } else {
                    "FI_PLATFORM_VERIFY_KEYS"
                }));
            }
            // Locally one ring signs access tokens and game tickets (compose, the v0 fi-api).
            (None, None) => AccessKeys::Signing(key_ring(
                "FI_TOKEN_SIGNING_KEYS",
                &required(env, "FI_TOKEN_SIGNING_KEYS")?,
            )?),
        };
        // In prod the ticket ring lives on fi-gate only (ADR 0014): a deployable that signs access
        // tokens but admits nobody refuses it, so a mis-wired secret cannot reach it.
        if prod
            && needs.sign_access
            && !needs.launch_tickets
            && env.get("FI_TOKEN_SIGNING_KEYS").is_some()
        {
            return Err(invalid(
                "FI_TOKEN_SIGNING_KEYS",
                "the game ticket's ring belongs to fi-gate only (ADR 0014)",
            ));
        }
        let ticket_keys = if needs.launch_tickets {
            Some(key_ring(
                "FI_LAUNCH_TICKET_SIGNING_KEYS",
                &required(env, "FI_LAUNCH_TICKET_SIGNING_KEYS")?,
            )?)
        } else {
            None
        };
        let pepper = if needs.pepper {
            let pepper =
                keys::parse_pepper("FI_SITE_KEY_PEPPER", &required(env, "FI_SITE_KEY_PEPPER")?)
                    .map_err(|e| invalid("FI_SITE_KEY_PEPPER", e.reason))?;
            if prod && pepper.secrets().any(|s| keys::is_local_dev_secret(s)) {
                return Err(invalid(
                    "FI_SITE_KEY_PEPPER",
                    "the well-known local development pepper is refused in prod",
                ));
            }
            Some(pepper)
        } else {
            None
        };

        let portal_assertion = if portal {
            match env
                .get("FI_PORTAL_ASSERTION_MODE")
                .as_deref()
                .map(str::trim)
            {
                Some("local-shared-secret") => {
                    if prod {
                        return Err(invalid(
                            "FI_PORTAL_ASSERTION_MODE",
                            "local-shared-secret is refused in prod",
                        ));
                    }
                    let secret = required(env, "FI_PORTAL_ASSERTION_SECRET")?;
                    if secret.len() < 32 {
                        return Err(invalid(
                            "FI_PORTAL_ASSERTION_SECRET",
                            "at least 32 characters",
                        ));
                    }
                    Some(PortalAssertion::LocalSharedSecret(secret.into_bytes()))
                }
                Some("google-oidc") => {
                    return Err(invalid(
                        "FI_PORTAL_ASSERTION_MODE",
                        "google-oidc is not implemented yet (SER-1.1 follow-up)",
                    ));
                }
                Some(_) => {
                    return Err(invalid(
                        "FI_PORTAL_ASSERTION_MODE",
                        "local-shared-secret or google-oidc",
                    ));
                }
                None => return Err(ConfigError::Missing("FI_PORTAL_ASSERTION_MODE")),
            }
        } else {
            None
        };

        let realm = match env.get("FI_REALM") {
            Some(r) => r,
            None if prod && needs.info => return Err(ConfigError::Missing("FI_REALM")),
            None => "forzainfinity-local".into(),
        };
        let allowed_car_db: Vec<String> = env
            .get("FI_ALLOWED_CAR_DB")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if allowed_car_db
            .iter()
            .any(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid(
                "FI_ALLOWED_CAR_DB",
                "SHA-256 hex digests, comma separated",
            ));
        }
        let version = |name: &'static str| env.get(name).unwrap_or_else(|| "0.1.0".into());
        let (min_launcher, latest_launcher) = launcher_versions_from_env(env)?;
        let trust = if needs.info {
            trust_keys(env, ticket_keys.as_ref(), &key_ring)?
        } else {
            None
        };
        let info = ServerInfo {
            api_version: 1,
            registration_enabled: match env.get("FI_REGISTRATION_ENABLED").as_deref() {
                None | Some("true") => true,
                Some("false") => false,
                Some(_) => return Err(invalid("FI_REGISTRATION_ENABLED", "true or false")),
            },
            realm,
            min_launcher,
            min_hook: version("FI_MIN_HOOK"),
            latest_launcher,
            latest_hook: version("FI_LATEST_HOOK"),
            allowed_car_db,
            checkpoints: CheckpointPolicy {
                enabled: env
                    .get("FI_CHECKPOINTS_ENABLED")
                    .map(|v| v.trim() != "false")
                    .unwrap_or(true),
                min_interval_secs: number(env, "FI_CHECKPOINT_MIN_INTERVAL_SECS", 300)?,
                max_commits_per_hour: number(env, "FI_CHECKPOINT_MAX_PER_HOUR", 30)?,
            },
            motd: env.get("FI_MOTD"),
            services: service_map(env)?,
            // Filled per request from the `game` table (launcher::auth::info).
            games: Vec::new(),
            idp: if needs.info && local {
                IdpConfig {
                    kind: IdpKind::Local,
                    ..IdpConfig::default()
                }
            } else if needs.info {
                idp_config(env, prod)?
            } else {
                IdpConfig::default()
            },
            trust,
            // CR-S51: always present; OD-239 (CR-S111): Hardcore only, a hard rule.
            saves: Some(fi_protocol::accounts::SavesPolicy {
                hardcore_from_profile: hardcore_from_profile_from_env(env)?,
            }),
        };
        let local_idp = if needs.id_tokens && local {
            let ring = key_ring(
                "FI_LOCAL_IDP_SIGNING_KEYS",
                &required(env, "FI_LOCAL_IDP_SIGNING_KEYS")?,
            )?;
            // Its own keys: a ring that also signs access tokens or tickets is refused.
            let others = access_keys
                .signing()
                .into_iter()
                .chain(ticket_keys.as_ref())
                .flat_map(SigningKeys::public_keys)
                .map(|(_, key)| key)
                .collect::<Vec<_>>();
            if ring
                .public_keys()
                .iter()
                .any(|(_, key)| others.contains(key))
            {
                return Err(invalid(
                    "FI_LOCAL_IDP_SIGNING_KEYS",
                    "shares a key with another signing ring: give the local IdP its own keys",
                ));
            }
            let providers = idp_providers(env);
            // The local provider's tokens say `password`: an allowlist without it would start
            // an instance that refuses every one of its own sign-ins.
            if !providers.iter().any(|p| p == "password") {
                return Err(invalid(
                    "FI_IDP_PROVIDERS",
                    "a local instance signs in with password: keep it in the list",
                ));
            }
            Some(
                LocalIdp::new(ring, &info.services.core, &info.realm, &providers)
                    .map_err(|_| invalid("FI_IDP_PROVIDERS", "no provider"))?,
            )
        } else {
            None
        };
        let (mail, firebase_hash) = if local_idp.is_some() {
            (
                Some(crate::mail::mail_from_env(env, &info.services.core)?),
                crate::mail::firebase_hash_from_env(env)?,
            )
        } else {
            (None, None)
        };
        let local_limit_scale = number(env, "FI_LOCAL_LIMIT_SCALE", 1)?;
        if !(1..=1000).contains(&local_limit_scale) {
            return Err(invalid("FI_LOCAL_LIMIT_SCALE", "1 to 1000"));
        }
        if prod && local_limit_scale != 1 {
            return Err(invalid(
                "FI_LOCAL_LIMIT_SCALE",
                "local rate-limit scaling is refused when APP_ENV or FI_ENV is prod",
            ));
        }
        let accounts = Arc::new(AccountLimiter::from_env(env)?);
        Ok(Self {
            verifier,
            local_idp,
            mail,
            firebase_hash,
            jwks,
            access_keys,
            ticket_keys,
            pepper,
            portal_assertion,
            info,
            local_limit_scale,
            accounts,
        })
    }

    /// `limit` with its `max` multiplied by [`IdentityConfig::local_limit_scale`] (1 in prod).
    pub fn scaled(&self, limit: &fi_db::limits::Limit) -> fi_db::limits::Limit {
        fi_db::limits::Limit {
            max: limit.max.saturating_mul(self.local_limit_scale),
            ..*limit
        }
    }
}

/// Identity Platform ID-token verification: the emulator (local, `demo-` projects only) or the
/// configured key set (`FI_IDP_JWKS`; the cached JWKS fetcher is a SER-1.1 follow-up).
/// `FI_IDP_PROVIDERS` is a comma-separated allowlist of `firebase.sign_in_provider` values,
/// `password` (e-mail and password) by default and the only provider at launch (OD-42): an ID
/// token from any other provider is `AUTH.PROVIDER_NOT_ALLOWED`.
fn id_token_verifier(
    env: &dyn Env,
    prod: bool,
) -> Result<(Verifier, Option<Arc<crate::jwks::RotatingVerifier>>), ConfigError> {
    let project = required(env, "FI_IDP_PROJECT_ID")?;
    let providers = idp_providers(env);
    let mut remote = None;
    let mode = match env.get("FIREBASE_AUTH_EMULATOR_HOST") {
        Some(_) => {
            if prod {
                return Err(invalid(
                    "FIREBASE_AUTH_EMULATOR_HOST",
                    "emulator mode is refused when APP_ENV or FI_ENV is prod",
                ));
            }
            let gcp = env.get("GCP_PROJECT_ID");
            if !project.starts_with("demo-") || gcp.is_some_and(|g| !g.starts_with("demo-")) {
                return Err(invalid(
                    "FIREBASE_AUTH_EMULATOR_HOST",
                    "emulator mode needs demo- project ids (FI_IDP_PROJECT_ID, GCP_PROJECT_ID)",
                ));
            }
            Mode::Emulator
        }
        None => match env.get("FI_IDP_JWKS") {
            Some(_) if prod => {
                return Err(invalid(
                    "FI_IDP_JWKS",
                    "static keys are local-test only; production uses rotating Google keys",
                ));
            }
            Some(jwks) => {
                let keys = StaticKeys::from_jwks(&jwks)
                    .map_err(|_| invalid("FI_IDP_JWKS", "not a JWKS of RSA keys"))?;
                Mode::Rs256(Arc::new(keys))
            }
            None => {
                remote = Some(Arc::new(
                    crate::jwks::RotatingVerifier::google(&project, &providers).map_err(|_| {
                        invalid("FI_IDP_PROJECT_ID", "cannot configure secure key retrieval")
                    })?,
                ));
                Mode::Rs256(Arc::new(StaticKeys::new()))
            }
        },
    };
    let verifier = Verifier::new(mode, &project, &providers).map_err(|e| match e {
        idtoken::ConfigError::EmulatorNeedsDemoProject => invalid(
            "FI_IDP_PROJECT_ID",
            "emulator mode needs a demo- project id",
        ),
        idtoken::ConfigError::Empty => invalid("FI_IDP_PROVIDERS", "no provider"),
    })?;
    Ok((verifier, remote))
}

/// `FI_HARDCORE_FROM_PROFILE` (CR-S52, SAVES_API.md §4.10): `true` or `false` (unset: `false`). A
/// Hardcore line whose start is `fresh_start` may then start from another of the account's
/// profiles' save, checked on the spot (`POST .../save/check`, a `seed_from_profile` commit).
pub fn hardcore_from_profile_from_env(env: &dyn Env) -> Result<bool, ConfigError> {
    match env
        .get("FI_HARDCORE_FROM_PROFILE")
        .as_deref()
        .map(str::trim)
    {
        None | Some("") | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(invalid("FI_HARDCORE_FROM_PROFILE", "true or false")),
    }
}

/// `FI_HARDCORE_START` (CR-S51, SAVES_API.md §4.9): `seed` or `fresh_start`. Unset or empty:
/// `seed` when `APP_ENV` or `FI_ENV` is prod (review M4: retiring production's seed never silently
/// switches it to player-supplied first saves; an operator sets `fresh_start` explicitly),
/// elsewhere (a standalone instance, local) decided by the game's active seed.
pub fn hardcore_start_from_env(env: &dyn Env) -> Result<HardcoreStart, ConfigError> {
    match env
        .get("FI_HARDCORE_START")
        .filter(|t| !t.trim().is_empty())
    {
        None if is_prod(env) => Ok(HardcoreStart::Seed),
        None => Ok(HardcoreStart::Auto),
        Some(text) => HardcoreStart::parse(&text)
            .ok_or_else(|| invalid("FI_HARDCORE_START", "seed or fresh_start (or unset)")),
    }
}

/// `FI_FRESH_START_MAX_*` (CR-S51, CLOUD_SAVES.md §7.4): the fresh-start ceilings, each a
/// non-negative integer, all 0 by default (the owner's "zero for everything"), and
/// `FI_FRESH_START_ALLOW_MISSING` (a comma list of `winnings`, `tokens`; empty by default: a first
/// save must carry both).
pub fn fresh_start_limits_from_env(env: &dyn Env) -> Result<FreshStartLimits, ConfigError> {
    let ceiling = |name: &'static str| -> Result<i64, ConfigError> {
        let n: i64 = number(env, name, 0)?;
        if n < 0 {
            return Err(invalid(name, "a non-negative integer"));
        }
        Ok(n)
    };
    FreshStartLimits {
        credits: ceiling("FI_FRESH_START_MAX_CREDITS")?,
        winnings: ceiling("FI_FRESH_START_MAX_WINNINGS")?,
        xp: ceiling("FI_FRESH_START_MAX_XP")?,
        level: ceiling("FI_FRESH_START_MAX_LEVEL")?,
        cars: ceiling("FI_FRESH_START_MAX_CARS")?,
        tokens: ceiling("FI_FRESH_START_MAX_TOKENS")?,
        ..FreshStartLimits::default()
    }
    .allowing_missing(&env.get("FI_FRESH_START_ALLOW_MISSING").unwrap_or_default())
    .map_err(|e| invalid("FI_FRESH_START_ALLOW_MISSING", e.to_string()))
}

/// The CR-S51 cloud-saves settings a save deployment reads (`FI_HARDCORE_START`, `FI_FRESH_START_MAX_*`); [`Default`] is production's (none set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloudSavesSettings {
    /// [`hardcore_start_from_env`].
    pub hardcore_start: HardcoreStart,
    /// [`fresh_start_limits_from_env`].
    pub fresh_start: FreshStartLimits,
    /// [`hardcore_from_profile_from_env`] (CR-S52).
    pub hardcore_from_profile: bool,
}

impl CloudSavesSettings {
    /// Reads the three settings.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        Ok(Self {
            hardcore_start: hardcore_start_from_env(env)?,
            fresh_start: fresh_start_limits_from_env(env)?,
            hardcore_from_profile: hardcore_from_profile_from_env(env)?,
        })
    }
}

impl PlatformConfig {
    /// Applies `s` (a save deployment's own settings).
    pub fn with_cloud_saves(mut self, s: CloudSavesSettings) -> Self {
        self.hardcore_start = s.hardcore_start;
        self.fresh_start = s.fresh_start;
        self.hardcore_from_profile = s.hardcore_from_profile;
        self
    }
}

/// `FI_GUARD_ENFORCE`: `true` or `false` (unset: `false`, shadow). The Hardcore guardrails then
/// refuse a score of 60 or more through the existing refusal paths (a parked bundle and
/// `SAVES.GATE_REFUSED` for a save, a refused sync and its strike episode for the game's syncs;
/// OD-62, CLOUD_SAVES.md §7.3). Allowed in prod: it is the owner's switch once the shadow data is
/// calibrated.
pub fn guard_enforce_from_env(env: &dyn Env) -> Result<bool, ConfigError> {
    match env.get("FI_GUARD_ENFORCE").as_deref().map(str::trim) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(invalid("FI_GUARD_ENFORCE", "true or false")),
    }
}

/// The livery settings of the game plane (CR-S59, GAME_PLATFORM.md §3a "Liveries"), the same on
/// every deployment that serves the proxied game requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiverySettings {
    /// `FI_LIVERY_ACCOUNT_BYTES` (default `fi_db::liveries::DEFAULT_ACCOUNT_BYTES`, 25 MiB; 1 MiB
    /// to 1 GiB): the distinct livery bytes an account may hold over its profiles.
    pub account_bytes: i64,
    /// `FI_LIVERY_LAYERED` (`true`/`false`, default `true` since the in-game check of
    /// GAME_PLATFORM.md §3a passed, owner 2026-10-27): whether designs with layers reach other
    /// players. `false` serves others only layerless designs.
    pub layered_to_others: bool,
}

impl Default for LiverySettings {
    fn default() -> Self {
        Self {
            account_bytes: fi_db::liveries::DEFAULT_ACCOUNT_BYTES,
            layered_to_others: true,
        }
    }
}

impl LiverySettings {
    /// Reads `FI_LIVERY_ACCOUNT_BYTES` and `FI_LIVERY_LAYERED`.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        let mut out = Self::default();
        if let Some(text) = env.get("FI_LIVERY_ACCOUNT_BYTES") {
            out.account_bytes = text
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|n| (1 << 20..=1 << 30).contains(n))
                .ok_or_else(|| invalid("FI_LIVERY_ACCOUNT_BYTES", "1048576 to 1073741824"))?;
        }
        out.layered_to_others = match env.get("FI_LIVERY_LAYERED").as_deref().map(str::trim) {
            None | Some("true") => true,
            Some("false") => false,
            Some(_) => return Err(invalid("FI_LIVERY_LAYERED", "true or false")),
        };
        Ok(out)
    }

    /// The process's settings, read once from its environment; an invalid value is logged and
    /// the defaults apply (the deployments validate it at start with [`Self::from_env`]).
    pub fn process() -> Self {
        static SETTINGS: std::sync::OnceLock<LiverySettings> = std::sync::OnceLock::new();
        *SETTINGS.get_or_init(|| {
            Self::from_env(&ProcessEnv).unwrap_or_else(|e| {
                tracing::error!(error = %e, "livery settings invalid, defaults apply");
                Self::default()
            })
        })
    }
}

/// Settings of the play-session, save, game and internal surfaces (local environment v0,
/// docs/server/LOCAL_ENV.md §2, §3.3). The timing knobs default to the contract's values and are
/// refused in prod with any other value; `FI_LOCAL_ADMIN` is refused in prod.
#[derive(Clone, Debug)]
pub struct PlatformConfig {
    /// Global launcher floor for legacy game deployments without identity-plane state.
    pub min_launcher: String,
    /// Required by default in processes. Explicit local synthetic-test bypass only.
    pub content_required: bool,
    /// `FI_POOL_ROUTING` (`vip=<pool>,free=<pool>`; default `fi_domain::PoolRouting::mvp`).
    pub routing: PoolRouting,
    /// `FI_POOLS_JSON`: `instanceCapacity` per pool for `/internal/v1/pools` (the rows themselves
    /// are written by `fi-cli local-seed`).
    pub pools: Vec<PoolSpec>,
    /// `FI_LEASE_TTL_MS` (default 300000; `heartbeatEveryMs = ttl / 5`).
    pub lease_ttl_ms: u32,
    /// `FI_FREE_LIVENESS_MS` (OD-40; default and most 600000, the ticket's lifetime; least 30000;
    /// `heartbeatEveryMs = 80%` of it): how long a free session's slot holds without a heartbeat
    /// or a game request. A shorter window is refused in prod.
    pub free_liveness_ms: u32,
    /// `FI_LOCAL_ADMIN=true`: `PUT /internal/v1/local/entitlements` answers (local only).
    pub local_admin: bool,
    /// `FI_GUARD_ENFORCE` (default `false`, shadow; [`guard_enforce_from_env`]): the Hardcore
    /// guardrails refuse a score of 60 or more (OD-62, CLOUD_SAVES.md §7.3). Ceilings refuse
    /// either way.
    pub guard_enforce: bool,
    /// `FI_INSTANCE_ID` (default: the hostname), stamped as `X-Fi-Instance`.
    pub instance_id: String,
    /// The pool a game deployment serves (`FI_POOL`), stamped as `X-Fi-Pool`.
    pub game_pool: Option<String>,
    /// `FI_TOKEN_SIGNING_KEYS`: signs session tokens (account plane) and verifies them (game).
    pub session_keys: Option<SigningKeys>,
    /// The cloud-saves bucket (`STORAGE_EMULATOR_HOST`, `CLOUD_SAVES_BUCKET`; launcher surface).
    pub blobs: Option<Blobs>,
    /// The game plane's per-instance liveness cache (`FI_GAME_LIVENESS_CACHE_MS`, default 60000,
    /// at most 60000, 0 disables; game surface only): derived state, ADR 0008 §4 amendment.
    pub liveness: std::sync::Arc<crate::game::cache::LivenessCache>,
    /// `FI_HARDCORE_START` ([`hardcore_start_from_env`], CR-S51): how a Hardcore line starts.
    pub hardcore_start: HardcoreStart,
    /// `FI_FRESH_START_MAX_*` ([`fresh_start_limits_from_env`], CR-S51): the fresh-start ceilings.
    pub fresh_start: FreshStartLimits,
    /// `FI_HARDCORE_FROM_PROFILE` ([`hardcore_from_profile_from_env`], CR-S52): a Hardcore line may
    /// start from another of the account's profiles' save (off by default).
    pub hardcore_from_profile: bool,
}

impl PlatformConfig {
    /// The contract's values and nothing configured (unit tests; [`crate::app::Config::parse`]).
    pub fn defaults(game_pool: Option<String>) -> Self {
        Self {
            content_required: false, // In-process legacy/unit fixture constructor, NOT process defaults.
            min_launcher: "0.1.0".into(),
            routing: PoolRouting::mvp(),
            pools: Vec::new(),
            lease_ttl_ms: fi_saves::lease::DEFAULT_TTL_MS,
            free_liveness_ms: fi_db::admission::FREE_LIVENESS_MS,
            local_admin: false,
            guard_enforce: false,
            instance_id: "local".into(),
            game_pool,
            session_keys: None,
            blobs: None,
            liveness: std::sync::Arc::new(crate::game::cache::LivenessCache::new(0)),
            hardcore_start: HardcoreStart::Auto,
            fresh_start: FreshStartLimits::default(),
            hardcore_from_profile: false,
        }
    }

    /// Reads the settings the served surfaces need: the key ring for `launcher` and `game`, the
    /// bucket for `launcher`.
    pub fn from_env(
        env: &dyn Env,
        game_pool: Option<String>,
        launcher: bool,
        game: bool,
    ) -> Result<Self, ConfigError> {
        let prod = is_prod(env);
        let mut out = Self::defaults(game_pool);
        out.content_required = content_required_from_env(env)?;
        out.min_launcher = launcher_versions_from_env(env)?.0;
        if let Some(text) = env.get("FI_POOL_ROUTING") {
            out.routing = parse_routing(&text)?;
        }
        if let Some(text) = env.get("FI_POOLS_JSON") {
            out.pools = fi_db::admission::parse_pools_json(&text)
                .map_err(|e| invalid("FI_POOLS_JSON", e))?;
        }
        let ttl: u32 = number(env, "FI_LEASE_TTL_MS", fi_saves::lease::DEFAULT_TTL_MS)?;
        if !(fi_saves::lease::MIN_TTL_MS..=fi_saves::lease::MAX_TTL_MS).contains(&ttl) {
            return Err(invalid("FI_LEASE_TTL_MS", "1000 to 3600000"));
        }
        if prod && ttl != fi_saves::lease::DEFAULT_TTL_MS {
            return Err(invalid(
                "FI_LEASE_TTL_MS",
                "the local timing knob is refused when APP_ENV or FI_ENV is prod",
            ));
        }
        out.lease_ttl_ms = ttl;
        let free: u32 = number(
            env,
            "FI_FREE_LIVENESS_MS",
            fi_db::admission::FREE_LIVENESS_MS,
        )?;
        // At most the default, the ticket's lifetime: the launcher's call at 80% of the window
        // must land before its ticket expires (GATE_API.md §4; tested below).
        if !(fi_db::admission::FREE_LIVENESS_MIN_MS..=fi_db::admission::FREE_LIVENESS_MS)
            .contains(&free)
        {
            return Err(invalid("FI_FREE_LIVENESS_MS", "30000 to 600000"));
        }
        if prod && free < fi_db::admission::FREE_LIVENESS_MS {
            return Err(invalid(
                "FI_FREE_LIVENESS_MS",
                "a window below 600000 is a local timing knob, refused when APP_ENV or FI_ENV is prod",
            ));
        }
        out.free_liveness_ms = free;
        if game {
            let cache_ms: u32 = number(
                env,
                "FI_GAME_LIVENESS_CACHE_MS",
                crate::game::cache::MAX_TTL_MS,
            )?;
            if cache_ms > crate::game::cache::MAX_TTL_MS {
                return Err(invalid(
                    "FI_GAME_LIVENESS_CACHE_MS",
                    "0 to 60000 (the game plane re-reads a session at least every 60 s)",
                ));
            }
            out.liveness = std::sync::Arc::new(crate::game::cache::LivenessCache::new(cache_ms));
        }
        out.local_admin = match env.get("FI_LOCAL_ADMIN").as_deref().map(str::trim) {
            None | Some("false") => false,
            Some("true") if prod => {
                return Err(invalid(
                    "FI_LOCAL_ADMIN",
                    "local administration is refused when APP_ENV or FI_ENV is prod",
                ));
            }
            Some("true") => true,
            Some(_) => return Err(invalid("FI_LOCAL_ADMIN", "true or false")),
        };
        out.guard_enforce = guard_enforce_from_env(env)?;
        // Validated at start; the game plane reads them once ([`LiverySettings::process`]).
        LiverySettings::from_env(env)?;
        out = out.with_cloud_saves(CloudSavesSettings::from_env(env)?);
        out.instance_id = env
            .get("FI_INSTANCE_ID")
            .or_else(|| env.get("HOSTNAME"))
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|h| h.trim().to_owned())
                    .filter(|h| !h.is_empty())
            })
            .unwrap_or_else(|| "local".into());
        if !(1..=128).contains(&out.instance_id.len())
            || !out
                .instance_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid(
                "FI_INSTANCE_ID",
                "1 to 128 letters, digits, dashes, dots or underscores",
            ));
        }
        if launcher || game {
            let ring = SigningKeys::parse(
                "FI_TOKEN_SIGNING_KEYS",
                &required(env, "FI_TOKEN_SIGNING_KEYS")?,
            )
            .map_err(|e| invalid("FI_TOKEN_SIGNING_KEYS", e.reason))?;
            if prod && ring.has_local_dev_key() {
                return Err(invalid(
                    "FI_TOKEN_SIGNING_KEYS",
                    "a well-known local development key is refused in prod",
                ));
            }
            out.session_keys = Some(ring);
        }
        if launcher
            && let Some(fs) = Blobs::from_parts(
                env.get("FI_BLOB_BACKEND").as_deref(),
                env.get("FI_BLOB_DIR").as_deref(),
                is_prod(env),
            )
            .map_err(|e| invalid("FI_BLOB_BACKEND", e.to_string()))?
        {
            out.blobs = Some(fs);
        } else if launcher {
            let endpoint = env.get("STORAGE_EMULATOR_HOST").ok_or_else(|| {
                invalid(
                    "STORAGE_EMULATOR_HOST",
                    "the saves bucket needs the Storage emulator in v0 (the metadata-server path is SER-2.2)",
                )
            })?;
            let bucket = required(env, "CLOUD_SAVES_BUCKET")?;
            out.blobs = Some(GcsBlobStore::new(endpoint.trim(), bucket.trim()).into());
        }
        Ok(out)
    }

    /// `instanceCapacity` of a pool, when `FI_POOLS_JSON` names it.
    pub fn instance_capacity(&self, pool: &str) -> Option<u32> {
        self.pools
            .iter()
            .find(|p| p.name.as_str() == pool)
            .and_then(|p| p.instance_capacity)
    }

    /// The slot length per pool tier every admission, poll and heartbeat decides with: the lease
    /// for Supporters, the free liveness window for free pools (OD-40).
    pub fn slot_ttl(&self) -> fi_db::admission::SlotTtl {
        fi_db::admission::SlotTtl {
            lease_ms: self.lease_ttl_ms,
            free_ms: self.free_liveness_ms,
        }
    }
}

/// Validated deployment floor and advertised latest. Absent latest defaults to the floor;
/// changing the advertised latest alone never changes online eligibility.
pub fn launcher_versions_from_env(env: &dyn Env) -> Result<(String, String), ConfigError> {
    use fi_domain::version::LauncherVersion;
    let minimum = env.get("FI_MIN_LAUNCHER").unwrap_or_else(|| "0.1.0".into());
    let latest = env
        .get("FI_LATEST_LAUNCHER")
        .unwrap_or_else(|| minimum.clone());
    let min_version = LauncherVersion::parse(&minimum)
        .map_err(|_| invalid("FI_MIN_LAUNCHER", "strict SemVer, 1 to 64 ASCII bytes"))?;
    let latest_version = LauncherVersion::parse(&latest)
        .map_err(|_| invalid("FI_LATEST_LAUNCHER", "strict SemVer, 1 to 64 ASCII bytes"))?;
    if latest_version.precedence_cmp(&min_version).is_lt() {
        return Err(invalid("FI_LATEST_LAUNCHER", "not below FI_MIN_LAUNCHER"));
    }
    Ok((minimum, latest))
}

/// Content enforcement is fail-closed unless an explicitly local process opts into test bypass.
pub fn content_required_from_env(env: &dyn Env) -> Result<bool, ConfigError> {
    match env.get("FI_CONTENT_LOCAL_TEST_BYPASS").as_deref() {
        None | Some("false") => Ok(true),
        Some("true")
            if !is_prod(env)
                && ["APP_ENV", "FI_ENV"]
                    .iter()
                    .any(|k| env.get(k).as_deref() == Some("local"))
                && ["APP_ENV", "FI_ENV"]
                    .iter()
                    .all(|k| env.get(k).is_none_or(|v| v == "local")) =>
        {
            Ok(false)
        }
        _ => Err(invalid(
            "FI_CONTENT_LOCAL_TEST_BYPASS",
            "true only with an explicitly local environment; otherwise false",
        )),
    }
}

/// `vip=<pool>,free=<pool>`, both tiers exactly once.
fn parse_routing(text: &str) -> Result<PoolRouting, ConfigError> {
    let bad = || invalid("FI_POOL_ROUTING", "vip=<pool>,free=<pool>");
    let (mut vip, mut free) = (None, None);
    for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (tier, pool) = item.split_once('=').ok_or_else(bad)?;
        let pool = PoolName::new(pool.trim()).map_err(|_| bad())?;
        let slot = match tier.trim() {
            "vip" => &mut vip,
            "free" => &mut free,
            _ => return Err(bad()),
        };
        if slot.replace(pool).is_some() {
            return Err(bad());
        }
    }
    match (vip, free) {
        (Some(vip), Some(free)) => Ok(PoolRouting { vip, free }),
        _ => Err(bad()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn launcher_process_policy_is_strict_and_latest_is_not_below_minimum() {
        let env = Vec::<(&str, String)>::new();
        assert_eq!(
            launcher_versions_from_env(&env).unwrap(),
            ("0.1.0".into(), "0.1.0".into())
        );
        let env = with(env, "FI_MIN_LAUNCHER", "1.2.0");
        assert_eq!(launcher_versions_from_env(&env).unwrap().1, "1.2.0");
        for (name, value) in [
            ("FI_MIN_LAUNCHER", "1.2"),
            ("FI_MIN_LAUNCHER", "v1.2.0"),
            ("FI_LATEST_LAUNCHER", "1.2.0-rc.1"),
            ("FI_LATEST_LAUNCHER", "1.1.9"),
            ("FI_LATEST_LAUNCHER", "1.2.0.4"),
        ] {
            assert!(launcher_versions_from_env(&with(env.clone(), name, value)).is_err());
        }
        assert_eq!(
            launcher_versions_from_env(&with(env, "FI_LATEST_LAUNCHER", "1.3.0+official"))
                .unwrap()
                .1,
            "1.3.0+official"
        );
    }

    /// CR-S52: `FI_HARDCORE_FROM_PROFILE` is off unless `true`, a typo refuses to start.
    #[test]
    fn hardcore_from_profile_is_off_by_default() {
        let none = Vec::<(&str, String)>::new();
        assert!(!hardcore_from_profile_from_env(&none).unwrap());
        assert!(
            !CloudSavesSettings::from_env(&none)
                .unwrap()
                .hardcore_from_profile
        );
        let on = vec![("FI_HARDCORE_FROM_PROFILE", "true".to_owned())];
        assert!(hardcore_from_profile_from_env(&on).unwrap());
        assert!(
            PlatformConfig::defaults(None)
                .with_cloud_saves(CloudSavesSettings::from_env(&on).unwrap())
                .hardcore_from_profile
        );
        assert!(
            hardcore_from_profile_from_env(&vec![("FI_HARDCORE_FROM_PROFILE", "yes".to_owned())])
                .is_err()
        );
    }

    #[test]
    fn cloud_saves_and_the_fresh_start_default_to_production_and_refuse_typos() {
        let none = Vec::<(&str, String)>::new();
        assert_eq!(hardcore_start_from_env(&none).unwrap(), HardcoreStart::Auto);
        assert_eq!(
            fresh_start_limits_from_env(&none).unwrap(),
            FreshStartLimits::default()
        );
        assert_eq!(
            hardcore_start_from_env(&vec![("FI_HARDCORE_START", String::new())]).unwrap(),
            HardcoreStart::Auto
        );
        assert_eq!(
            hardcore_start_from_env(&vec![("FI_HARDCORE_START", "fresh_start".to_owned())])
                .unwrap(),
            HardcoreStart::FreshStart
        );
        assert!(hardcore_start_from_env(&vec![("FI_HARDCORE_START", "auto".to_owned())]).is_err());
        // Review M4: unset in prod is the seed, never the active-seed fallback.
        for prod in ["APP_ENV", "FI_ENV"] {
            assert_eq!(
                hardcore_start_from_env(&vec![(prod, "prod".to_owned())]).unwrap(),
                HardcoreStart::Seed
            );
            assert_eq!(
                hardcore_start_from_env(&vec![
                    (prod, "prod".to_owned()),
                    ("FI_HARDCORE_START", String::new())
                ])
                .unwrap(),
                HardcoreStart::Seed
            );
            assert_eq!(
                hardcore_start_from_env(&vec![
                    (prod, "prod".to_owned()),
                    ("FI_HARDCORE_START", "fresh_start".to_owned())
                ])
                .unwrap(),
                HardcoreStart::FreshStart
            );
        }
        assert_eq!(
            hardcore_start_from_env(&vec![("FI_ENV", "local".to_owned())]).unwrap(),
            HardcoreStart::Auto
        );
        let allow = fresh_start_limits_from_env(&vec![(
            "FI_FRESH_START_ALLOW_MISSING",
            "winnings,tokens".to_owned(),
        )])
        .unwrap();
        assert!(allow.allow_missing_winnings && allow.allow_missing_tokens);
        assert!(
            fresh_start_limits_from_env(&vec![("FI_FRESH_START_ALLOW_MISSING", "cars".to_owned())])
                .is_err()
        );
        let loose = vec![("FI_FRESH_START_MAX_CREDITS", "5000".to_owned())];
        assert_eq!(fresh_start_limits_from_env(&loose).unwrap().credits, 5_000);
        for bad in ["-1", "x"] {
            assert!(
                fresh_start_limits_from_env(&vec![("FI_FRESH_START_MAX_CARS", bad.to_owned())])
                    .is_err()
            );
        }
    }

    #[test]
    fn content_policy_is_required_unless_explicitly_local_test_bypass() {
        assert!(content_required_from_env(&Vec::<(&str, String)>::new()).unwrap());
        assert!(content_required_from_env(&local()).unwrap());
        assert!(
            !content_required_from_env(&with(local(), "FI_CONTENT_LOCAL_TEST_BYPASS", "true"))
                .unwrap()
        );
        for env in [
            vec![("FI_CONTENT_LOCAL_TEST_BYPASS", "true".into())],
            vec![
                ("APP_ENV", "prod".into()),
                ("FI_CONTENT_LOCAL_TEST_BYPASS", "true".into()),
            ],
            vec![
                ("APP_ENV", "local".into()),
                ("FI_ENV", "staging".into()),
                ("FI_CONTENT_LOCAL_TEST_BYPASS", "true".into()),
            ],
            vec![
                ("APP_ENV", "local".into()),
                ("FI_CONTENT_LOCAL_TEST_BYPASS", "1".into()),
            ],
        ] {
            assert!(content_required_from_env(&env).is_err());
        }
    }

    #[test]
    fn the_service_map_takes_origins_only() {
        let mut env = local();
        assert_eq!(service_map(&env).unwrap(), ServiceMap::default());
        env.push(("FI_PUBLIC_CORE_URL", "https://api.example.test/".into()));
        env.push(("FI_PUBLIC_GATE_URL", "http://127.0.0.1:8680".into()));
        let map = service_map(&env).unwrap();
        assert_eq!(
            (map.core.as_str(), map.gate.as_str(), map.saves.as_str()),
            ("https://api.example.test", "http://127.0.0.1:8680", "")
        );
        for bad in [
            "api.example.test",
            "https://api.example.test/core",
            "https://",
            "ftp://x",
            "https://u@x",
        ] {
            assert!(
                service_map(&with(local(), "FI_PUBLIC_SAVES_URL", bad)).is_err(),
                "{bad}"
            );
        }
        let id = IdentityConfig::from_env(&env, false).unwrap();
        assert_eq!(id.info.services.core, "https://api.example.test");
        assert!(id.info.games.is_empty());
    }

    #[test]
    fn platform_settings_default_to_the_contract_and_local_knobs_stay_local() {
        let mut env = local();
        env.push(("STORAGE_EMULATOR_HOST", "http://127.0.0.1:4453".into()));
        env.push(("CLOUD_SAVES_BUCKET", "fi-dev-cloud-saves".into()));
        let p = PlatformConfig::from_env(&env, None, true, false).unwrap();
        assert_eq!(p.routing, PoolRouting::mvp());
        assert_eq!(p.lease_ttl_ms, 300_000);
        // OD-40: free sessions hold their slot for the ticket's 10 minutes by default.
        assert_eq!(p.free_liveness_ms, 600_000);
        assert_eq!(
            p.slot_ttl(),
            fi_db::admission::SlotTtl {
                lease_ms: 300_000,
                free_ms: 600_000,
                // OD-129: every profile keeps a line by default.
            }
        );
        assert_eq!(
            i64::from(fi_db::admission::FREE_LIVENESS_MS),
            fi_auth::session::TTL_SECS * 1_000,
            "the longest free window is the ticket's lifetime"
        );
        assert!(!p.local_admin);
        assert!(!p.guard_enforce, "the guardrails start in shadow");
        assert!(p.session_keys.is_some() && p.blobs.is_some());
        for (value, enforce) in [("true", true), ("false", false), (" true ", true)] {
            let p = PlatformConfig::from_env(
                &with(env.clone(), "FI_GUARD_ENFORCE", value),
                None,
                true,
                false,
            )
            .unwrap();
            assert_eq!(p.guard_enforce, enforce, "{value}");
        }
        // The owner's switch is allowed in prod.
        assert!(
            guard_enforce_from_env(&with(
                with(env.clone(), "APP_ENV", "prod"),
                "FI_GUARD_ENFORCE",
                "true"
            ))
            .unwrap()
        );
        let mut demo = env.clone();
        demo.push(("FI_POOL_ROUTING", "vip=vip, free=free".into()));
        demo.push(("FI_LEASE_TTL_MS", "20000".into()));
        demo.push(("FI_FREE_LIVENESS_MS", "90000".into()));
        demo.push(("FI_LOCAL_ADMIN", "true".into()));
        demo.push(("FI_INSTANCE_ID", "game-vip-1".into()));
        demo.push((
            "FI_POOLS_JSON",
            r#"[{"name":"vip","tier":"vip","capacity":12,"queueLimit":100,"instanceCapacity":4,"gameEndpoint":"http://127.0.0.1:8480/pools/vip"}]"#.into(),
        ));
        let p = PlatformConfig::from_env(&demo, None, true, false).unwrap();
        assert_eq!(p.routing.free.as_str(), "free");
        assert_eq!((p.lease_ttl_ms, p.local_admin), (20_000, true));
        assert_eq!(p.slot_ttl().for_tier("free"), 90_000);
        assert_eq!(p.slot_ttl().for_tier("vip"), 20_000);
        assert_eq!(p.instance_id, "game-vip-1");
        assert_eq!(p.instance_capacity("vip"), Some(4));
        assert_eq!(p.instance_capacity("free"), None);
        for (name, value) in [
            ("FI_POOL_ROUTING", "vip=vip"),
            ("FI_POOL_ROUTING", "vip=vip,free=free,vip=x"),
            ("FI_POOL_ROUTING", "gold=vip,free=free"),
            ("FI_LEASE_TTL_MS", "10"),
            ("FI_FREE_LIVENESS_MS", "29999"),
            ("FI_FREE_LIVENESS_MS", "600001"),
            ("FI_FREE_LIVENESS_MS", "0"),
            ("FI_FREE_LIVENESS_MS", "ten minutes"),
            ("FI_LOCAL_ADMIN", "yes"),
            ("FI_GUARD_ENFORCE", "1"),
            ("FI_POOLS_JSON", "[1]"),
            ("FI_INSTANCE_ID", "a b"),
        ] {
            assert!(
                PlatformConfig::from_env(&with(env.clone(), name, value), None, true, false)
                    .is_err(),
                "{name}={value}"
            );
        }
        let prod = with(env.clone(), "APP_ENV", "prod");
        for (name, value) in [
            ("FI_LOCAL_ADMIN", "true"),
            ("FI_LEASE_TTL_MS", "20000"),
            ("FI_FREE_LIVENESS_MS", "480000"),
            ("FI_FREE_LIVENESS_MS", "30000"),
        ] {
            match PlatformConfig::from_env(&with(prod.clone(), name, value), None, false, false) {
                Err(ConfigError::Invalid { name: n, .. }) => assert_eq!(n, name),
                other => panic!("{name} must be refused in prod: {other:?}"),
            }
        }
        // Production runs the default window; naming it is not refused.
        let p = PlatformConfig::from_env(
            &with(prod.clone(), "FI_FREE_LIVENESS_MS", "600000"),
            None,
            false,
            false,
        )
        .unwrap();
        assert_eq!(p.free_liveness_ms, 600_000);
        // The game plane needs the key ring and no bucket.
        let mut game: Vec<(&'static str, String)> =
            vec![("FI_TOKEN_SIGNING_KEYS", LOCAL_TOKEN.into())];
        assert!(
            PlatformConfig::from_env(&game, Some("vip".into()), false, true)
                .unwrap()
                .blobs
                .is_none()
        );
        let p = PlatformConfig::from_env(&game, Some("vip".into()), false, true).unwrap();
        assert_eq!(
            p.liveness.ttl_ms(),
            60_000,
            "the contract's bound by default"
        );
        let p = PlatformConfig::from_env(
            &with(game.clone(), "FI_GAME_LIVENESS_CACHE_MS", "4000"),
            Some("vip".into()),
            false,
            true,
        )
        .unwrap();
        assert_eq!(p.liveness.ttl_ms(), 4_000);
        assert!(
            PlatformConfig::from_env(
                &with(game.clone(), "FI_GAME_LIVENESS_CACHE_MS", "60001"),
                Some("vip".into()),
                false,
                true,
            )
            .is_err()
        );
        game.clear();
        assert!(PlatformConfig::from_env(&game, Some("vip".into()), false, true).is_err());
    }

    const LOCAL_TOKEN: &str = "at-local-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=";
    /// The well-known local `FI_LOCAL_IDP_SIGNING_KEYS` (fi_auth::keys::LOCAL_DEV_LABELS).
    pub(crate) const LOCAL_IDP: &str = "idp-local-1:BvJcQZ87cw9bQswFPCv0HZSm7+67u1v9rhIWKh01i/o=";
    /// A prod-shaped local IdP ring (32 bytes of 0x0a).
    const PROD_IDP: &str = "idp-2026-10:CgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgo=";
    const LOCAL_TICKET: &str = "lt-local-1:8BxGVNw9Cyi6bbk5aWzYhSbdgW8CRkM125qYUron8uI=";
    const LOCAL_PEPPER: &str = "1:84yC96SRqM8E+D3MCtARPCjLJ7eiFc/pGvdtLu2TaHQ=";
    pub(crate) const LOCAL_SECRET: &str = "Y/ye0PBhcv0H2gDUYTIfTIOYHrOapl5avbbKuu76XNo=";
    // Not the local values (32 bytes of 0x07, 0x08, 0x09 in base64).
    const PROD_TOKEN: &str = "at-2026-10:BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
    const PROD_TICKET: &str = "lt-2026-10:CAgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAg=";
    const PROD_PEPPER: &str = "1:CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";
    /// A placeholder Web API key (prod-shaped: not the local `demo-api-key`).
    const PROD_API_KEY: &str = "placeholder-web-api-key-not-a-real-one";

    pub(crate) fn local() -> Vec<(&'static str, String)> {
        vec![
            ("APP_ENV", "local".into()),
            ("GCP_PROJECT_ID", "demo-forza-infinity".into()),
            ("FIREBASE_AUTH_EMULATOR_HOST", "127.0.0.1:9399".into()),
            ("FI_IDP_PROJECT_ID", "demo-forza-infinity".into()),
            ("FI_TOKEN_SIGNING_KEYS", LOCAL_TOKEN.into()),
            ("FI_LAUNCH_TICKET_SIGNING_KEYS", LOCAL_TICKET.into()),
            ("FI_SITE_KEY_PEPPER", LOCAL_PEPPER.into()),
            ("FI_PORTAL_ASSERTION_MODE", "local-shared-secret".into()),
            ("FI_PORTAL_ASSERTION_SECRET", LOCAL_SECRET.into()),
        ]
    }

    #[test]
    fn livery_settings_default_and_parse() {
        let d = LiverySettings::from_env(&Vec::<(&'static str, String)>::new()).unwrap();
        assert_eq!(d, LiverySettings::default());
        assert_eq!(d.account_bytes, 25 * 1024 * 1024);
        assert!(d.layered_to_others);
        let s = LiverySettings::from_env(&vec![
            ("FI_LIVERY_ACCOUNT_BYTES", "2097152".to_owned()),
            ("FI_LIVERY_LAYERED", "false".to_owned()),
        ])
        .unwrap();
        assert_eq!((s.account_bytes, s.layered_to_others), (2_097_152, false));
        for (name, value) in [
            ("FI_LIVERY_ACCOUNT_BYTES", "12"),
            ("FI_LIVERY_ACCOUNT_BYTES", "lots"),
            ("FI_LIVERY_LAYERED", "yes"),
        ] {
            assert!(LiverySettings::from_env(&vec![(name, value.to_owned())]).is_err());
        }
    }

    fn with(
        mut env: Vec<(&'static str, String)>,
        name: &'static str,
        value: &str,
    ) -> Vec<(&'static str, String)> {
        env.retain(|(k, _)| *k != name);
        env.push((name, value.to_owned()));
        env
    }

    fn refused(env: &Vec<(&'static str, String)>, name: &str) {
        match IdentityConfig::from_env(env, true) {
            Err(ConfigError::Invalid { name: n, reason }) => {
                assert_eq!(n, name, "{reason}");
                for secret in [LOCAL_TOKEN, LOCAL_PEPPER, LOCAL_SECRET, PROD_TOKEN] {
                    assert!(!reason.contains(&secret[secret.len() - 20..]), "{reason}");
                }
            }
            Err(ConfigError::Missing(n)) => assert_eq!(n, name),
            Ok(_) => panic!("{name} must be refused"),
        }
    }

    #[test]
    fn the_local_limit_scale_multiplies_limits_and_is_refused_in_prod() {
        let c =
            IdentityConfig::from_env(&with(local(), "FI_LOCAL_LIMIT_SCALE", "100"), true).unwrap();
        assert_eq!(c.scaled(&fi_db::limits::CREATE_NET).max, 1000);
        assert_eq!(c.scaled(&fi_db::limits::CREATE_NET).window_s, 86_400);
        for bad in ["0", "1001", "x"] {
            assert!(
                IdentityConfig::from_env(&with(local(), "FI_LOCAL_LIMIT_SCALE", bad), true)
                    .is_err(),
                "{bad}"
            );
        }
        let prod = with(
            with(local(), "APP_ENV", "prod"),
            "FI_LOCAL_LIMIT_SCALE",
            "100",
        );
        assert!(IdentityConfig::from_env(&prod, true).is_err());
    }

    /// fi-core's identity settings in prod, without `FI_IDP_API_KEY`.
    fn prod_core() -> Vec<(&'static str, String)> {
        vec![
            ("FI_ENV", "prod".into()),
            ("GCP_PROJECT_ID", "fi-placeholder".into()),
            ("FI_IDP_PROJECT_ID", "fi-placeholder".into()),
            ("FI_PLATFORM_SIGNING_KEYS", PROD_TOKEN.into()),
            ("FI_SITE_KEY_PEPPER", PROD_PEPPER.into()),
            ("FI_REALM", "forzainfinity-prod".into()),
        ]
    }

    #[test]
    fn info_carries_the_identity_platform_web_configuration() {
        // Emulator mode: the local key by default; the public emulator origin only when named.
        let c = IdentityConfig::from_env(&local(), true).unwrap();
        assert_eq!(
            c.info.idp,
            IdpConfig {
                kind: fi_protocol::accounts::IdpKind::Firebase,
                api_key: "demo-api-key".into(),
                project_id: "demo-forza-infinity".into(),
                emulator_url: None,
            }
        );
        let lan = with(
            local(),
            "FI_IDP_EMULATOR_PUBLIC_URL",
            "http://192.0.2.10:9399/",
        );
        let c = IdentityConfig::from_env(&lan, true).unwrap();
        assert_eq!(
            c.info.idp.emulator_url.as_deref(),
            Some("http://192.0.2.10:9399")
        );
        assert_ne!(
            c.info.idp.emulator_url.as_deref(),
            Some("http://127.0.0.1:9399"),
            "never derived from FIREBASE_AUTH_EMULATOR_HOST"
        );
        let named = with(local(), "FI_IDP_API_KEY", "local-key_1");
        assert_eq!(
            IdentityConfig::from_env(&named, true)
                .unwrap()
                .info
                .idp
                .api_key,
            "local-key_1"
        );
        // Prod: the configured key, no emulator URL.
        let prod = with(prod_core(), "FI_IDP_API_KEY", PROD_API_KEY);
        let c = IdentityConfig::from_env_for(&prod, IdentityNeeds::CORE).unwrap();
        assert_eq!(c.info.idp.api_key, PROD_API_KEY);
        assert_eq!(c.info.idp.project_id, "fi-placeholder");
        assert_eq!(c.info.idp.emulator_url, None);
        let wire = serde_json::to_value(&c.info).unwrap();
        assert!(wire["idp"].get("emulatorUrl").is_none(), "{wire}");
        // The gate serves no /info: it reads neither variable.
        let gate = vec![
            ("FI_ENV", "prod".to_owned()),
            (
                "FI_PLATFORM_VERIFY_KEYS",
                PublicKeys::of(&SigningKeys::parse("X", PROD_TOKEN).unwrap()).to_text(),
            ),
            ("FI_LAUNCH_TICKET_SIGNING_KEYS", PROD_TICKET.into()),
            ("FI_IDP_EMULATOR_PUBLIC_URL", "http://127.0.0.1:9399".into()),
        ];
        let g = IdentityConfig::from_env_for(&gate, IdentityNeeds::GATE).unwrap();
        assert_eq!(g.info.idp, IdpConfig::default());
    }

    #[test]
    fn the_idp_settings_are_refused_where_they_do_not_belong() {
        let core = |env: &Vec<(&'static str, String)>| {
            IdentityConfig::from_env_for(env, IdentityNeeds::CORE).map(|_| ())
        };
        // Prod: the key is required, the local key refused, the emulator URL refused.
        assert!(matches!(
            core(&prod_core()),
            Err(ConfigError::Missing("FI_IDP_API_KEY"))
        ));
        let prod = with(prod_core(), "FI_IDP_API_KEY", PROD_API_KEY);
        for (name, value) in [
            ("FI_IDP_API_KEY", "demo-api-key"),
            ("FI_IDP_API_KEY", "key with spaces"),
            ("FI_IDP_EMULATOR_PUBLIC_URL", "http://127.0.0.1:9399"),
        ] {
            match core(&with(prod.clone(), name, value)) {
                Err(ConfigError::Invalid { name: n, .. }) => assert_eq!(n, name, "{value}"),
                other => panic!("{name}={value} must be refused in prod: {other:?}"),
            }
        }
        // Locally without the emulator: the key is required and the emulator URL refused.
        let mut jwks = local();
        jwks.retain(|(k, _)| *k != "FIREBASE_AUTH_EMULATOR_HOST");
        jwks.push(("FI_IDP_JWKS", r#"{"keys":[]}"#.into()));
        refused(&jwks, "FI_IDP_API_KEY");
        let jwks = with(jwks, "FI_IDP_API_KEY", "local-key");
        assert!(IdentityConfig::from_env(&jwks, true).is_ok());
        refused(
            &with(jwks, "FI_IDP_EMULATOR_PUBLIC_URL", "http://127.0.0.1:9399"),
            "FI_IDP_EMULATOR_PUBLIC_URL",
        );
        // Emulator mode: an origin, nothing else.
        for bad in [
            "127.0.0.1:9399",
            "auth-emulator:9399",
            "http://127.0.0.1:9399/identitytoolkit.googleapis.com",
            "ftp://127.0.0.1:9399",
            "http://",
        ] {
            refused(
                &with(local(), "FI_IDP_EMULATOR_PUBLIC_URL", bad),
                "FI_IDP_EMULATOR_PUBLIC_URL",
            );
        }
    }

    #[test]
    fn the_local_stack_starts_in_emulator_mode() {
        let c = IdentityConfig::from_env(&local(), true).unwrap();
        assert!(c.verifier.as_ref().unwrap().is_emulator());
        assert_eq!(c.info.realm, "forzainfinity-local");
        assert_eq!(c.info.checkpoints.min_interval_secs, 300);
        assert_eq!(c.local_limit_scale, 1);
        assert_eq!(c.scaled(&fi_db::limits::CREATE_NET).max, 10);
        assert!(c.portal_assertion.is_some());
        assert!(
            IdentityConfig::from_env(&local(), false)
                .unwrap()
                .portal_assertion
                .is_none()
        );
    }

    /// An unsigned emulator ID token of `provider` (verified e-mail), issued now.
    fn emulator_token(provider: &str) -> (String, i64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = serde_json::json!({
            "iss": "https://securetoken.google.com/demo-forza-infinity",
            "aud": "demo-forza-infinity",
            "auth_time": now, "iat": now, "exp": now + 3_600,
            "sub": "uid-od42", "email": "driver@example.test", "email_verified": true,
            "firebase": {"sign_in_provider": provider}
        });
        let token = format!(
            "{}.{}.",
            fi_auth::b64::url_encode(br#"{"alg":"none","typ":"JWT"}"#),
            fi_auth::b64::url_encode(&serde_json::to_vec(&claims).unwrap())
        );
        (token, now)
    }

    #[test]
    fn sign_in_accepts_e_mail_and_password_only_by_default() {
        use fi_auth::idtoken::IdTokenError;
        let verify = |env: &Vec<(&'static str, String)>, provider: &str| {
            let (token, now) = emulator_token(provider);
            let c = IdentityConfig::from_env(env, true).unwrap();
            c.verifier
                .unwrap()
                .verify(&token, now)
                .map(|id| id.provider_id)
        };
        // OD-42: without FI_IDP_PROVIDERS (and with compose's `password`) a Google ID token is refused.
        for env in [local(), with(local(), "FI_IDP_PROVIDERS", "password")] {
            assert_eq!(verify(&env, "password").as_deref(), Ok("password"));
            assert_eq!(
                verify(&env, "google.com"),
                Err(IdTokenError::ProviderNotAllowed)
            );
        }
        // The allowlist stays configuration: a provider added later needs no client release.
        let more = with(local(), "FI_IDP_PROVIDERS", "password, oidc.example");
        assert_eq!(verify(&more, "oidc.example").as_deref(), Ok("oidc.example"));
        assert_eq!(
            verify(&more, "google.com"),
            Err(IdTokenError::ProviderNotAllowed)
        );
        refused(
            &with(local(), "FI_IDP_PROVIDERS", " , "),
            "FI_IDP_PROVIDERS",
        );
    }

    #[test]
    fn emulator_mode_never_meets_a_real_project_or_prod() {
        refused(
            &with(local(), "FI_IDP_PROJECT_ID", "example-project"),
            "FIREBASE_AUTH_EMULATOR_HOST",
        );
        refused(
            &with(local(), "GCP_PROJECT_ID", "example-project"),
            "FIREBASE_AUTH_EMULATOR_HOST",
        );
        refused(
            &with(local(), "APP_ENV", "prod"),
            "FIREBASE_AUTH_EMULATOR_HOST",
        );
        refused(
            &with(local(), "FI_ENV", "prod"),
            "FIREBASE_AUTH_EMULATOR_HOST",
        );
    }

    #[test]
    fn prod_refuses_local_secrets_and_the_shared_secret_assertion() {
        let mut prod: Vec<(&'static str, String)> = local()
            .into_iter()
            .filter(|(k, _)| *k != "FIREBASE_AUTH_EMULATOR_HOST")
            .collect();
        prod = with(prod, "APP_ENV", "prod");
        prod = with(prod, "FI_IDP_PROJECT_ID", "fi-placeholder");
        prod = with(prod, "FI_REALM", "forzainfinity-prod");
        prod = with(prod, "FI_IDP_API_KEY", PROD_API_KEY);
        // In prod the access tokens need the platform ring; the local single ring is not enough.
        refused(&prod, "FI_PLATFORM_SIGNING_KEYS");
        refused(
            &with(prod.clone(), "FI_PLATFORM_SIGNING_KEYS", LOCAL_TOKEN),
            "FI_PLATFORM_SIGNING_KEYS",
        );
        let p = with(prod.clone(), "FI_PLATFORM_SIGNING_KEYS", PROD_TOKEN);
        refused(&p, "FI_LAUNCH_TICKET_SIGNING_KEYS");
        let p = with(p, "FI_LAUNCH_TICKET_SIGNING_KEYS", PROD_TICKET);
        refused(&p, "FI_SITE_KEY_PEPPER");
        let p = with(p, "FI_SITE_KEY_PEPPER", PROD_PEPPER);
        refused(&p, "FI_PORTAL_ASSERTION_MODE");
        let p = with(p, "FI_PORTAL_ASSERTION_MODE", "google-oidc");
        refused(&p, "FI_PORTAL_ASSERTION_MODE");
        // Without the portal surface the same prod settings start (RS256 with the static key set).
        let c = IdentityConfig::from_env(&p, false).unwrap();
        assert!(!c.verifier.as_ref().unwrap().is_emulator());
    }

    /// A local stack on the `local` sign-in backend: no Identity Platform setting at all.
    pub(crate) fn local_backend() -> Vec<(&'static str, String)> {
        let mut env: Vec<_> = local()
            .into_iter()
            .filter(|(k, _)| !matches!(*k, "FIREBASE_AUTH_EMULATOR_HOST" | "FI_IDP_PROJECT_ID"))
            .collect();
        env.push(("FI_AUTH_BACKEND", "local".into()));
        env.push(("FI_LOCAL_IDP_SIGNING_KEYS", LOCAL_IDP.into()));
        env
    }

    #[test]
    fn the_local_backend_needs_its_own_ring_and_no_identity_platform() {
        let c = IdentityConfig::from_env(&local_backend(), true).unwrap();
        // Its tokens say `password`: an allowlist without it is refused at startup.
        let no_password = with(local_backend(), "FI_IDP_PROVIDERS", "google.com");
        assert!(matches!(
            IdentityConfig::from_env(&no_password, true),
            Err(ConfigError::Invalid {
                name: "FI_IDP_PROVIDERS",
                ..
            })
        ));
        assert!(c.is_local() && c.verifier.is_none() && c.jwks.is_none());
        assert_eq!(
            c.info.idp,
            IdpConfig {
                kind: IdpKind::Local,
                ..IdpConfig::default()
            }
        );
        let wire = serde_json::to_value(&c.info.idp).unwrap();
        assert_eq!(wire, serde_json::json!({"kind": "local"}));
        assert_eq!(
            c.local_idp.as_ref().unwrap().issuer(),
            "/launcher/v1/auth",
            "no FI_PUBLIC_CORE_URL locally"
        );
        let with_core = with(
            with(
                local_backend(),
                "FI_PUBLIC_CORE_URL",
                "https://api.example.test",
            ),
            // A public origin with the log mail backend needs registration closed (H1).
            "FI_REGISTRATION_ENABLED",
            "false",
        );
        let c = IdentityConfig::from_env(&with_core, true).unwrap();
        assert_eq!(
            c.local_idp.as_ref().unwrap().issuer(),
            "https://api.example.test/launcher/v1/auth"
        );
        // A firebase instance keeps its wire: no `kind`, no local provider.
        let f = IdentityConfig::from_env(&local(), true).unwrap();
        assert!(!f.is_local() && f.info.idp.kind == IdpKind::Firebase);
        assert!(
            serde_json::to_value(&f.info.idp)
                .unwrap()
                .get("kind")
                .is_none()
        );
        for (name, value) in [("FI_AUTH_BACKEND", "Local"), ("FI_AUTH_BACKEND", "oidc")] {
            refused(&with(local(), name, value), name);
        }
        refused(
            &with(local_backend(), "FI_AUTH_LOCAL_IN_PROD", "yes"),
            "FI_AUTH_LOCAL_IN_PROD",
        );
        let missing: Vec<_> = local_backend()
            .into_iter()
            .filter(|(k, _)| *k != "FI_LOCAL_IDP_SIGNING_KEYS")
            .collect();
        refused(&missing, "FI_LOCAL_IDP_SIGNING_KEYS");
        refused(
            &with(local_backend(), "FI_LOCAL_IDP_SIGNING_KEYS", "nokid"),
            "FI_LOCAL_IDP_SIGNING_KEYS",
        );
        // Its own keys: the access-token ring is refused as the IdP's ring.
        refused(
            &with(local_backend(), "FI_LOCAL_IDP_SIGNING_KEYS", LOCAL_TOKEN),
            "FI_LOCAL_IDP_SIGNING_KEYS",
        );
        refused(
            &with(local_backend(), "FI_IDP_PROVIDERS", " , "),
            "FI_IDP_PROVIDERS",
        );
        // Deployables that never sign in do not read it.
        let gate = IdentityConfig::from_env_for(&missing, IdentityNeeds::GATE).unwrap();
        assert!(!gate.is_local());
    }

    #[test]
    fn prod_refuses_the_local_backend_unless_explicitly_allowed() {
        let prod: Vec<(&'static str, String)> = [
            ("FI_ENV", "prod"),
            ("FI_AUTH_BACKEND", "local"),
            ("FI_PLATFORM_SIGNING_KEYS", PROD_TOKEN),
            ("FI_SITE_KEY_PEPPER", PROD_PEPPER),
            ("FI_REALM", "forzainfinity-prod"),
            ("FI_LOCAL_IDP_SIGNING_KEYS", PROD_IDP),
            // CR-S53: local sign-in in prod mails its links over SMTP.
            ("FI_MAIL_BACKEND", "smtp"),
            ("FI_SMTP_PASSWORD", "re_synthetic_not_a_key"),
            ("FI_MAIL_FROM", "Infinity Horizon <no-reply@example.test>"),
            ("FI_PUBLIC_CORE_URL", "https://api.example.test"),
        ]
        .iter()
        .map(|(k, v)| (*k, (*v).to_owned()))
        .collect();
        let refused_core =
            |env: &Vec<(&'static str, String)>, name: &str| match IdentityConfig::from_env_for(
                env,
                IdentityNeeds::CORE,
            ) {
                Err(ConfigError::Invalid { name: n, reason }) => {
                    assert_eq!(n, name, "{reason}");
                    assert!(!reason.contains("CgoK"), "{reason}");
                }
                Err(ConfigError::Missing(n)) => assert_eq!(n, name),
                Ok(_) => panic!("{name} must be refused"),
            };
        refused_core(&prod, "FI_AUTH_BACKEND");
        refused_core(
            &with(prod.clone(), "FI_AUTH_LOCAL_IN_PROD", "false"),
            "FI_AUTH_BACKEND",
        );
        let allowed = with(prod.clone(), "FI_AUTH_LOCAL_IN_PROD", "true");
        let c = IdentityConfig::from_env_for(&allowed, IdentityNeeds::CORE).unwrap();
        assert!(c.is_local() && c.verifier.is_none());
        assert!(c.mail.as_ref().unwrap().requires_confirmation());
        // The log mail backend, or no SMTP key, is refused in prod.
        refused_core(
            &with(allowed.clone(), "FI_MAIL_BACKEND", "log"),
            "FI_MAIL_BACKEND",
        );
        refused_core(
            &allowed
                .iter()
                .filter(|(k, _)| *k != "FI_SMTP_PASSWORD")
                .cloned()
                .collect(),
            "FI_SMTP_PASSWORD",
        );
        assert_eq!(c.info.idp.kind, IdpKind::Local);
        // Even then the well-known local ring is refused in prod.
        refused_core(
            &with(allowed, "FI_LOCAL_IDP_SIGNING_KEYS", LOCAL_IDP),
            "FI_LOCAL_IDP_SIGNING_KEYS",
        );
        // APP_ENV=prod counts too.
        let app_prod: Vec<_> = prod
            .iter()
            .map(|(k, v)| {
                if *k == "FI_ENV" {
                    ("APP_ENV", v.clone())
                } else {
                    (*k, v.clone())
                }
            })
            .collect();
        refused_core(&app_prod, "FI_AUTH_BACKEND");
    }

    #[test]
    fn each_deployable_reads_only_its_own_keys() {
        let for_needs = |env: &Vec<(&'static str, String)>, needs| {
            IdentityConfig::from_env_for(env, needs).map(|_| ())
        };
        let mut prod: Vec<(&'static str, String)> = local()
            .into_iter()
            .filter(|(k, _)| {
                !matches!(
                    *k,
                    "FIREBASE_AUTH_EMULATOR_HOST"
                        | "FI_TOKEN_SIGNING_KEYS"
                        | "FI_LAUNCH_TICKET_SIGNING_KEYS"
                        | "FI_SITE_KEY_PEPPER"
                        | "FI_PORTAL_ASSERTION_MODE"
                        | "FI_PORTAL_ASSERTION_SECRET"
                )
            })
            .collect();
        prod = with(prod, "FI_ENV", "prod");
        prod = with(prod, "FI_IDP_PROJECT_ID", "fi-placeholder");
        let core = [
            ("FI_PLATFORM_SIGNING_KEYS", PROD_TOKEN),
            ("FI_SITE_KEY_PEPPER", PROD_PEPPER),
            ("FI_REALM", "forzainfinity-prod"),
            ("FI_IDP_API_KEY", PROD_API_KEY),
        ]
        .iter()
        .fold(prod.clone(), |e, (k, v)| with(e, k, v));
        // fi-core: signs access tokens; no launch-ticket ring, and the game ticket's ring refused.
        let c = IdentityConfig::from_env_for(&core, IdentityNeeds::CORE).unwrap();
        assert!(c.access_keys.signing().is_some() && c.ticket_keys.is_none());
        assert!(c.pepper.is_some() && c.verifier.is_some());
        match for_needs(
            &with(core.clone(), "FI_TOKEN_SIGNING_KEYS", PROD_TICKET),
            IdentityNeeds::CORE,
        ) {
            Err(ConfigError::Invalid { name, .. }) => assert_eq!(name, "FI_TOKEN_SIGNING_KEYS"),
            other => panic!("core must refuse the ticket ring: {other:?}"),
        }
        let no_realm: Vec<_> = core
            .iter()
            .filter(|(k, _)| *k != "FI_REALM")
            .cloned()
            .collect();
        assert!(matches!(
            for_needs(&no_realm, IdentityNeeds::CORE),
            Err(ConfigError::Missing("FI_REALM"))
        ));
        // fi-gate: public keys of core's ring and the launch-ticket ring; no IdP, no pepper.
        let public = PublicKeys::of(&SigningKeys::parse("X", PROD_TOKEN).unwrap()).to_text();
        let gate: Vec<(&'static str, String)> = vec![
            ("FI_ENV", "prod".into()),
            ("FI_PLATFORM_VERIFY_KEYS", public.clone()),
            ("FI_LAUNCH_TICKET_SIGNING_KEYS", PROD_TICKET.into()),
        ];
        let g = IdentityConfig::from_env_for(&gate, IdentityNeeds::GATE).unwrap();
        assert!(g.access_keys.signing().is_none() && g.verifier.is_none() && g.pepper.is_none());
        assert_eq!(g.access_keys.public().to_text(), public);
        assert!(g.signing_keys().is_err() && g.pepper().is_err() && g.verifier().is_err());
        // A token core signs verifies on the gate with the public keys only.
        let issued = access::issue(
            c.access_keys.signing().unwrap(),
            &Uuid::now_v7().to_string(),
            &Uuid::now_v7().to_string(),
            Audience::Launcher,
            "player",
            1_800_000_000,
        );
        let token = issued.token.into_inner();
        assert!(
            g.access_keys
                .verify(&token, Audience::Launcher, 1_800_000_010)
                .is_ok()
        );
        match for_needs(
            &with(gate.clone(), "FI_PLATFORM_SIGNING_KEYS", PROD_TOKEN),
            IdentityNeeds::GATE,
        ) {
            Err(ConfigError::Invalid { name, .. }) => assert_eq!(name, "FI_PLATFORM_SIGNING_KEYS"),
            other => panic!("the gate must refuse core's private ring: {other:?}"),
        }
        let no_keys: Vec<_> = gate
            .iter()
            .filter(|(k, _)| *k != "FI_PLATFORM_VERIFY_KEYS")
            .cloned()
            .collect();
        assert!(matches!(
            for_needs(&no_keys, IdentityNeeds::GATE),
            Err(ConfigError::Missing("FI_PLATFORM_VERIFY_KEYS"))
        ));
        // Verify keys alone cannot serve a deployable that signs.
        assert!(matches!(
            for_needs(
                &with(prod, "FI_PLATFORM_VERIFY_KEYS", &public),
                IdentityNeeds::CORE
            ),
            Err(ConfigError::Missing("FI_PLATFORM_SIGNING_KEYS"))
        ));
        // Locally the single ring serves both (compose).
        let local_gate = IdentityConfig::from_env_for(&local(), IdentityNeeds::GATE).unwrap();
        assert!(local_gate.access_keys.signing().is_some());
    }

    #[test]
    fn missing_or_malformed_settings_fail_startup() {
        let mut no_keys = local();
        no_keys.retain(|(k, _)| *k != "FI_TOKEN_SIGNING_KEYS");
        refused(&no_keys, "FI_TOKEN_SIGNING_KEYS");
        refused(
            &with(local(), "FI_SITE_KEY_PEPPER", "1:short"),
            "FI_SITE_KEY_PEPPER",
        );
        refused(
            &with(local(), "FI_PORTAL_ASSERTION_SECRET", "short"),
            "FI_PORTAL_ASSERTION_SECRET",
        );
        refused(
            &with(local(), "FI_ALLOWED_CAR_DB", "abc"),
            "FI_ALLOWED_CAR_DB",
        );
        let mut no_emulator = local();
        no_emulator.retain(|(k, _)| *k != "FIREBASE_AUTH_EMULATOR_HOST");
        let configured = with(no_emulator, "FI_IDP_API_KEY", "local-key");
        assert!(
            IdentityConfig::from_env(&configured, true)
                .unwrap()
                .jwks
                .is_some()
        );
        let prod = with(prod_core(), "FI_IDP_API_KEY", PROD_API_KEY);
        assert!(
            IdentityConfig::from_env_for(&prod, IdentityNeeds::CORE)
                .unwrap()
                .jwks
                .is_some()
        );
        assert!(matches!(
            IdentityConfig::from_env_for(
                &with(prod, "FI_IDP_JWKS", r#"{"keys":[]}"#),
                IdentityNeeds::CORE
            ),
            Err(ConfigError::Invalid {
                name: "FI_IDP_JWKS",
                ..
            })
        ));
    }
    #[test]
    fn registration_flag_is_independent_and_strict() {
        for (raw, expected) in [("true", true), ("false", false)] {
            let mut vars = local();
            vars.push(("FI_REGISTRATION_ENABLED", raw.into()));
            assert_eq!(
                IdentityConfig::from_env(&vars, true)
                    .unwrap()
                    .info
                    .registration_enabled,
                expected
            );
        }
        let mut vars = local();
        vars.push(("FI_REGISTRATION_ENABLED", "maybe".into()));
        assert!(IdentityConfig::from_env(&vars, true).is_err());
        assert!(
            IdentityConfig::from_env(&local(), true)
                .unwrap()
                .info
                .registration_enabled
        );
    }

    fn hex_of(ring: &SigningKeys) -> Vec<String> {
        ring.public_keys()
            .into_iter()
            .map(|(kid, key)| {
                let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
                format!("{kid}:{hex}")
            })
            .collect()
    }

    #[test]
    fn trust_keys_are_published_only_when_asked_and_strictly() {
        // Default and `false`: absent, so the official wire is unchanged.
        assert_eq!(
            IdentityConfig::from_env(&local(), true).unwrap().info.trust,
            None
        );
        let off = with(local(), "FI_PUBLISH_TRUST_KEYS", "false");
        assert_eq!(
            IdentityConfig::from_env(&off, true).unwrap().info.trust,
            None
        );
        for bad in ["yes", "TRUE", "1"] {
            refused(
                &with(local(), "FI_PUBLISH_TRUST_KEYS", bad),
                "FI_PUBLISH_TRUST_KEYS",
            );
        }

        let ticket = SigningKeys::parse("T", LOCAL_TICKET).unwrap();
        let relay = SigningKeys::parse("R", PROD_TICKET.replace("lt-", "rg-").as_str()).unwrap();
        let on = with(local(), "FI_PUBLISH_TRUST_KEYS", "true");

        // Neither relay ring: an empty list (no worlds), never a missing `trust`.
        let t = IdentityConfig::from_env(&on, true)
            .unwrap()
            .info
            .trust
            .unwrap();
        assert_eq!(t.launch_ticket, hex_of(&ticket));
        assert!(t.launch_ticket[0].starts_with("lt-local-1:"));
        assert_eq!(t.launch_ticket[0].split_once(':').unwrap().1.len(), 64);
        assert!(t.relay_grant.is_empty());

        // Derived from the signing ring (a standalone instance).
        let signing = with(
            on.clone(),
            "FI_RELAY_GRANT_SIGNING_KEYS",
            &PROD_TICKET.replace("lt-", "rg-"),
        );
        let t = IdentityConfig::from_env(&signing, true)
            .unwrap()
            .info
            .trust
            .unwrap();
        assert_eq!(t.relay_grant, hex_of(&relay));
        assert!(t.relay_grant[0].starts_with("rg-2026-10:"));

        // From the public ring (a split deployment), the same hex.
        let verify = with(
            on.clone(),
            "FI_RELAY_GRANT_VERIFY_KEYS",
            &PublicKeys::of(&relay).to_text(),
        );
        let t = IdentityConfig::from_env(&verify, true)
            .unwrap()
            .info
            .trust
            .unwrap();
        assert_eq!(t.relay_grant, hex_of(&relay));
        let wire = serde_json::to_value(&t).unwrap();
        let hex = wire["relayGrant"][0]
            .as_str()
            .unwrap()
            .split_once(':')
            .unwrap()
            .1
            .to_owned();
        assert!(
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        refused(
            &with(
                on.clone(),
                "FI_RELAY_GRANT_VERIFY_KEYS",
                "rg-1:bm90LWEta2V5",
            ),
            "FI_RELAY_GRANT_VERIFY_KEYS",
        );

        // A deployable without the launch-ticket ring (fi-core) refuses rather than publish less.
        let mut core = prod_core();
        core.push(("FI_PUBLISH_TRUST_KEYS", "true".into()));
        assert!(matches!(
            IdentityConfig::from_env_for(&core, IdentityNeeds::CORE),
            Err(ConfigError::Invalid {
                name: "FI_PUBLISH_TRUST_KEYS",
                ..
            })
        ));
        // One that does not serve `/info` (fi-gate) never reads it.
        let gate = with(local(), "FI_PUBLISH_TRUST_KEYS", "maybe");
        assert_eq!(
            IdentityConfig::from_env_for(&gate, IdentityNeeds::GATE)
                .unwrap()
                .info
                .trust,
            None
        );
    }
}
