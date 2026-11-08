// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-08: "store remote information instead of always loading when clicking". What the pages read from the servers is kept in
/// <see cref="RemoteCache"/>: <c>/me</c> lives in the session (read at sign-in and after our own changes only), the cloud revisions of a
/// career (keyed by game and profile), the Discord link, the server lists of the picker. Pages render from it; our writes invalidate exactly
/// what they change; sign-in and sign-out clear it; the pages' Refresh asks again at most every 10 s.
/// </summary>
public sealed partial class ShellViewModel
{
    private readonly RemoteCache _remote = new();

    /// <summary>Test seam: the cache the pages read through.</summary>
    internal RemoteCache Remote => _remote;

    private static string DiscordCacheKey => RemoteCache.Key("discord");

    /// <summary>The cloud revisions of <paramref name="xuid"/>'s career in <paramref name="game"/>.</summary>
    private static string RevisionsKey(string game, string xuid) => RemoteCache.Key("revisions", game, xuid);

    /// <summary>A save write of ours (an upload by Play, a download, a restore, a deletion, a seed): this game's cloud revisions are read again.</summary>
    private void SavesChanged(string? game = null) => _remote.InvalidatePrefix(RemoteCache.Key("revisions", game ?? CurrentGame.Id) + ":");

    /// <summary>Another account (or none): nothing read for the previous one is shown or kept.</summary>
    private void ForgetRemote() => _remote.Clear();

    /// <summary>A failed read of the revisions (not cached; shown as missing and logged).</summary>
    private sealed class RevisionsUnavailableException(string code) : Exception(code)
    {
        public string Code { get; } = code;
    }
}
