// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// OD-112 (docs/launcher/MODDED_LOBBIES.md §2-§3): the session's mod set on hosting, the list and
// joins, the protocol 2 list rows and the player details pane.
public sealed partial class WorldsBridge
{
    /// <summary>The session's lobby mod set (OD-112); the default is vanilla. Hosting and joining wait for it to be ready.</summary>
    public IModSetSource ModSet { get; init; } = VanillaModSet.Instance;

    private IReadOnlyList<ModEntry> _hostedMods = [];

    /// <summary>The set to send (null: vanilla, left out of the body), or the <c>worlds.*</c> key of why nothing may be sent now.</summary>
    private (IReadOnlyList<ModEntry>? Mods, string? Refusal) WireMods()
    {
        var state = ModSet.State;
        return state.Phase switch
        {
            ModSetPhase.Ready => (state.Mods.Count == 0 ? null : state.Mods, null),
            ModSetPhase.NotReady => (null, "worlds.mods_not_ready"),
            _ => (null, "worlds.mods_failed"),
        };
    }

    /// <summary>The world still carries the set it was opened with (a world's set is fixed, §2.7).</summary>
    private bool HostedModsCurrent() => ModSet.State is { Ready: true } state && ModSets.Same(state.Mods, Volatile.Read(ref _hostedMods));

    /// <summary>The protocol 2 list (0x14 → 0x15): the modded filter, each row's players and mods, and whether its mods equal this session's.</summary>
    public async Task<UiFrame.WorldsListV2> ListV2Async(UiFrame.WorldsQueryV2 query, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(query);
        UiFrame.WorldsListV2 Failed(string message) => new(query.Filter, query.Modded, 2, message, []);
        RequestReconcile();
        using var online = OnlineCancellation(cancellation);
        cancellation = online.Token;
        if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } denied)
            return Failed(DeniedMessage(denied, "list"));
        var filter = query.Filter switch
        {
            1 => WorldFilters.Friends,
            2 => WorldFilters.Near,
            _ => WorldFilters.Global,
        };
        try
        {
            var answer = await OperationAsync(_listRetry, token => _mm.ListAsync(filter, query.NotFull, query.Modded, token), cancellation).ConfigureAwait(false);
            if (await AccessAsync(cancellation).ConfigureAwait(false) is { Allowed: false } changed)
                return Failed(DeniedMessage(changed, "list"));
            if (answer is not { Ok: true, Value: { } list })
            {
                var key = answer?.Error?.Kind == MmErrorKind.SupporterRequired ? "worlds.friends_supporters" : "worlds.list_failed";
                return Failed(_text(key));
            }
            var local = ModSet.State;
            var rows = list.Items.Select(w => LobbyRow(w, false, local)).Concat(list.Locked.Select(w => LobbyRow(w, true, local)))
                // an older server ignores the filter: never show a world of the other kind
                .Where(r => r.Modded == query.Modded)
                .Take(UiFrame.MaxLobbyRows).ToList();
            return new UiFrame.WorldsListV2(query.Filter, query.Modded, 0, string.Empty, rows);
        }
        catch (Exception e) when (HostFailure(e))
        {
            return Failed(_text("worlds.list_failed"));
        }
    }

    /// <summary>One matchmaking world as a protocol 2 row: <c>ModsMatch</c> only when this session's set is ready and equal.</summary>
    public static LobbyRow LobbyRow(WorldView world, bool lockedSection, ModSetState local)
    {
        ArgumentNullException.ThrowIfNull(world);
        ArgumentNullException.ThrowIfNull(local);
        var mods = world.Mods ?? [];
        var modded = mods.Count > 0;
        var members = (world.Members ?? []).Take(UiFrame.MaxLobbyMembers)
            .Select(m => new LobbyMember(Clip(m.Gamertag, 64), m.Tier == "vip" ? UiTier.Supporter : UiTier.Free,
                m.Role switch { "host" => (byte)0, "seated" => (byte)1, _ => (byte)2 }))
            .ToList();
        var shown = mods.Take(UiFrame.MaxLobbyMods).Select(m => new LobbyMod(Clip(m.Id, 64), Clip(m.Version, 16))).ToList();
        return new LobbyRow(Row(world, lockedSection), modded, local.Ready && ModSets.Same(local.Mods, mods), members, shown);
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
            rows.Add(new(_text("pane.player_tier"), _text(member.Tier == UiTier.Supporter ? "pane.tier_supporter" : "pane.tier_free"),
                member.Tier == UiTier.Supporter ? UiPaneIcon.Supporter : UiPaneIcon.None));
            rows.Add(new(_text("pane.player_lobby"), _text(member.Role switch { 0 => "pane.role_host", 1 => "pane.role_in", _ => "pane.role_reserved" }),
                member.Role == 0 ? UiPaneIcon.Host : UiPaneIcon.None));
        }
        return new UiFrame.DetailPane("player:" + xuid, name, _text("pane.player_subtitle"), string.Empty,
            [new PaneSection(UiPaneSection.Rows, _text("pane.profile"), rows)],
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
