// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary>
/// CORE_API §4.2a (OD-218, CR-S102 review #1): the first <c>GET /launcher/v1/invites</c> after this launcher started carries
/// <c>first=true</c>, so a restart within the server's hour does not replay friends who came online meanwhile; every later poll
/// (any play session of this launcher process) goes without it. The mark stays until the server answered a poll: a failed or rate-limited
/// first poll is repeated with it. An older server answers 400 <c>REQUEST.INVALID {field: "query"}</c> to the unknown parameter: that
/// is logged once and the same poll is repeated at once without it (an older server never breaks the invites inbox).
/// </summary>
public sealed class InvitesFirstPoll
{
    /// <summary>This launcher process's (the "launcher start" of §4.2a).</summary>
    public static InvitesFirstPoll Process { get; } = new();

    private int _done;

    /// <summary>The next poll is the first one (it carries <c>first=true</c>).</summary>
    public bool Pending => Volatile.Read(ref _done) == 0;

    /// <summary>One poll through <paramref name="read"/> (its argument: whether to add <c>first=true</c>).</summary>
    public async Task<InviteList> ReadAsync(Func<bool, CancellationToken, Task<InviteList>> read, Action<string>? log, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(read);
        if (!Pending) return await read(false, cancellation).ConfigureAwait(false);
        InviteList list;
        try
        {
            list = await read(true, cancellation).ConfigureAwait(false);
        }
        catch (ApiException error) when (QueryRefused(error))
        {
            Interlocked.Exchange(ref _done, 1);
            log?.Invoke("invites: this server does not take first=true yet (400 REQUEST.INVALID query); the first poll is repeated without it");
            return await read(false, cancellation).ConfigureAwait(false);
        }
        Interlocked.Exchange(ref _done, 1);
        return list;
    }

    /// <summary>A 400 <c>REQUEST.INVALID {field: "query"}</c>: the server does not know a query parameter.</summary>
    public static bool QueryRefused(ApiException error) =>
        error is { Status: HttpStatusCode.BadRequest, Code: "REQUEST.INVALID" } && error.Details is { ValueKind: JsonValueKind.Object } details
        && details.TryGetProperty("field", out var field) && field.ValueKind == JsonValueKind.String && field.GetString() == "query";
}
