// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Runtime.ExceptionServices;
using System.Text.Json;

namespace Infinity.Core.Mods;

public sealed partial class ManagedModFiles
{
    /// <summary>True while this Azimuth folder's mods are in the old flat layout, on disk or in a receipt (OD-233).</summary>
    public bool LayoutMigrationPending
    {
        get
        {
            if (ExtraOptionsLayout.NeedsMigration(_azimuth)) return true;
            if (!Directory.Exists(BackupRoot)) return false;
            foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json"))
            {
                try
                {
                    var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path));
                    if (receipt?.Files?.Any(f => f.Root == "azimuth" && ExtraOptionsLayout.LegacyFolder(f.Path) is not null) == true) return true;
                }
                catch (Exception error) when (error is JsonException or IOException or UnauthorizedAccessException) { }
            }
            return false;
        }
    }

    /// <summary>
    /// OD-233, before a launch and when the mods are read (game closed, no interrupted operation): <see cref="ExtraOptionsLayout.Migrate"/>
    /// moves the old flat <c>mods\</c> to the per-game folders, then every receipt whose folder has moved names the files where they now
    /// are. A folder's game is its installed manifest's (<see cref="ModManifest.ModsGame"/>), else <paramref name="catalogGame"/>'s answer
    /// for its name (the catalog entry of that id), else FH2's (the flat layout's loader, 0.4.4, loaded only FH2's mods). Run again after a stop (a
    /// conflict, a refused move, a crash between the steps), it continues; on the per-game layout it changes nothing.
    /// </summary>
    /// <param name="backupDirectory">Where a copy of the old <c>mods\loadorder.txt</c> is kept (the launcher's state).</param>
    /// <param name="receiptsOnly">
    /// Without the catalog (not loaded yet, or unreachable): only the folders whose game the receipts decide move, with their receipts
    /// (and dot folders, always FH2's). That is exactly what a run with the catalog does for them, since a receipt's game comes before
    /// the catalog's answer; a folder copied by hand waits, with the root <c>loadorder.txt</c>, for the next run with the catalog (or
    /// before Play). With nothing copied by hand the whole move completes.
    /// </param>
    public ExtraOptionsLayoutResult MigrateLayout(Func<string, string?>? catalogGame = null, string? backupDirectory = null, bool receiptsOnly = false)
    {
        using var gate = Lock();
        return MigrateLayoutLocked(catalogGame, backupDirectory, receiptsOnly);
    }

    private ExtraOptionsLayoutResult MigrateLayoutLocked(Func<string, string?>? catalogGame, string? backupDirectory, bool receiptsOnly)
    {
        // Read raw: ReadReceipt refuses a receipt of the old layout, which is exactly what this rewrites.
        var receipts = new List<(string Path, ManagedModReceipt Receipt, string Game)>();
        var managed = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        var skipped = new List<string>();
        if (Directory.Exists(BackupRoot))
            foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json").ToArray())
            {
                RejectLinks(path);
                // An unreadable receipt is left as it is (Mods shows it and Resolve archives it); it never stops the move of the others.
                ManagedModReceipt? receipt;
                try { receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)); }
                catch (Exception error) when (error is JsonException or IOException or UnauthorizedAccessException) { receipt = null; }
                if (receipt is null) { skipped.Add(Path.GetFileName(path)); continue; }
                if (receipt.Files is null || !string.Equals(_azimuth, receipt.AzimuthRoot, StringComparison.OrdinalIgnoreCase)) continue;
                var legacy = receipt.Files.Where(f => f.Root == "azimuth").Select(f => ExtraOptionsLayout.LegacyFolder(f.Path)).OfType<string>()
                    .Distinct(StringComparer.OrdinalIgnoreCase).ToList();
                if (legacy.Count == 0) continue;
                string game;
                try { game = SnapshotFor(receipt).ModsGame; }
                catch (ModException) { game = ModManifest.DefaultGame; } // without its manifest, the old layout's own game
                foreach (var folder in legacy) managed.TryAdd(folder, game);
                receipts.Add((path, receipt, game));
            }
        string GameOf(string folder) => managed.TryGetValue(folder, out var game) ? game
            : catalogGame?.Invoke(folder) is "fm5" ? "fm5" : ModManifest.DefaultGame;

        ExtraOptionsLayoutResult result; ExceptionDispatchInfo? failure = null;
        Func<string, bool>? include = receiptsOnly ? folder => folder.StartsWith('.') || managed.ContainsKey(folder) : null;
        try { result = ExtraOptionsLayout.Migrate(_azimuth, GameOf, backupDirectory, _gameRunning, include); }
        catch (ExtraOptionsLayoutException error) { failure = ExceptionDispatchInfo.Capture(error); result = ExtraOptionsLayoutResult.Nothing; }

        // A receipt follows its folder only once the folder has left mods\ (a stop before its move keeps both where they were).
        var notes = new List<string>(result.Notes);
        notes.AddRange(skipped.Select(name => "mods layout: " + name + " is unreadable, left for Mods' resolve"));
        foreach (var (path, receipt, game) in receipts)
        {
            if (FollowsItsFolder(receipt, game, _azimuth) is not { } moved) continue;
            foreach (var file in moved.Files) _ = Target(file.Root, file.Path);
            Atomic(path, JsonSerializer.SerializeToUtf8Bytes(moved, Json));
            notes.Add("mods layout: " + receipt.Id + "'s receipt names mods\\" + ExtraOptionsLayout.GameFolder(game));
        }
        failure?.Throw();
        return result with { Notes = notes };
    }

    /// <summary>
    /// OD-233: a receipt of the old flat layout rewritten to name its game's folder, once none of its folders is still at the root of
    /// <paramref name="azimuth"/>'s <c>mods\</c> (until then its files are where it says); null when it names nothing of the old layout or
    /// a folder has not moved. Pure: the caller writes it.
    /// </summary>
    internal static ManagedModReceipt? FollowsItsFolder(ManagedModReceipt receipt, string game, string azimuth)
    {
        var folders = receipt.Files.Where(f => f.Root == "azimuth").Select(f => ExtraOptionsLayout.LegacyFolder(f.Path)).OfType<string>().ToList();
        if (folders.Count == 0 || folders.Any(f => Directory.Exists(Path.Combine(ExtraOptionsMods.ModsRoot(azimuth), f)))) return null;
        string Moved(string fileRoot, string relative) => fileRoot == "azimuth" ? ExtraOptionsLayout.CanonicalDestination(relative, game) : relative;
        return receipt with
        {
            Files = receipt.Files.Select(f => f with { Path = Moved(f.Root, f.Path) }).ToList(),
            CreatedDirectories = receipt.CreatedDirectories?.Select(d => d with { Path = Moved(d.Root, d.Path) }).Distinct().ToList(),
        };
    }

    /// <summary>The game of an installed manifest snapshot kept in <paramref name="bucketFolder"/> (FH2 without one: the old layout's game).</summary>
    internal static string SnapshotGame(string bucketFolder, ManagedModReceipt receipt)
    {
        foreach (var name in new[] { "manifests/" + receipt.ManifestHash + ".json", receipt.Id + ".manifest.json" })
        {
            var path = Path.Combine(bucketFolder, name.Replace('/', Path.DirectorySeparatorChar));
            try
            {
                if (File.Exists(path) && File.ReadAllBytes(path) is var bytes && ModManifest.Hash(bytes) == receipt.ManifestHash)
                    return ModManifest.Installed(bytes).ModsGame;
            }
            catch (Exception error) when (error is ModException or JsonException or IOException or UnauthorizedAccessException or KeyNotFoundException
                or InvalidOperationException) { }
        }
        return ModManifest.DefaultGame;
    }
}
