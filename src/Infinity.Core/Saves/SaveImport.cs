// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Process;

namespace Infinity.Core.Saves;

/// <summary>Why a picked folder is not a save of the game: a string key (en + pt-BR, <c>{detail}</c> and <c>{game}</c>) and its detail.</summary>
public sealed record SaveFileRefusal(string Key, string Detail = "");

/// <summary>
/// A checked save copied off a PC: the folder holding its <c>revisions</c>, the newest sealed career revision there, and the sibling
/// containers to copy with it (those its career rows name, plus optional ones such as replays). <see cref="MissingSiblings"/>: names the
/// career points to that did not come with it (liveries and thumbnails may be missing; warned, never blocking).
/// </summary>
public sealed record SaveFileSource(string ProfileDir, LocalRevision Revision, IReadOnlyList<string> Siblings, IReadOnlyList<string> MissingSiblings)
{
    /// <summary>Sibling containers that came with the save but no career row names (not copied; logged).</summary>
    public IReadOnlyList<string> Unreferenced { get; init; } = [];
}

/// <summary>The check of a picked folder: a source, or the refusal.</summary>
public sealed record SaveFileCheck(SaveFileSource? Source, SaveFileRefusal? Refusal)
{
    public bool Ok => Source is not null;
}

/// <summary>What <see cref="SaveImport.Import"/> did: the career's new revision, and which sibling containers were copied or kept.</summary>
public sealed record SaveImportResult(CloneResult Career, IReadOnlyList<string> CopiedSiblings, IReadOnlyList<string> SkippedSiblings);

