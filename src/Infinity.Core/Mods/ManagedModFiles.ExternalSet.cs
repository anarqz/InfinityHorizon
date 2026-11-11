// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Mods;

/// <summary>What "Install all" did for one mod of a set (OD-232).</summary>
public enum ExternalSetOutcome
{
    /// <summary>Installed from the pick.</summary>
    Installed,
    /// <summary>An installed older version updated from the pick.</summary>
    Updated,
    /// <summary>This version was already installed: nothing to do.</summary>
    UpToDate,
    /// <summary>The pick holds none of its files.</summary>
    NotFound,
    /// <summary>Found but not installed: <see cref="ExternalSetResult.Reason"/> says why.</summary>
    Failed,
}

/// <summary>
/// One mod's line of the report. <see cref="Reason"/> for <see cref="ExternalSetOutcome.Failed"/>: <c>files</c> (some files missing or
/// different: <see cref="Missing"/>, <see cref="Different"/>), <c>dependency:&lt;id&gt;</c> (a mod of the set it needs did not install),
/// <c>retired</c>, or a <see cref="ModErrors"/> code.
/// </summary>
public sealed record ExternalSetResult(ModManifest Mod, ExternalSetOutcome Outcome, string? Reason = null)
{
    public int Missing { get; init; }
    public int Different { get; init; }
    /// <summary>Hotfix 3.1.2: the first expected file (its path inside the pack) the pick lacks or holds with other bytes, for the report.</summary>
    public string? FirstMissing { get; init; }
    public bool Succeeded => Outcome is ExternalSetOutcome.Installed or ExternalSetOutcome.Updated or ExternalSetOutcome.UpToDate;
}

/// <summary>"Install all" progress: the mod being checked or installed (its 0-based place of <see cref="Total"/>).</summary>
public readonly record struct ExternalSetProgress(int Index, int Total, string ModId);

/// <summary>
/// OD-232: the catalog mods whose one external source carries the same <c>hint</c> (the 22 ZS FM5 packs, the 21 ZS FM6 packs) form a
/// set: one card, one folder pick for all of them. <see cref="InstallExternalSet"/> reads the pick once and installs or updates every
/// mod of the set it holds, the set's dependencies (its Common) first.
/// </summary>
public sealed partial class ManagedModFiles
{
    /// <summary>The hint that makes <paramref name="manifest"/> part of a set: its only external source's, or null.</summary>
    public static string? SetHint(ModManifest manifest) =>
        (manifest ?? throw new ArgumentNullException(nameof(manifest))) is { IsExternal: true, Retired: false, ExternalSources: [{ Hint: { Length: > 0 } hint }] } ? hint : null;

    /// <summary>
    /// The sets of <paramref name="catalog"/>: two or more mods sharing a <see cref="SetHint"/> (a lone mod keeps its own card), in the
    /// order their first mod appears, each in <see cref="SetOrder"/>.
    /// </summary>
    public static IReadOnlyList<(string Hint, IReadOnlyList<ModManifest> Mods)> ExternalSets(IEnumerable<ModManifest> catalog)
    {
        ArgumentNullException.ThrowIfNull(catalog);
        return catalog.Where(m => SetHint(m) is not null).GroupBy(m => SetHint(m)!, StringComparer.Ordinal).Where(g => g.Count() >= 2)
            .Select(g => (g.Key, SetOrder(g.ToList()))).ToArray();
    }

    /// <summary>
    /// The install order of a set: a mod after every mod of the set it depends on (the Common first), otherwise in the given (catalog)
    /// order. A dependency cycle inside the set keeps the remaining mods in their given order.
    /// </summary>
    public static IReadOnlyList<ModManifest> SetOrder(IReadOnlyList<ModManifest> mods)
    {
        ArgumentNullException.ThrowIfNull(mods);
        var ids = mods.Select(m => m.Id).ToHashSet(StringComparer.Ordinal);
        var done = new HashSet<string>(StringComparer.Ordinal);
        var ordered = new List<ModManifest>();
        var left = mods.ToList();
        while (left.Count > 0)
        {
            var next = left.FirstOrDefault(m => m.Dependencies.All(d => !ids.Contains(d.Id) || done.Contains(d.Id))) ?? left[0];
            ordered.Add(next); done.Add(next.Id); left.Remove(next);
        }
        return ordered.AsReadOnly();
    }

