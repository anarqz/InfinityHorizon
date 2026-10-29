// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json.Nodes;

namespace Infinity.Core.Presence;

/// <summary>The presence's activity line (DISCORD_PRESENCE.md §1): never hidden, it is the presence itself.</summary>
public enum PresenceActivity
{
    /// <summary>No game: the launcher alone.</summary>
    Launcher,

    /// <summary>The game runs but is not ready (CR-L27 readiness absent or stale).</summary>
    Menus,

    /// <summary>The game is ready (local player and car present) and the profile is in no world.</summary>
    FreeRoam,

    /// <summary>Seated in or hosting a world.</summary>
    Lobby,
}

/// <summary>The world the profile hosts or sits in: <paramref name="Audience"/> is the contract's (<c>everyone</c>, <c>supporters</c>, <c>friends</c>, <c>private</c>).</summary>
public sealed record PresenceWorld(string Id, string Audience, int Players, int Max, int OpenSeats);

/// <summary>
/// What the card is built from. <see cref="Mode"/> is the profile's (<c>hardcore</c> / <c>noob</c>); <see cref="Tier"/> is carried for the
/// card's future use and shown nowhere today (§1 has no tier field). <see cref="CarName"/> stays null until a car name source exists
/// (CR-L41.4): the hook's <c>Car</c> event gives only a versioned livery id.
/// </summary>
public sealed record PresenceInputs
{
    public required string ProfileName { get; init; }
    public required string Mode { get; init; }
    public string? Tier { get; init; }
    public string? ClubTag { get; init; }
    public uint? Level { get; init; }
    public ulong? Credits { get; init; }
    public string? CarName { get; init; }

    /// <summary>CR-L44: the current car's Storefront livery preview (an https URL checked by the API client), or null.</summary>
    public string? LiveryImage { get; init; }
    public PresenceActivity Activity { get; init; }
    public PresenceWorld? World { get; init; }
    public required DateTimeOffset StartedAt { get; init; }
    public required string LauncherVersion { get; init; }
}

/// <summary>
/// Settings → Discord (§5): a master switch and one toggle per field, all on by default (OD-136), kept per launcher install
/// (<c>discord-presence.json</c> in the state folder), never synced.
/// </summary>
public sealed record PresenceToggles
{
    public bool Enabled { get; init; } = true;
    public bool Club { get; init; } = true;
    public bool Mode { get; init; } = true;
    public bool Name { get; init; } = true;
    public bool Level { get; init; } = true;
    public bool Credits { get; init; } = true;
    public bool Car { get; init; } = true;
    public bool Lobby { get; init; } = true;
    public bool Join { get; init; } = true;

    /// <summary>CR-L44 (OD-137): "Show my livery": the current car's livery preview as the large image; off sends no <c>?car=</c>. A
    /// file saved before this toggle existed reads as on.</summary>
    public bool ShowLivery { get; init; } = true;

    public static PresenceToggles AllOn { get; } = new();

    private const string FileName = "discord-presence.json";

    /// <summary>The saved toggles; missing, unreadable or malformed: all on.</summary>
    public static PresenceToggles Load(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, FileName);
            if (!File.Exists(path) || new FileInfo(path).Length > 4096) return AllOn;
            return System.Text.Json.JsonSerializer.Deserialize<PresenceToggles>(File.ReadAllBytes(path), Json) ?? AllOn;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or System.Text.Json.JsonException)
        {
            return AllOn;
        }
    }

    public void Save(string stateDirectory)
    {
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        File.WriteAllBytes(path + ".tmp", System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(this, Json));
        File.Move(path + ".tmp", path, overwrite: true);
    }

    private static readonly System.Text.Json.JsonSerializerOptions Json = new(System.Text.Json.JsonSerializerDefaults.Web);
}

/// <summary>
/// The card from fixed inputs (§1), pure: the same inputs and toggles always give the same JSON, so the coalescer can drop an
/// unchanged card. <c>details</c> = <c>[TAG] · HC · Name · Lv 42</c>, <c>state</c> = <c>activity · 1,234,567 CR · car</c> (the
/// locale's digit grouping), each piece behind its toggle; the party (hex SHA-256 of the world id, never the id) behind the lobby
/// toggle; Join (§4) only for an <c>everyone</c>/<c>supporters</c>/<c>friends</c> world with an open seat, never <c>private</c>;
/// Infinity Discord always. The large image is the livery preview (CR-L44) behind its toggle, else <c>infinity</c>. A line longer than 128 characters is cut at a grapheme with <c>…</c>: the car first, then the name.
/// </summary>
public static class PresenceComposer
{
    public const int MaxField = 128;
    public const string LargeImage = "infinity";
    public const string JoinBase = "https://infinity-horizon.com/join/";
    public const string InfinityDiscordInvite = "https://discord.com/invite/bK8E5fNKg6";