/// <summary>
/// NOOB_FROM_FILE (owner 2026-11-08, Alex via SAURON): "Launcher must accept starting a NOOB profile from a local save file (it syncs with the
/// cloud and starts the game)". A folder picked on this PC (a <c>revisions</c> folder, the folder that holds it, or a copied <c>User_</c>
/// folder) is checked against the game's <see cref="SaveLayout"/>; then only its CONTENTS go into THIS profile's own
/// <c>User_&lt;xuid&gt;</c> folder (never another XUID's), through <see cref="AzimuthSaves.Clone"/>'s staged, sha256-verified copy. Sibling
/// containers are copied beside it only when absent (never overwritten). The caller checks the game is closed and archives the target
/// folder to History first (SAVE_MANAGER_UX §7.1).
/// </summary>
public static class SaveImport
{
    /// <summary>Checks <paramref name="picked"/> as a save of the game whose layout is <paramref name="layout"/>. Reads only.</summary>
    public static SaveFileCheck Inspect(string picked, SaveLayout layout)
    {
        ArgumentNullException.ThrowIfNull(layout);
        static SaveFileCheck No(string key, string detail = "") => new(null, new SaveFileRefusal(key, detail));
        string full;
        try { full = Path.TrimEndingDirectorySeparator(Path.GetFullPath(picked ?? string.Empty)); }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException) { return No("save_file.not_found"); }
        if (picked is null || !Directory.Exists(full)) return No("save_file.not_found");
        try
        {
            string profileDir;
            if (string.Equals(Path.GetFileName(full), layout.RevisionsDir, StringComparison.OrdinalIgnoreCase) && Path.GetDirectoryName(full) is { } parent)
                profileDir = parent;
            else if (Directory.Exists(Path.Combine(full, layout.RevisionsDir)))
                profileDir = full;
            else
                return No("save_file.no_revisions");

            var revisionsDir = Path.Combine(profileDir, layout.RevisionsDir);
            foreach (var entry in Directory.EnumerateFileSystemEntries(revisionsDir))
            {
                var name = Path.GetFileName(entry);
                if (!Directory.Exists(entry) || name.Length != 16 || !name.All(Uri.IsHexDigit)) return No("save_file.unexpected", name);
            }
            var folders = AzimuthSaves.RevisionFolders(profileDir);
            if (folders.Count == 0) return No("save_file.no_revisions");
            if (AzimuthSaves.NewestSealed(profileDir) is not { } newest)
            {
                var (number, path) = folders[^1];
                var problems = AzimuthSaves.ReadRevision(number, path) is { } torn ? AzimuthSaves.Verify(torn) : ["no commit file"];
                return No("save_file.not_sealed", string.Join("; ", problems.Take(3)));
            }
            var names = newest.Entries.Select(e => e.Name).ToList();
            var foreign = names.Where(n => !layout.AllowedFiles.Contains(n, StringComparer.Ordinal)).ToList();
            if (foreign.Count > 0) return No("save_file.wrong_game", string.Join(", ", foreign));
            var missing = layout.RequiredFiles.Where(n => !names.Contains(n, StringComparer.Ordinal)).ToList();
            if (missing.Count > 0) return No("save_file.missing", string.Join(", ", missing));

            // fm5-save-format.md §1: the restore set is the User_ revision plus the containers its career rows name
            IReadOnlySet<string> referenced;
            try { referenced = ReferencedSiblings(newest, layout); }
            catch (SaveException bad) { return No("save_file.unreadable", bad.Message); }
            bool Wanted(string name) => referenced.Contains(name) || layout.OptionalSiblingPrefixes.Any(p => name.StartsWith(p, StringComparison.Ordinal));
            var siblings = new List<string>();
            var unreferenced = new List<string>();
            var staged = Path.Combine(profileDir, layout.StagedSiblingsDir);
            var candidates = new List<string>();
            if (Directory.Exists(staged))
            {
                // a save staged off a PC: everything in its containers folder must be one of the game's sibling containers
                foreach (var entry in Directory.EnumerateFileSystemEntries(staged).Order(StringComparer.Ordinal))
                {
                    var name = Path.GetFileName(entry);
                    if (!Directory.Exists(entry) || !layout.IsSibling(name)) return No("save_file.unexpected_container", name);
                    if (Wanted(name)) candidates.Add(entry); else unreferenced.Add(name);
                }
            }
            else if (referenced.Count > 0 && Path.GetDirectoryName(profileDir) is { } player
                && string.Equals(Path.GetFileName(player), layout.PlayerDir, StringComparison.OrdinalIgnoreCase)
                && Path.GetFileName(profileDir).StartsWith(layout.UserPrefix, StringComparison.OrdinalIgnoreCase))
            {
                // a User_ folder inside a copied player-1: the containers it names are next to it (the others are other careers')
                candidates.AddRange(referenced.Order(StringComparer.Ordinal).Select(n => Path.Combine(player, n)).Where(Directory.Exists));
            }
            foreach (var entry in candidates)
            {
                var name = Path.GetFileName(entry);
                var revisions = AzimuthSaves.RevisionFolders(entry);
                if (revisions.Count == 0 || revisions.Any(r => AzimuthSaves.ReadRevision(r.Number, r.Path) is not { } rev || AzimuthSaves.Verify(rev).Count > 0))
                    return No("save_file.container_not_sealed", name);
                siblings.Add(entry);
            }
            var found = siblings.Select(Path.GetFileName).ToHashSet(StringComparer.Ordinal);
            var absent = referenced.Where(n => !found.Contains(n)).Order(StringComparer.Ordinal).ToList();
            return new(new SaveFileSource(profileDir, newest, siblings, absent) { Unreferenced = unreferenced }, null);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return No("save_file.unreadable", e.Message);
        }
    }

    /// <summary>
    /// The sibling container names <paramref name="revision"/>'s career rows point to (<see cref="SaveLayout.References"/>; empty when the
    /// game names none). The data file is read from a private temporary copy past its header, read-only; a file that is not the database
    /// the layout says throws <see cref="SaveException"/> (never guessed around). Values that are not a sibling name are not references.
    /// </summary>
    public static IReadOnlySet<string> ReferencedSiblings(LocalRevision revision, SaveLayout layout)
    {
        ArgumentNullException.ThrowIfNull(revision);
        ArgumentNullException.ThrowIfNull(layout);
        var names = new HashSet<string>(StringComparer.Ordinal);
        if (layout.References is not { } refs) return names;
        var temp = Path.Combine(Path.GetTempPath(), "fi-save-refs-" + Guid.NewGuid().ToString("N") + ".db");
        try
        {
            byte[] bytes;
            try { bytes = File.ReadAllBytes(Path.Combine(revision.Path, "data", refs.File)); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { throw new SaveException(refs.File + ": " + e.Message); }
            if (bytes.Length <= refs.HeaderBytes) throw new SaveException(refs.File + " is too short");
            File.WriteAllBytes(temp, bytes[refs.HeaderBytes..]);
            using var db = new Microsoft.Data.Sqlite.SqliteConnection(new Microsoft.Data.Sqlite.SqliteConnectionStringBuilder
            { DataSource = temp, Mode = Microsoft.Data.Sqlite.SqliteOpenMode.ReadOnly, Pooling = false }.ToString());
            db.Open();
            foreach (var column in refs.Columns)
            {
                using var query = db.CreateCommand();
                query.CommandText = $"SELECT \"{column.Replace("\"", "\"\"", StringComparison.Ordinal)}\" FROM \"{refs.Table.Replace("\"", "\"\"", StringComparison.Ordinal)}\"";
                using var reader = query.ExecuteReader();
                while (reader.Read())
                {
                    if (reader.IsDBNull(0) || reader.GetValue(0) is not string value) continue;
                    foreach (var prefix in refs.StripPrefixes)
                        if (value.StartsWith(prefix, StringComparison.Ordinal)) { value = value[prefix.Length..]; break; }
                    if (layout.IsSibling(value)) names.Add(value);
                }
            }
            return names;
        }
        catch (Microsoft.Data.Sqlite.SqliteException e) { throw new SaveException(refs.File + " is not a readable database: " + e.Message); }
        finally
        {
            try { if (File.Exists(temp)) File.Delete(temp); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        }
    }

    /// <summary>
    /// The referenced containers that neither came with the save nor are already in this PC's <c>player-1</c>: the career's liveries or
    /// thumbnails may be missing (warned, never blocking).
    /// </summary>
    public static IReadOnlyList<string> MissingAfterImport(string savesRoot, SaveLayout layout, SaveFileSource source)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(layout);
        ArgumentNullException.ThrowIfNull(source);
        return [.. source.MissingSiblings.Where(n => !Directory.Exists(Path.Combine(savesRoot, layout.PlayerDir, n)))];
    }

    /// <summary>
    /// Why nothing may be written now (a string key), or null: never while the game runs, only with the Azimuth folder known, and only into
    /// a Noob career (a Hardcore career starts from the beginning or through the §4.10 check, OD-60).
    /// </summary>
    public static string? GateKey(bool gameRunning, bool installKnown, string? mode) =>
        gameRunning ? "saves.game_running" : !installKnown ? "play.setup_needed" : mode != "noob" ? "save_file.noob_only" : null;

    /// <summary>
    /// What happens with the cloud: a game with cloud saves uploads the copy at the next Play (a normal upload, SAVES_API §4.3); a game
    /// without them (FM5 until FM5SAVE1) keeps it on this PC and says cloud sync comes later. Never a failure.
    /// </summary>
    public static string CloudNoteKey(GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(game);
        return game.FullFeatures ? "save_file.cloud_next_play" : "save_file.cloud_later";
    }

    /// <summary>
    /// Copies <paramref name="source"/> into <paramref name="targetXuid"/>'s own folder of the game (created when missing): first each
    /// sibling container that is not there yet (a name already present is kept, never overwritten), then the career revision as the
    /// folder's newest (<see cref="AzimuthSaves.CloneRevision"/>: the target's revisions archived, a staged copy verified by sha256, then
    /// renamed). A failure removes only the sibling containers this call created and throws <see cref="SaveException"/>.
    /// </summary>
    public static SaveImportResult Import(string savesRoot, SaveLayout layout, SaveFileSource source, ulong targetXuid, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(layout);
        ArgumentNullException.ThrowIfNull(source);
        var targetFolder = layout.UserFolder(targetXuid);
        var targetDir = Path.TrimEndingDirectorySeparator(Path.GetFullPath(layout.ProfileDir(savesRoot, targetXuid)));
        if (string.Equals(Path.TrimEndingDirectorySeparator(Path.GetFullPath(source.ProfileDir)), targetDir, StringComparison.OrdinalIgnoreCase))
            throw new SaveException("the picked save is this profile's own folder");
        var stamp = nowUtc.ToString("yyyyMMdd'T'HHmmss'Z'", CultureInfo.InvariantCulture);
        var copied = new List<string>();
        var skipped = new List<string>();
        try
        {
            foreach (var sibling in source.Siblings)
            {
                var name = Path.GetFileName(sibling);
                var destination = Path.Combine(savesRoot, layout.PlayerDir, name);
                if (Directory.Exists(destination) || File.Exists(destination)) { skipped.Add(name); continue; }
                CopyContainer(savesRoot, sibling, destination, stamp);
                copied.Add(name);
            }
            var clone = AzimuthSaves.CloneRevision(savesRoot, source.Revision, source.ProfileDir, targetFolder, "imported", targetFolder, nowUtc);
            return new SaveImportResult(clone, copied, skipped);
        }
        catch (Exception e) when (e is not SaveException { Landed: true })
        {
            foreach (var name in copied)
            {
                try { Directory.Delete(Path.Combine(savesRoot, layout.PlayerDir, name), recursive: true); }
                catch (Exception cleanup) when (cleanup is IOException or UnauthorizedAccessException) { }
            }
            if (e is SaveException) throw;
            throw new SaveException(e.Message);
        }
    }

    /// <summary>A sibling container (every revision, each verified against its commit) staged on the saves volume, then renamed into place.</summary>
    private static void CopyContainer(string savesRoot, string from, string to, string stamp)
    {
        var staging = Path.Combine(savesRoot, AzimuthSaves.IncomingDir, $"{Path.GetFileName(from)}-{stamp}");
        if (Directory.Exists(staging)) Directory.Delete(staging, recursive: true);
        try
        {
            foreach (var (number, path) in AzimuthSaves.RevisionFolders(from))
            {
                var revision = AzimuthSaves.ReadRevision(number, path) ?? throw new SaveException(Path.GetFileName(from) + ": a revision has no commit file");
                if (AzimuthSaves.Verify(revision) is { Count: > 0 } changed) throw new SaveException(Path.GetFileName(from) + ": " + string.Join("; ", changed));
                var stagedRevision = Path.Combine(staging, "revisions", Path.GetFileName(path));
                Directory.CreateDirectory(Path.Combine(stagedRevision, "data"));
                AzimuthSaves.CopyDurably(Path.Combine(path, "commit"), Path.Combine(stagedRevision, "commit"));
                foreach (var entry in revision.Entries)
                    AzimuthSaves.CopyDurably(Path.Combine(path, "data", entry.Name), Path.Combine(stagedRevision, "data", entry.Name));
                var staged = AzimuthSaves.ReadRevision(number, stagedRevision) ?? throw new SaveException("the staged commit file does not parse");
                if (AzimuthSaves.Verify(staged) is { Count: > 0 } torn || !staged.Entries.SequenceEqual(revision.Entries))
                    throw new SaveException(Path.GetFileName(from) + ": the staged copy does not verify");
            }
            Directory.CreateDirectory(Path.GetDirectoryName(to)!);
            Directory.Move(staging, to);
        }
        catch
        {
            try { if (Directory.Exists(staging)) Directory.Delete(staging, recursive: true); }
            catch (IOException) { }
            throw;
        }
        AzimuthSaves.TryDeleteEmpty(Path.Combine(savesRoot, AzimuthSaves.IncomingDir));
    }

    /// <summary>Every string key the flow uses (each in en and pt-BR).</summary>
    public static IReadOnlyList<string> TextKeys { get; } =
    [
        "save_file.not_found", "save_file.no_revisions", "save_file.unexpected", "save_file.not_sealed", "save_file.wrong_game", "save_file.missing",
        "save_file.unexpected_container", "save_file.container_not_sealed", "save_file.unreadable", "save_file.noob_only", "save_file.cloud_next_play",
        "save_file.cloud_later", "save_file.refused", "save_file.failed", "save_file.done", "save_file.siblings_missing", "save_file.pick_title",
        "save_file.pick_this", "save_file.option", "save_file.hint", "save_file.hint_hardcore", "save_file.menu", "save_file.confirm_title",
        "save_file.confirm_body", "save_file.confirm", "save_file.cancel", "save_file.reason", "save_file.siblings", "busy.checking_save_file",
        "busy.copying_save_file",
    ];
}
