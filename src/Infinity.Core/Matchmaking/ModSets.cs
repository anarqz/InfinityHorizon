// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>Where a session's lobby mod set stands (OD-112, docs/launcher/MODDED_LOBBIES.md §2).</summary>
public enum ModSetPhase
{
    /// <summary>Mods are planned but not confirmed yet: no join (never as vanilla).</summary>
    NotReady,

    /// <summary><see cref="ModSetState.Mods"/> is the set to send (empty: vanilla).</summary>
    Ready,

    /// <summary>The set cannot be known (a mod failed, the plan did not build): no join.</summary>
    Failed,
}

/// <summary>A snapshot of the lobby mod set: <see cref="Mods"/> is sorted by id and only meaningful when <see cref="Ready"/>.</summary>
public sealed record ModSetState(ModSetPhase Phase, IReadOnlyList<ModEntry> Mods, string? Reason = null)
{
    public static ModSetState Vanilla { get; } = new(ModSetPhase.Ready, []);

    public static ModSetState NotReady { get; } = new(ModSetPhase.NotReady, []);

    public static ModSetState ReadyWith(IReadOnlyList<ModEntry> mods) => new(ModSetPhase.Ready, ModSets.Checked(mods));

    public static ModSetState FailedWith(string reason) => new(ModSetPhase.Failed, [], reason);

    public bool Ready => Phase == ModSetPhase.Ready;
}

/// <summary>
/// The player's multiplayer mod set for one session (OD-111/OD-112), the same for host and guest.
/// <see cref="Planned"/> is known before the game starts (the gate's <c>modded</c> flag, GATE_API.md §2);
/// <see cref="State"/> is what lobbies use. The launcher's source is <see cref="ExtraOptionsModSetSource"/>: not ready until
/// ExtraOptions' log of this run says what it loaded, raising <see cref="Changed"/> on every transition. Mod-agnostic: no mod is named here.
/// </summary>
public interface IModSetSource
{
    /// <summary>The launch plan's set (sorted, ≤ 32): what this session was started with.</summary>
    IReadOnlyList<ModEntry> Planned { get; }

    ModSetState State { get; }

    /// <summary>Raised after <see cref="State"/> changed (any thread).</summary>
    event EventHandler<ModSetState>? Changed;
}

/// <summary>A session without mods: always ready and vanilla.</summary>
public sealed class VanillaModSet : IModSetSource
{
    public static VanillaModSet Instance { get; } = new();

    private VanillaModSet() { }

    public IReadOnlyList<ModEntry> Planned => [];

    public ModSetState State => ModSetState.Vanilla;

    public event EventHandler<ModSetState>? Changed { add { } remove { } }
}

/// <summary>A mod set that the contract would refuse (MATCHMAKING_API.md §2.7).</summary>
public sealed class ModSetException(string message) : Exception(message);

/// <summary>Checking and comparing mod sets (MODDED_LOBBIES.md §2; MATCHMAKING_API.md §2.7).</summary>
public static class ModSets
{
    /// <summary>
    /// The gate's <c>modded</c> flag (GATE_API.md §2 "Modded sessions"): exactly when the launch's multiplayer set is
    /// not empty. The server then requires that set on every host PUT and refuses it from a non-modded session, so
    /// a cosmetic-only plan (an empty set) never sets it. A plan that failed to build is not modded, and its
    /// <see cref="ModSetPhase.Failed"/> state keeps the session out of every lobby instead.
    /// </summary>
    public static bool Modded(IModSetSource? source) => source is not null && source.Planned.Count > 0;

    /// <summary>The set as the contract takes it (≤ 32, valid ids, versions and fingerprints, unique, sorted), or <see cref="ModSetException"/>.</summary>
    public static IReadOnlyList<ModEntry> Checked(IReadOnlyList<ModEntry> mods)
    {
        ArgumentNullException.ThrowIfNull(mods);
        if (mods.Count > MatchmakingLimits.ModsMax) throw new ModSetException("more than 32 multiplayer mods");
        for (var i = 0; i < mods.Count; i++)
        {
            var mod = mods[i];
            if (!Id(mod.Id)) throw new ModSetException("invalid mod id");
            if (mod.Version is not { Length: >= 1 and <= 16 } || !mod.Version.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '_' or '+' or '-'))
                throw new ModSetException("invalid mod version");
            if (mod.Fingerprint is not { Length: 64 } || !mod.Fingerprint.All(c => char.IsAsciiDigit(c) || c is >= 'a' and <= 'f'))
                throw new ModSetException("invalid mod fingerprint");
            if (i > 0 && string.CompareOrdinal(mods[i - 1].Id, mod.Id) >= 0) throw new ModSetException("mod ids repeat or are not sorted");
        }
        return mods;
    }

    private static bool Id(string? id) => id is { Length: >= 1 and <= 64 } && (char.IsAsciiDigit(id[0]) || char.IsAsciiLetterLower(id[0]))
        && id.All(c => char.IsAsciiDigit(c) || char.IsAsciiLetterLower(c) || c is '.' or '_' or '-');

    /// <summary>The join rule on the client: the same entries (id, version, fingerprint) in the same order (both sorted).</summary>
    public static bool Same(IReadOnlyList<ModEntry>? a, IReadOnlyList<ModEntry>? b)
    {
        a ??= []; b ??= [];
        if (a.Count != b.Count) return false;
        for (var i = 0; i < a.Count; i++)
            if (!string.Equals(a[i].Id, b[i].Id, StringComparison.Ordinal) || !string.Equals(a[i].Version, b[i].Version, StringComparison.Ordinal)
                || !string.Equals(a[i].Fingerprint, b[i].Fingerprint, StringComparison.Ordinal)) return false;
        return true;
    }

    /// <summary>The entries of <paramref name="from"/> that <paramref name="other"/> does not hold exactly (by id, version and fingerprint).</summary>
    public static IEnumerable<ModEntry> Missing(IReadOnlyList<ModEntry>? from, IReadOnlyList<ModEntry>? other) =>
        (from ?? []).Where(m => !(other ?? []).Any(o => o.Id == m.Id && o.Version == m.Version && o.Fingerprint == m.Fingerprint));
}
