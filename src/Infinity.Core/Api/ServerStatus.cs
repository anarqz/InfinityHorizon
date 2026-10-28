// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary>
/// A server's public figures for the picker, the shell's sidebar and the overlay (CR-L40,
/// docs/launcher/LOBBIES_AND_SERVER_STATUS.md §1), read from <c>GET /core/v1/status</c>. A figure the
/// server did not send is null and never shown as zero: <c>live</c> is additive (CR-L40 a), so an
/// older server gives the online count alone (the sum of its pools' active sessions).
/// </summary>
public sealed record ServerStatus(uint Online, uint? Capacity, uint? Hardcore, uint? Noob, uint? Lobbies,
    ulong? ProfilesHardcore, ulong? ProfilesNoob)
{
    public const int MaxBytes = 65536;
    public const string Path = "/core/v1/status";

    /// <summary>The live mode and lobby figures came from the server (CR-L40 a).</summary>
    public bool HasLive => Hardcore is not null && Noob is not null && Lobbies is not null;

    public bool HasProfiles => ProfilesHardcore is not null && ProfilesNoob is not null;

    /// <summary>The figures of <paramref name="game"/> in a <c>/core/v1/status</c> body; null when the game is not listed.</summary>
    public static ServerStatus? Parse(ReadOnlySpan<byte> json, string game = "fh2")
    {
        using var document = JsonDocument.Parse(json.ToArray(), new JsonDocumentOptions { MaxDepth = 16 });
        if (document.RootElement.ValueKind != JsonValueKind.Object
            || !document.RootElement.TryGetProperty("games", out var games) || games.ValueKind != JsonValueKind.Array)
            throw new InvalidDataException("The server status has no games.");
        foreach (var entry in games.EnumerateArray())
        {
            if (entry.ValueKind != JsonValueKind.Object || !entry.TryGetProperty("id", out var id)
                || id.ValueKind != JsonValueKind.String || id.GetString() != game) continue;
            return FromGame(entry);
        }
        return null;
    }

    private static ServerStatus FromGame(JsonElement game)
    {
        if (game.TryGetProperty("live", out var live) && live.ValueKind == JsonValueKind.Object)
        {
            ulong? hardcoreProfiles = null, noobProfiles = null;
            if (live.TryGetProperty("profiles", out var profiles) && profiles.ValueKind == JsonValueKind.Object)
                (hardcoreProfiles, noobProfiles) = (Count(profiles, "hardcore"), Count(profiles, "noob"));
            var online = Count(live, "online") ?? throw new InvalidDataException("The server status' live online is missing.");
            return new(Narrow(online), live.TryGetProperty("capacity", out var cap) && cap.ValueKind == JsonValueKind.Number ? Narrow(Number(cap)) : null,
                Narrow(Count(live, "hardcore")), Narrow(Count(live, "noob")), Narrow(Count(live, "lobbies")), hardcoreProfiles, noobProfiles);
        }
        // An older server: the pools' active sessions, no capacity (each pool has its own) and nothing per mode.
        ulong active = 0;
        if (game.TryGetProperty("queue", out var queue) && queue.ValueKind == JsonValueKind.Object)
            foreach (var pool in queue.EnumerateObject())
                if (pool.Value.ValueKind == JsonValueKind.Object && Count(pool.Value, "active") is { } count) active += count;
        return new(Narrow(active), null, null, null, null, null, null);
    }

    private static ulong? Count(JsonElement parent, string name) =>
        parent.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.Number ? Number(value) : null;

    private static ulong Number(JsonElement value) =>
        value.TryGetUInt64(out var n) ? n : throw new InvalidDataException("A server status figure is not a count.");

    private static uint Narrow(ulong value) => value > uint.MaxValue ? uint.MaxValue : (uint)value;

    private static uint? Narrow(ulong? value) => value is { } v ? Narrow(v) : null;

    /// <summary><c>Online 12/50 · Hardcore 5 (210) · Noob 7 (340) · Lobbies 3</c>; figures the server did not send are left out.</summary>
    public string Line()
    {
        var parts = new List<string> { "Online " + Online + (Capacity is { } cap ? "/" + cap : "") };
        if (Hardcore is { } hardcore) parts.Add("Hardcore " + hardcore + (ProfilesHardcore is { } p ? " (" + p + ")" : ""));
        else if (ProfilesHardcore is { } onlyHardcore) parts.Add("Hardcore (" + onlyHardcore + ")");
        if (Noob is { } noob) parts.Add("Noob " + noob + (ProfilesNoob is { } p ? " (" + p + ")" : ""));
        else if (ProfilesNoob is { } onlyNoob) parts.Add("Noob (" + onlyNoob + ")");
        if (Lobbies is { } lobbies) parts.Add("Lobbies " + lobbies);
        return string.Join(" · ", parts);
    }
}
