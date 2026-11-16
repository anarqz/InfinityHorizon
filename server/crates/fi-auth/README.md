# fi-auth

Server-side identity and token primitives of the account plane ([ACCOUNTS.md](../../docs/contracts/ACCOUNTS.md)
§4, ADR 0006, ADR 0901). Pure: no I/O and no clock (callers pass `now`); server-only, never a
dependency of the leaf crates the launcher shares (`fi-protocol`, `fi-domain`, `fi-saves`).

| Module | What | Gate |
| --- | --- | --- |
| `idtoken` | Identity Platform ID tokens: `Mode::Rs256(Arc<dyn KeySet>)` (Google's keys; `StaticKeys`, JWKS parsing) or `Mode::Emulator` (unsigned emulator tokens, `demo-` projects only); every ACCOUNTS.md §4.1 check, failures as `Invalid` / `ProviderNotAllowed` / `EmailUnverified` only; `is_recent` for `AUTH.REAUTH_REQUIRED` | RS256 with a 2048-bit key generated in the test, each claim failure, alg confusion, tampering |
| `paseto` | PASETO `v4.public` on ed25519-dalek: PAE, footer (constant-time), implicit assertion, strict verification | The specification's vectors 4-S-1..3 (exact tokens) and 4-F-1..5 |
| `access` | Access tokens: `{sub, fam, aud, role, iat, exp}`, 15 minutes, `{"kid"}` footer, a ring where every key verifies | Rotation, expiry, audience, forged kid |
| `ticket` | Launch tickets: canonical claims, Ed25519 over `fi-launch-ticket-v1\n` ‖ claims, ≤ 900 s | `fi_launcher_core::session::verify_ticket` accepts them |
| `opaque` | `fi_rt_` / `fi_it_` / `fi_ps_` + 43 base64url characters, SHA-256 lookup | Shapes, kinds never mix |
| `codes` | Authorization codes (32 base32), device codes (43 base64url), user codes (`XXXX-XXXX`, site-key alphabet) | Shapes, typed forms |
| `pkce` | RFC 7636 `S256` | RFC 7636 appendix B |
| `pepper` | Versioned HMAC-SHA256: Beta site keys (`HMAC(pepper, hex(SHA-1(key)))`), rate-limit keys, identity scopes | Wrapping per version, no subject in clear |
| `keys` | `FI_TOKEN_SIGNING_KEYS`, `FI_LAUNCH_TICKET_SIGNING_KEYS` (`kid:base64seed,...`), `FI_SITE_KEY_PEPPER` (`version:base64,...`), the well-known local values | Errors never echo a value |

RS256 uses `jsonwebtoken` with `default-features = false, features = ["rust_crypto"]` (no aws-lc,
no cmake in the slim build image); `cargo tree -p fi-auth | grep -iE 'aws-lc|openssl'` prints
nothing. Test keys are generated in code (no key files: `.gitignore` drops `*.pem`, `*.key`).

```sh
cargo test -p fi-auth      # 24 tests
cargo clippy -p fi-auth --all-targets -- -D warnings
```
