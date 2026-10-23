# Infinity native mod runtime

The runtime is part of Infinity Horizon (GPL-3.0-only). The two public C SDK
headers are MIT licensed (LICENSE-SDK.txt) so mods may use the ABI independently.

Hook coordination derives from Trinculo54/fh2-mod-platform commit
6e851d89df50d11d5cb7f8f19293ff2d3a0e3fcb, MIT (licenses/FH2-hooks-MIT.txt).
MinHook retains its BSD license and HDE notices in third_party/minhook.
The platform loader, SQL generator, database deletion, UI and settings services
are not included. The launcher owns persistent file publication and recovery. SQLite session-copy execution
is implemented here using the public-domain SQLite amalgamation (third_party/sqlite/VERSION.txt).

Only explicitly selected, hash-verified modules receive initialization calls.
This controls compatible mods; native DLLs are not sandboxed and this is not
an anti-cheat mechanism. The host must verify this runtime before loading it.