    /// <summary>
    /// "Install all" for a set (OD-232): <paramref name="picked"/> (a folder of pack ZIPs or folders, any folder above it, or one ZIP) is
    /// read once, remembered for every mod of the set, and each mod, in <see cref="SetOrder"/>, is installed (or updated) when the pick
    /// holds all its files; already installed this version is up to date. A mod whose set dependency did not succeed is not installed.
    /// Every outcome is in the report; nothing that succeeded is undone by a later failure.
    /// </summary>
    public IReadOnlyList<ExternalSetResult> InstallExternalSet(IReadOnlyList<ModManifest> set, string picked, IProgress<ExternalSetProgress>? progress,
        CancellationToken cancellation, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(set); ArgumentNullException.ThrowIfNull(picked);
        if (set.Count == 0 || set.Any(m => !m.IsExternal)) throw new ModException("not a set of external mods", ModErrors.Unsupported);
        var order = SetOrder(set);
        var results = new List<ExternalSetResult>();
        using var source = SourceEntries.Open(picked);
        foreach (var mod in order) RememberChoices(mod, picked);
        for (var index = 0; index < order.Count; index++)
        {
            cancellation.ThrowIfCancellationRequested();
            var mod = order[index];
            progress?.Report(new(index, order.Count, mod.Id));
            results.Add(InstallFromSet(mod, picked, source, results, hardcore, cancellation));
        }
        return results.AsReadOnly();
    }

    private void RememberChoices(ModManifest mod, string picked)
    {
        using var gate = Lock();
        foreach (var sourceId in mod.ExternalSources.Select(s => s.Id)) RememberExternalChoice(mod.Id, sourceId, picked);
    }

    private ExternalSetResult InstallFromSet(ModManifest mod, string picked, SourceEntries source, List<ExternalSetResult> earlier, bool hardcore, CancellationToken cancellation)
    {
        try
        {
            var receipt = ReadReceipt(mod.Id);
            if (receipt is not null && SnapshotFor(receipt) is { Retired: true }) return new(mod, ExternalSetOutcome.Failed, "retired");
            if (receipt is not null && (receipt.ManifestHash == mod.Digest || SnapshotFor(receipt).SameContent(mod))) return new(mod, ExternalSetOutcome.UpToDate);
            // a pack whose Common (or any mod of the set it needs) did not install is not installed either
            if (mod.Dependencies.Select(d => earlier.FirstOrDefault(r => r.Mod.Id == d.Id)).FirstOrDefault(r => r is { Succeeded: false }) is { } failed)
                return new(mod, ExternalSetOutcome.Failed, "dependency:" + failed.Mod.Id);
            if (ExternalFilesNotStored(mod).Count > 0)
            {
                int missing = 0, different = 0, fromSource = 0; var ok = true; string? firstMissing = null;
                using (var gate = Lock())
                    foreach (var sourceId in mod.ExternalSources.Select(s => s.Id))
                    {
                        var check = Check(mod, sourceId, picked, source, null, cancellation);
                        missing += check.Missing; different += check.Different; fromSource += check.FromSource;
                        ok &= check.Files.Count > 0 && check.Found == check.Files.Count;
                        firstMissing ??= check.Files.FirstOrDefault(f => f.State != ExternalFileState.Ok)?.File.Path;
                    }
                if (fromSource == 0) return new(mod, ExternalSetOutcome.NotFound) { FirstMissing = firstMissing };
                if (!ok) return new(mod, ExternalSetOutcome.Failed, "files") { Missing = missing, Different = different, FirstMissing = firstMissing };
            }
            var update = receipt is not null;
            InstallExternal(mod, null, update, hardcore);
            return new(mod, update ? ExternalSetOutcome.Updated : ExternalSetOutcome.Installed);
        }
        catch (Exception error) when (IsStateError(error) && error is not OperationCanceledException)
        { return new(mod, ExternalSetOutcome.Failed, Code(error)); }
    }
}
