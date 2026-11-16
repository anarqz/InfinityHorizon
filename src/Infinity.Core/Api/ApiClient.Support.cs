// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

/// <summary>Core's support route (CORE_API.md §3, register CR-L52) with the launcher access token, on core's origin.</summary>
public sealed partial class ApiClient
{
    /// <summary><c>GET /core/v1/me/support</c> → 200 <see cref="SupportInfo"/>.</summary>
    public Task<SupportInfo> SupportAsync(string access, CancellationToken cancellation = default) =>
        SendAsync<SupportInfo>(HttpMethod.Get, Core, "/core/v1/me/support", null, access, idempotent: false, cancellation);
}
