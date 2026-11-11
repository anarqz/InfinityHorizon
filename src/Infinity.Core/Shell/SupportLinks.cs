// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>A Discord channel: its guild (server) id and channel id.</summary>
public sealed record DiscordChannel(string GuildId, string ChannelId)
{
    /// <summary>The Discord app's own link (opens the channel in the installed client).</summary>
    public Uri App => new($"discord://-/channels/{GuildId}/{ChannelId}");

    /// <summary>The same channel in a browser, when the app's <c>discord://</c> handler is not there.</summary>
    public Uri Web => new($"https://discord.com/channels/{GuildId}/{ChannelId}");
}

/// <summary>The launcher's support destinations: data, one entry each.</summary>
public static class SupportLinks
{
    /// <summary>OD-241 (owner, MACBOOK chat 2026-11-11): logs are pasted by hand in the community's #help channel.</summary>
    public static readonly DiscordChannel Help = new("1561214669320880210", "1562127527802179634");
}
