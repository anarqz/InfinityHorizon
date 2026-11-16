# fi-protocol

Wire contracts of the v1 client APIs ([ACCOUNTS.md](../../docs/contracts/ACCOUNTS.md),
[SAVES_API.md](../../docs/contracts/SAVES_API.md), [PORTAL_API.md](../../docs/contracts/PORTAL_API.md)):
request and response bodies (camelCase; XUIDs as strings; requests refuse unknown fields), route
paths (`routes`, the strings fi-api's router mounts; CR-L3), header names (`X-Fi-*`, Beta's `X-Fh2-*`
on the game plane, `Repr-Digest` helpers), wire instants (`instant`, six fractional digits), and
`ErrorCode`, every stable error code with its HTTP status (incl. the five `PORTAL.*` codes). Leaf
crate: serde only, so the launcher and its Windows build can depend on it.

| Module | Contract |
| --- | --- |
| `accounts` | ACCOUNTS.md §2, §4.1, §5: `/launcher/v1` bodies (`TokenGrant`, `SignInResult`, `LauncherAccountCreate`, `LegacyLink`, `Me`, ...); the withdrawn pre-OD-20 shapes stay, marked, until LAU-M2 |
| `portal` | PORTAL_API.md §3: `PortalSignIn`, `PortalSignInResult`, `Viewer`, `PortalAccountCreate`, ... |
| `saves` | SAVES_API.md §3-§4 |
| `routes` | Every path above |
| `error`, `headers`, `instant` | §1, §9 conventions |

The TypeScript mirror is `packages/contracts`. `tests/parity.rs` fails when the Rust and TypeScript
error tables differ, when a `fi_saves::SaveError` code is missing here, or when an API instant and an
event instant (`fi_events::instant`) would differ, so edit both together.

SER-M3 adds the game protocol here (the xtsw codec). Its tests must use synthetic messages generated
in code, never captured game traffic (OD-17).

```sh
cargo test -p fi-protocol   # 13 unit + 3 parity tests
```

OD-74: `accounts::TokenGrant` accepts only native `id_token`. Retired launcher-browser/device
DTOs and routes live under `reference` for the frozen Rust launcher only, never active services.
