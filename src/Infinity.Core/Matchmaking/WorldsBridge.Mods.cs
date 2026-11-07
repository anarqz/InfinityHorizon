// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// OD-112 (docs/launcher/MODDED_LOBBIES.md §2-§3): the session's mod set on joins, the lobby rows (the pool move's list) and the
// player details pane.
public sealed partial class WorldsBridge
{
    /// <summary>The session's lobby mod set (OD-112); the default is vanilla. Joining waits for it to be ready.</summary>
    public IModSetSource ModSet { get; init; } = VanillaModSet.Instance;

    /// <summary>
    /// The set to send (null: vanilla, left out of the body), or the <c>worlds.*</c> key of why nothing may be sent now. OD-181
    /// (CR-L48): every player declares the set its game loads, so a modded session (the gate's <c>modded</c>, a non-empty plan)
    /// never sends an empty set: one whose ready set came out empty joins nothing (the server would answer 400
    /// <c>{field: "mods"}</c>).
    /// </summary>
    private (IReadOnlyList<ModEntry>? Mods, string? Refusal) WireMods()
    {
        var state = ModSet.State;
        return state.Phase switch
        {
            ModSetPhase.Ready when state.Mods.Count == 0 && ModSets.Modded(ModSet) => (null, "worlds.mods_failed"),
            ModSetPhase.Ready => (state.Mods.Count == 0 ? null : state.Mods, null),
            ModSetPhase.NotReady => (null, "worlds.mods_not_ready"),
            _ => (null, "worlds.mods_failed"),
        };
    }

    /// <summary>
    /// OD-181 (CR-L48): the catalog's name of a mod by its multiplayer id (the localized name, or null when the catalog does
    /// not know it); the lobby list, the player details and the roster's mods show it with the version, else the id.
    /// </summary>
    public Func<string, string?>? ModName { get; init; }

    /// <summary>One mod as shown: the catalog's name when <paramref name="names"/> knows it, else its id; then its version (cut to the pipe's limits).</summary>
    public static LobbyMod ShownMod(ModEntry mod, Func<string, string?>? names)
    {
        ArgumentNullException.ThrowIfNull(mod);
        var name = names?.Invoke(mod.Id);
        var shown = Clip(string.IsNullOrWhiteSpace(name) ? mod.Id : name.Trim(), 64);
        return new LobbyMod(shown.Length == 0 ? Clip(mod.Id, 64) : shown, Clip(mod.Version, 16));
    }

    /// <summary>
    /// One matchmaking world as a protocol 2 row with the host's mods (<c>WorldView.mods</c>) as shown. OD-181 (CR-L48)
    /// retired the equal-set join rule: <c>ModsMatch</c> is always true, so no overlay blocks a join over mods.
    /// </summary>
    public static LobbyRow LobbyRow(WorldView world, bool lockedSection, Func<string, string?>? names = null)
    {
        ArgumentNullException.ThrowIfNull(world);
        var mods = world.Mods ?? [];
        var modded = mods.Count > 0;
        var members = (world.Members ?? []).Take(UiFrame.MaxLobbyMembers)
            .Select(m => new LobbyMember(Clip(m.Gamertag, 64), UiTier.Of(m.Tier),
                m.Role switch { "host" => (byte)0, "seated" => (byte)1, _ => (byte)2 }))
            .ToList();
        var shown = mods.Take(UiFrame.MaxLobbyMods).Select(m => ShownMod(m, names)).ToList();
        return new LobbyRow(Row(world, lockedSection), modded, true, members, shown, WorldModes.OpenToBoth(world));
    }

