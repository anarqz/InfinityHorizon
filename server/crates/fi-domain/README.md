# fi-domain

Identity value types and routing rules (ADR 0006, ADR 0008, [ACCOUNTS.md](../../docs/contracts/ACCOUNTS.md)).
Leaf crate: no internal dependency, no I/O, no clock (callers pass `now` as `UnixMs`).

| Module | What |
| --- | --- |
| `ids` | `AccountId`, `ProfileId`, `DeviceId`, `PlaySessionId`: distinct UUIDv7 types, nil refused |
| `xuid` | `Xuid`: Beta range vs new allocator range (from 2535100000000001), `User_<HEX>` save folder |
| `gamertag` | `Name#12345` and the portal handle `Name-12345`, Beta's name rules |
| `site_key` | `XXXX-XXXX-XXXX` keys from Beta's 32-letter alphabet; CSPRNG generation; `Debug` hides it |
| `mode` | `hardcore` / `noob`, one-way change |
| `routing` | `Tier`, `EntitlementStatus` (monetization's wire strings), `Access::tier_at` (the VIP rule), `PoolName`, `PoolRouting` |
| `time` | `UnixMs`: server decisions use the database clock, passed in |

```sh
cargo test -p fi-domain   # 22 tests
```