    /// <summary>
    /// The activity JSON, or null when the master switch is off (the card is cleared). <paramref name="text"/> gives the localized
    /// <c>presence.*</c> strings; <paramref name="culture"/> the credits' digit grouping.
    /// </summary>
    public static JsonObject? Compose(PresenceInputs inputs, PresenceToggles toggles, Func<string, string> text, CultureInfo culture)
    {
        ArgumentNullException.ThrowIfNull(inputs);
        ArgumentNullException.ThrowIfNull(toggles);
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(culture);
        if (!toggles.Enabled) return null;
        var hardcore = inputs.Mode == "hardcore";

        // details: [TAG] · HC · Name · Lv 42 (the name is the piece cut when too long)
        var details = new List<string>();
        if (toggles.Club && Clean(inputs.ClubTag) is { } tag) details.Add("[" + tag + "]");
        if (toggles.Mode) details.Add(hardcore ? "HC" : "NB");
        var nameAt = -1;
        if (toggles.Name && Clean(inputs.ProfileName) is { } name) { nameAt = details.Count; details.Add(name); }
        if (toggles.Level && inputs.Level is { } level and > 0)
            details.Add(text("presence.level").Replace("{level}", level.ToString(culture), StringComparison.Ordinal));

        // state: activity · credits CR · car (the car is cut first)
        var state = new List<string> { text(ActivityKey(inputs.Activity)) };
        if (toggles.Credits && inputs.Credits is { } credits)
            state.Add(text("presence.credits").Replace("{credits}", credits.ToString("N0", culture), StringComparison.Ordinal));
        var carAt = -1;
        if (toggles.Car && Clean(inputs.CarName) is { } car) { carAt = state.Count; state.Add(car); }

        var activity = new JsonObject();
        if (Fit(details, nameAt) is { Length: >= 2 } detailsLine) activity["details"] = detailsLine;
        if (Fit(state, carAt) is { Length: >= 2 } stateLine) activity["state"] = stateLine;
        activity["timestamps"] = new JsonObject { ["start"] = inputs.StartedAt.ToUnixTimeMilliseconds() };
        var assets = new JsonObject
        {
            ["large_image"] = toggles.ShowLivery && inputs.LiveryImage is { } livery && livery.StartsWith("https://", StringComparison.Ordinal)
                ? livery : LargeImage,
            ["large_text"] = Cut("Infinity Horizon " + inputs.LauncherVersion, MaxField),
        };
        if (toggles.Mode)
        {
            assets["small_image"] = hardcore ? "hc" : "nb";
            assets["small_text"] = text(hardcore ? "presence.hardcore" : "presence.noob");
        }
        activity["assets"] = assets;

        var world = inputs.Activity == PresenceActivity.Lobby ? inputs.World : null;
        if (toggles.Lobby && world is { Max: > 0, Players: > 0 } party && party.Players <= party.Max && JoinLink.IsWorldId(party.Id))
            activity["party"] = new JsonObject { ["id"] = JoinLink.PartyId(party.Id), ["size"] = new JsonArray(party.Players, party.Max) };

        var buttons = new JsonArray();
        if (toggles.Join && CanOfferJoin(world))
            buttons.Add(new JsonObject { ["label"] = text("presence.join"), ["url"] = JoinLink.Web(world!.Id) });
        buttons.Add(new JsonObject { ["label"] = text("presence.discord"), ["url"] = InfinityDiscordInvite });
        activity["buttons"] = buttons;
        activity["instance"] = false;
        return activity;
    }

    /// <summary>§4: a world the profile is in, open to <c>everyone</c>, <c>supporters</c> or <c>friends</c>, with a seat left; never <c>private</c>.</summary>
    public static bool CanOfferJoin(PresenceWorld? world) =>
        world is { OpenSeats: > 0 } open && open.Audience is "everyone" or "supporters" or "friends" && JoinLink.IsWorldId(open.Id);

    public static string ActivityKey(PresenceActivity activity) => activity switch
    {
        PresenceActivity.Lobby => "presence.lobby",
        PresenceActivity.FreeRoam => "presence.free_roam",
        PresenceActivity.Menus => "presence.menus",
        _ => "presence.launcher",
    };

    private static string? Clean(string? value)
    {
        if (string.IsNullOrWhiteSpace(value)) return null;
        var plain = new string(value.Where(c => !char.IsControl(c)).ToArray()).Trim();
        return plain.Length == 0 ? null : plain;
    }

    /// <summary>The pieces joined by <c> · </c>, the piece at <paramref name="cutAt"/> (or the whole line) cut to fit.</summary>
    private static string Fit(List<string> pieces, int cutAt)
    {
        const string separator = " · ";
        var line = string.Join(separator, pieces);
        if (line.Length <= MaxField) return line;
        if (cutAt >= 0)
        {
            var others = line.Length - pieces[cutAt].Length;
            var room = MaxField - others;
            if (room >= 2)
            {
                pieces[cutAt] = Cut(pieces[cutAt], room);
                return string.Join(separator, pieces);
            }
            pieces.RemoveAt(cutAt);
            line = string.Join(separator, pieces);
            if (line.Length <= MaxField) return line;
        }
        return Cut(line, MaxField);
    }

    /// <summary><paramref name="value"/> cut at a grapheme boundary so it and a trailing <c>…</c> fit <paramref name="max"/> UTF-16 units.</summary>
    internal static string Cut(string value, int max)
    {
        if (value.Length <= max) return value;
        var kept = 0;
        var elements = StringInfo.GetTextElementEnumerator(value);
        while (elements.MoveNext())
        {
            var next = elements.ElementIndex + ((string)elements.Current).Length;
            if (next > max - 1) break;
            kept = next;
        }
        return value[..kept].TrimEnd() + "…";
    }
}