    /// <summary>
    /// OD-181 (CR-L48): the mods each player of a roster declared, for <see cref="UiFrame.RosterMods"/> and the player
    /// details. The host's row falls back to the world's set (<paramref name="worldMods"/>) when the server sends none for it
    /// (a server before CR-S84). Players without mods are left out.
    /// </summary>
    public static UiFrame.RosterMods RosterModsOf(string worldId, IEnumerable<(string Xuid, bool Host, IReadOnlyList<ModEntry>? Mods)> members,
        IReadOnlyList<ModEntry>? worldMods, Func<string, string?>? names)
    {
        ArgumentNullException.ThrowIfNull(members);
        if (string.IsNullOrEmpty(worldId) || System.Text.Encoding.UTF8.GetByteCount(worldId) > UiFrame.MaxPaneId) return UiFrame.RosterMods.None;
        var listed = new List<RosterModsMember>();
        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var (xuid, host, declared) in members)
        {
            if (listed.Count == UiFrame.MaxRosterMembers) break;
            if (xuid is not { Length: >= 1 and <= 20 } || !xuid.All(char.IsAsciiDigit) || !seen.Add(xuid)) continue;
            var mods = declared is { Count: > 0 } ? declared : host ? worldMods ?? [] : [];
            // server text is untrusted: an entry without a usable id is skipped (the frame refuses an empty name), and
            // ShownMod cuts names and versions to the frame's limits
            var shown = mods.Where(m => m is { Id: { } id } && !string.IsNullOrWhiteSpace(id))
                .Select(m => ShownMod(m, names)).Where(m => m.Id.Length > 0).Take(UiFrame.MaxLobbyMods).ToList();
            if (shown.Count > 0) listed.Add(new RosterModsMember(xuid, shown));
        }
        return new UiFrame.RosterMods(worldId, listed);
    }

    private UiFrame.RosterMods _rosterMods = UiFrame.RosterMods.None;

    /// <summary>OD-181: the mods of each player of the last "my world" answer (protocol 4 overlays; no request).</summary>
    public UiFrame.RosterMods RosterModsFrame()
    {
        var mods = Volatile.Read(ref _rosterMods);
        var mine = Volatile.Read(ref _mineSnapshot);
        return mine.Role == UiWorldRole.None || mods.WorldId != mine.WorldId ? UiFrame.RosterMods.None : mods;
    }

    /// <summary>
    /// Server text cut to the pipe's byte limit at a UTF-8 code point boundary (never a split surrogate pair),
    /// so one long gamertag, mod id or name cannot make a whole frame unencodable.
    /// </summary>
    public static string Clip(string? text, int bytes)
    {
        if (string.IsNullOrEmpty(text)) return string.Empty;
        if (System.Text.Encoding.UTF8.GetByteCount(text) <= bytes) return text;
        var builder = new System.Text.StringBuilder();
        var used = 0;
        foreach (var rune in text.EnumerateRunes())
        {
            if (used + rune.Utf8SequenceLength > bytes) break;
            used += rune.Utf8SequenceLength;
            builder.Append(rune.ToString());
        }
        return builder.ToString();
    }

    /// <summary>The <c>not_allowed {reason: "mods"}</c> message: the mods the world needs that this session lacks, and the ones it must not have.</summary>
    private string ModsRefusal(ModsMismatch? details)
    {
        static string Names(IEnumerable<ModEntry> mods) => string.Join(", ", mods.Select(m => m.Id + " " + m.Version));
        var expected = details?.Expected ?? [];
        var actual = details?.Actual ?? [];
        var needed = ModSets.Missing(expected, actual).ToList();
        var extra = ModSets.Missing(actual, expected).ToList();
        var parts = new List<string> { _text("worlds.not_allowed_mods") };
        if (needed.Count > 0) parts.Add(_text("worlds.mods_needed").Replace("{mods}", Names(needed), StringComparison.Ordinal));
        if (extra.Count > 0) parts.Add(_text("worlds.mods_extra").Replace("{mods}", Names(extra), StringComparison.Ordinal));
        if (expected.Count == 0) parts.Add(_text("worlds.mods_vanilla_only"));
        var message = string.Join(" ", parts);
        // the overlay's text limit (UiFrame strings ≤ 512 bytes)
        return Clip(message, 512);
    }

    /// <summary>
    /// The player card's details (0x17 → 0x16 <c>player:&lt;xuid&gt;</c>): the profile name from this
    /// launcher's world roster when it knows the player, else the card's gamertag; the avatar key empty
    /// until avatars exist; "Add friend" shown disabled (OD-112). Never the XUID in a visible field (OD-61).
    /// </summary>
    public UiFrame.DetailPane PlayerDetails(UiFrame.PlayerDetailsQuery query)
    {
        ArgumentNullException.ThrowIfNull(query);
        if (query.Xuid is not { Length: >= 1 and <= 20 } xuid || !xuid.All(char.IsAsciiDigit))
            return new(string.Empty, string.Empty, string.Empty, string.Empty, [], []); // closes the pane: nothing to show
        var member = Volatile.Read(ref _mineSnapshot).Members.FirstOrDefault(m => m.Xuid == xuid);
        var name = member?.Gamertag is { Length: > 0 } known ? known
            : query.Gamertag is { Length: > 0 } carded ? carded : _text("pane.player_unknown");
        name = Clip(name, 64);
        var rows = new List<PaneEntry> { new(_text("pane.player_name"), name, UiPaneIcon.Player) };
        if (member is not null)
        {
            // CR-S69: every player is an Infinity Racer; a Supporter carries the badge
            rows.Add(new(_text("pane.player_tier"), _text(member.Tier == UiTier.Supporter ? "pane.tier_supporter" : "pane.tier_racer"),
                member.Tier == UiTier.Supporter ? UiPaneIcon.Supporter : UiPaneIcon.None));
            rows.Add(new(_text("pane.player_lobby"), _text(member.Role switch { 0 => "pane.role_host", 1 => "pane.role_in", _ => "pane.role_reserved" }),
                member.Role == 0 ? UiPaneIcon.Host : UiPaneIcon.None));
        }
        List<PaneSection> sections = [new PaneSection(UiPaneSection.Rows, _text("pane.profile"), rows)];
        // OD-181 (CR-L48): the mods this player declared, as the catalog names them (else the id), with their versions
        if (member is not null && RosterModsFrame().Members.FirstOrDefault(m => m.Xuid == xuid) is { } declared)
        {
            sections.Add(new PaneSection(UiPaneSection.Items, _text("pane.player_mods"), declared.Mods.Take(UiFrame.MaxPaneEntries)
                .Select(m => new PaneEntry(m.Id, Clip(_text("pane.mod_version").Replace("{version}", m.Version, StringComparison.Ordinal), UiFrame.MaxPaneText), UiPaneIcon.Mod))
                .ToList()));
        }
        else if (member is not null) rows.Add(new(_text("pane.player_mods"), _text("pane.no_mods"), UiPaneIcon.None));
        return new UiFrame.DetailPane("player:" + xuid, name, _text("pane.player_subtitle"), string.Empty, sections,
            [new PaneAction(PaneActions.FriendAdd, _text("pane.add_friend"), false)]);
    }

    /// <summary>A pane button (0x18). Every action is disabled today, so each click is a no-op with a log line (ids only).</summary>
    public void PaneClick(UiFrame.PaneActionClick click)
    {
        ArgumentNullException.ThrowIfNull(click);
        var pane = click.PaneId.StartsWith("player:", StringComparison.Ordinal) ? "player" : Clip(click.PaneId, 64);
        _log($"overlay: pane action {Clip(click.ActionId, 64)} on {pane} ignored (disabled or unknown)");
    }
}

/// <summary>The ids of the detail pane's actions (OD-112).</summary>
public static class PaneActions
{
    /// <summary>"Add friend": shown disabled until friends from the pane work.</summary>
    public const string FriendAdd = "friend.add";
}
