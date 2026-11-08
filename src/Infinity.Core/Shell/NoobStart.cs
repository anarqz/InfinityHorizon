// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Process;

namespace Infinity.Core.Shell;

/// <summary>Where a Noob career's start from a Hardcore career takes its save (owner 2026-10-28, SAVE_MANAGER_UX §2.6).</summary>
public enum NoobStartSource
{
    /// <summary>No save to copy: the start is refused and nothing is created or written.</summary>
    None,

    /// <summary>The Hardcore career's newest sealed save on this PC (its own <c>User_</c> folder).</summary>
    ThisPc,

    /// <summary>The Hardcore career's newest cloud revision, downloaded into the Noob's folder (SAVES_API §4.2 bundle).</summary>
    Cloud,
}

/// <summary>What this PC knows of one Hardcore career of the game, read without a request.</summary>
/// <param name="Hardcore">The Hardcore career's row (<c>/me</c> profile or a local profile row).</param>
/// <param name="LocalSave">Its own folder on this PC holds a sealed save.</param>
/// <param name="CloudRevision"><c>career.cloud.revision</c> from <c>/me</c>; null or 0 for none.</param>
public sealed record HardcoreSourceFacts(Profile Hardcore, bool LocalSave, long? CloudRevision);

/// <summary>The decision for one source: where the save comes from, and the string key that says so (or why not).</summary>
public sealed record NoobStartPlan(NoobStartSource Source, Profile? Hardcore, string TextKey)
{
    public bool Possible => Source != NoobStartSource.None;
}

/// <summary>
/// Owner 2026-10-28: "a new NOOB can start from a HARDCORE, no check needed for Noob". A Noob career (a new profile, or a new career of the
/// chosen game) starts from the account's Hardcore career of the same game: its save on this PC first, else (a game with cloud saves,
/// signed in) its newest cloud revision. The copy lands in the Noob's own folder and the Noob line's first Play uploads it as a normal
/// upload (SAVES_API §4.3; the §4.10 seed is Hardcore-only). Never a silent fallback: no source, no start.
/// </summary>
public static class NoobStart
{
    /// <summary>The Hardcore careers of <paramref name="game"/> among <paramref name="profiles"/> (never another game's).</summary>
    public static IReadOnlyList<Profile> Candidates(IEnumerable<Profile> profiles, string game)
    {
        ArgumentNullException.ThrowIfNull(profiles);
        return [.. profiles.Where(p => p.Mode == "hardcore" && p.Game == game && p.Status is not ("deleted" or "deletion_pending")).DistinctBy(p => p.ProfileId)];
    }

    /// <summary>The plan for <paramref name="source"/> in <paramref name="game"/>; <paramref name="cloudReachable"/>: signed in with a saves service.</summary>
    public static NoobStartPlan Plan(GameDefinition game, HardcoreSourceFacts? source, bool cloudReachable)
    {
        ArgumentNullException.ThrowIfNull(game);
        if (source is null) return new(NoobStartSource.None, null, "noob_hc.no_hardcore");
        if (source.Hardcore.Game != game.Id) return new(NoobStartSource.None, source.Hardcore, "noob_hc.no_hardcore");
        if (source.LocalSave) return new(NoobStartSource.ThisPc, source.Hardcore, "noob_hc.from_pc");
        if (game.FullFeatures && cloudReachable && source.CloudRevision is > 0) return new(NoobStartSource.Cloud, source.Hardcore, "noob_hc.from_cloud");
        return new(NoobStartSource.None, source.Hardcore,
            !game.FullFeatures ? "noob_hc.no_save_cloudless" : cloudReachable ? "noob_hc.no_save" : "noob_hc.no_save_offline");
    }

    /// <summary>The sources a Noob of <paramref name="game"/> may start from (each with a possible plan), in the order given.</summary>
    public static IReadOnlyList<NoobStartPlan> Usable(GameDefinition game, IEnumerable<HardcoreSourceFacts> sources, bool cloudReachable)
    {
        ArgumentNullException.ThrowIfNull(sources);
        return [.. sources.Select(s => Plan(game, s, cloudReachable)).Where(p => p.Possible)];
    }

    /// <summary>Why nothing can start, for the first of <paramref name="sources"/> (or "no Hardcore career" when there is none).</summary>
    public static string RefusalKey(GameDefinition game, IReadOnlyList<HardcoreSourceFacts> sources, bool cloudReachable)
    {
        ArgumentNullException.ThrowIfNull(sources);
        return sources.Count == 0 ? "noob_hc.no_hardcore" : Plan(game, sources[0], cloudReachable).TextKey;
    }

    /// <summary>Every string key the decision and the copy's outcome use (each in en and pt-BR).</summary>
    public static IReadOnlyList<string> TextKeys { get; } =
    [
        "noob_hc.from_pc", "noob_hc.from_cloud", "noob_hc.no_hardcore", "noob_hc.no_save", "noob_hc.no_save_cloudless", "noob_hc.no_save_offline",
        "noob_hc.label", "noob_hc.done", "noob_hc.failed", "noob_hc.not_created", "noob_hc.new_profile", "noob_hc.busy",
    ];
}
