// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text.Json;
using System.Text.Json.Serialization;
using Infinity.Core.Process;

namespace Infinity.Core.Saves;

/// <summary>One archived file of a history entry: its path below the entry's <c>saves</c> folder (forward slashes), size and SHA-256.</summary>
public sealed record HistoryFile(string Path, long Bytes, string Sha256);

/// <summary>The profile an entry is about (synthetic in tests; never written to a log).</summary>
public sealed record HistoryProfile(string Xuid, string Gamertag, string Mode);

/// <summary>The player-facing reason, in both languages (SAVE_MANAGER_UX.md §7: "Before copying NOOB → Azimuth").</summary>
public sealed record HistoryReason(string En, [property: JsonPropertyName("ptBR")] string PtBr)
{
    public string For(string locale) => locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? PtBr : En;
}

/// <summary><c>entry.json</c> (SAVE_MANAGER_UX.md §7, schema 1).</summary>
public sealed record HistoryEntry(
    int SchemaVersion,
    string At,
    string Action,
    string Game,
    string TitleId,
    string? From,
    string? To,
    HistoryProfile? Profile,
    HistoryReason Reason,
    string LauncherVersion,
    IReadOnlyList<HistoryFile> Files,
    bool? Empty = null);

/// <summary>An entry as listed: its folder and what <c>entry.json</c> says.</summary>
public sealed record HistoryItem(string Folder, HistoryEntry Entry)
{
    public string Name => System.IO.Path.GetFileName(Folder);

    public bool IsOriginal => Entry.Action == LocalHistory.Original;

    public long Bytes => Entry.Files.Sum(f => f.Bytes);
}

/// <summary>A history write that did not verify: the action it guarded must not run (nothing was written).</summary>
public sealed class HistoryException(string message) : Exception(message);

/// <summary>
/// SAVE_MANAGER_UX.md §7 (owner 2026-11-07): the launcher's own, per game and title, history of every save it replaces.
/// <c>&lt;root&gt;/history/&lt;game&gt;/&lt;titleId&gt;/</c>, never inside Azimuth's folder; one folder per action
/// (<c>yyyyMMddTHHmmssZ-&lt;action&gt;[-&lt;slug&gt;]</c>) with <c>entry.json</c> and <c>saves/</c>, never overwritten, never pruned.
/// <list type="bullet">
/// <item><see cref="Archive"/> copies first and verifies every file's hash against the source; a mismatch removes the half-written entry and
/// throws, so the caller writes nothing (§7.1 "A failed archive stops the action").</item>
/// <item><see cref="EnsureOriginal"/>: the title's saves as first found, once (§7.2); an empty folder is recorded as such.</item>
/// <item><see cref="RestoreOriginal"/>: archives what is there as <c>restore-original</c>, then copies <c>original/</c> back, verified.</item>
/// </list>
/// Only <c>player-1</c> (the game's own saves) is copied: the launcher's <c>fi-launcher-*</c> folders stay where they are.
/// </summary>
public sealed class LocalHistory
{
    public const string Original = "original";
    public const string EntryFile = "entry.json";
    public const string SavesDir = "saves";

    /// <summary>The <c>action</c> values of §7.</summary>
    public static readonly IReadOnlySet<string> Actions = new HashSet<string>(StringComparer.Ordinal)
    {
        "copy", "restore", "resolve", "delete-career", "delete-profile", "start-career", "fresh-start", "restore-original", "import", Original,
    };

    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web)
    {
        WriteIndented = true,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    private readonly string _launcherVersion;

    public LocalHistory(string infinityRoot, GameDefinition game, string launcherVersion)
    {
        ArgumentNullException.ThrowIfNull(infinityRoot);
        ArgumentNullException.ThrowIfNull(game);
        Game = game;
        _launcherVersion = launcherVersion ?? "0.0.0";
        Root = System.IO.Path.Combine(infinityRoot, "history", game.Id, game.TitleKey);
    }

    public GameDefinition Game { get; }

    /// <summary><c>&lt;root&gt;/history/&lt;game&gt;/&lt;titleId&gt;</c> (lower-case title id: FH2 <c>1a96545b</c>, FM5 <c>7b35d917</c>).</summary>
    public string Root { get; }

    public string OriginalFolder => System.IO.Path.Combine(Root, Original);

    public bool HasOriginal => File.Exists(System.IO.Path.Combine(OriginalFolder, EntryFile));

    /// <summary>
    /// Copies <paramref name="source"/> (a folder: a whole <c>player-1</c> or one <c>User_*</c>; a missing folder archives nothing and
    /// records <c>empty</c>) into a new entry and verifies it. Returns the entry. Throws <see cref="HistoryException"/> (nothing left
    /// behind) when a copy does not match its source.
    /// </summary>
    public HistoryItem Archive(string action, string? source, HistoryReason reason, DateTime nowUtc,
        string? from = null, string? to = null, HistoryProfile? profile = null, string? slug = null)
    {
        ArgumentNullException.ThrowIfNull(action);
        ArgumentNullException.ThrowIfNull(reason);
        if (!Actions.Contains(action) || action == Original) throw new ArgumentException("not a history action: " + action, nameof(action));
        var name = Stamp(nowUtc) + "-" + action + (string.IsNullOrWhiteSpace(slug) ? string.Empty : "-" + Slug(slug));
        var folder = System.IO.Path.Combine(Root, name);
        for (var i = 2; Directory.Exists(folder); i++) folder = System.IO.Path.Combine(Root, name + "-" + i.ToString(CultureInfo.InvariantCulture));
        return Write(folder, action, source, reason, nowUtc, from, to, profile);
    }

    /// <summary>
    /// §7.2: the first time the launcher touches the title's saves, they are copied as found into <c>original/</c>, once. Returns the
    /// entry when written now, null when it existed already (it is never overwritten).
    /// </summary>
    public HistoryItem? EnsureOriginal(string savesRoot, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        // review 3.1.1: callers run concurrently (the cards' background copy, an action's archive-first): one writer per title at a time,
        // so a second caller never deletes the first one's half-written original/ as a crash leftover; it finds it complete instead
        lock (TitleLock(Root))
        {
            if (HasOriginal) return null;
            if (Directory.Exists(OriginalFolder)) Directory.Delete(OriginalFolder, recursive: true); // a half-written one from a crash
            return Write(OriginalFolder, Original, PlayerDir(savesRoot), new HistoryReason("Azimuth's save as Infinity first found it", "O save do Azimuth como o Infinity o encontrou"),
                nowUtc, null, null, null);
        }
    }

    /// <summary>One lock per title's history folder (every <see cref="LocalHistory"/> of it in this process shares it).</summary>
    private static readonly System.Collections.Concurrent.ConcurrentDictionary<string, object> TitleLocks = new(StringComparer.OrdinalIgnoreCase);

    private static object TitleLock(string root) => TitleLocks.GetOrAdd(System.IO.Path.GetFullPath(root), _ => new object());

    /// <summary>Every entry, newest first, <c>original</c> last; a folder whose <c>entry.json</c> does not read is skipped.</summary>
    public IReadOnlyList<HistoryItem> List()
    {
        if (!Directory.Exists(Root)) return [];
        var items = new List<HistoryItem>();
        foreach (var dir in Directory.EnumerateDirectories(Root))
        {
            if (Read(dir) is { } item) items.Add(item);
        }
        return [.. items.OrderBy(i => i.IsOriginal).ThenByDescending(i => i.Entry.At, StringComparer.Ordinal).ThenByDescending(i => i.Name, StringComparer.Ordinal)];
    }

    /// <summary>The bytes the history of this title holds (§7.1 "History shows its size per game").</summary>
    public long SizeBytes() => List().Sum(i => i.Bytes);

    /// <summary>The files of <paramref name="item"/> that are missing or no longer match their hash (empty: intact).</summary>
    public static IReadOnlyList<string> Verify(HistoryItem item)
    {
        ArgumentNullException.ThrowIfNull(item);
        var saves = System.IO.Path.Combine(item.Folder, SavesDir);
        var bad = new List<string>();
        foreach (var file in item.Entry.Files)
        {
            var path = System.IO.Path.Combine(saves, file.Path.Replace('/', System.IO.Path.DirectorySeparatorChar));
            if (!File.Exists(path) || new FileInfo(path).Length != file.Bytes || !string.Equals(Sha256Of(path), file.Sha256, StringComparison.Ordinal)) bad.Add(file.Path);
        }
        return bad;
    }

    /// <summary>
    /// §7.2 "Restore Azimuth's original save": archives the current <c>player-1</c> as a <c>restore-original</c> entry (verified), then
    /// replaces it with <c>original/</c>'s copy and verifies the result by hash. The caller checks the game is not running.
    /// </summary>
    public HistoryItem RestoreOriginal(string savesRoot, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        var original = Read(OriginalFolder) ?? throw new HistoryException("no original save was recorded for this title");
        if (Verify(original) is { Count: > 0 } broken) throw new HistoryException("the original save does not verify: " + string.Join(", ", broken));
        var archived = Archive("restore-original", PlayerDir(savesRoot),
            new HistoryReason("Before restoring Azimuth's original save", "Antes de restaurar o save original do Azimuth"), nowUtc);
        var player = PlayerDir(savesRoot);
        if (Directory.Exists(player)) Directory.Delete(player, recursive: true); // verified in the entry above
        var from = System.IO.Path.Combine(original.Folder, SavesDir);
        if (Directory.Exists(from)) CopyTree(from, player);
        else Directory.CreateDirectory(player);
        foreach (var file in original.Entry.Files)
        {
            var path = System.IO.Path.Combine(player, file.Path.Replace('/', System.IO.Path.DirectorySeparatorChar));
            if (!File.Exists(path) || !string.Equals(Sha256Of(path), file.Sha256, StringComparison.Ordinal))
                throw new HistoryException("the restored file does not verify: " + file.Path);
        }
        return archived;
    }

    /// <summary>
    /// The newest sealed save an entry holds (an archived <c>User_*</c> folder, or a whole <c>player-1</c>), as a source for
    /// <see cref="AzimuthSaves.Clone"/>; its <c>Folder</c> names the entry, so it can be copied back onto the folder it came from.
    /// </summary>
    public static SaveProfile? NewestSave(HistoryItem item)
    {
        ArgumentNullException.ThrowIfNull(item);
        var saves = System.IO.Path.Combine(item.Folder, SavesDir);
        if (!Directory.Exists(saves)) return null;
        var candidates = Directory.EnumerateDirectories(saves, "revisions", SearchOption.AllDirectories)
            .Select(r => System.IO.Path.GetDirectoryName(r)!)
            .Select(AzimuthSaves.NewestSealed)
            .OfType<LocalRevision>()
            .Select(r => (Revision: r, Played: File.GetLastWriteTimeUtc(System.IO.Path.Combine(r.Path, "commit"))))
            .OrderByDescending(r => r.Played)
            .ToList();
        return candidates is [var newest, ..]
            ? new SaveProfile("history-" + item.Name, null, newest.Revision, newest.Played, AzimuthSaves.Credits(newest.Revision))
            : null;
    }

    /// <summary><c>&lt;saves&gt;/player-1</c>: what Azimuth reads, and what <see cref="EnsureOriginal"/> keeps.</summary>
    public static string PlayerDir(string savesRoot) => System.IO.Path.Combine(savesRoot, "player-1");

    private HistoryItem Write(string folder, string action, string? source, HistoryReason reason, DateTime nowUtc, string? from, string? to, HistoryProfile? profile)
    {
        var saves = System.IO.Path.Combine(folder, SavesDir);
        try
        {
            Directory.CreateDirectory(saves);
            var files = new List<HistoryFile>();
            var empty = source is null || !Directory.Exists(source);
            if (!empty)
            {
                CopyTree(source!, saves);
                foreach (var path in Directory.EnumerateFiles(source!, "*", SearchOption.AllDirectories).OrderBy(p => p, StringComparer.Ordinal))
                {
                    var relative = System.IO.Path.GetRelativePath(source!, path);
                    var copy = System.IO.Path.Combine(saves, relative);
                    var expected = Sha256Of(path);
                    if (!File.Exists(copy) || !string.Equals(Sha256Of(copy), expected, StringComparison.Ordinal))
                        throw new HistoryException("the copy of " + relative + " does not match its source");
                    files.Add(new HistoryFile(relative.Replace(System.IO.Path.DirectorySeparatorChar, '/'), new FileInfo(path).Length, expected));
                }
                empty = files.Count == 0;
            }
            var entry = new HistoryEntry(1, nowUtc.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", CultureInfo.InvariantCulture), action, Game.Id, Game.TitleKey,
                from, to, profile, reason, _launcherVersion, files, empty ? true : null);
            File.WriteAllText(System.IO.Path.Combine(folder, EntryFile), JsonSerializer.Serialize(entry, Json));
            return new HistoryItem(folder, entry);
        }
        catch (Exception e) when (e is HistoryException or IOException or UnauthorizedAccessException)
        {
            try { if (Directory.Exists(folder)) Directory.Delete(folder, recursive: true); }
            catch (Exception cleanup) when (cleanup is IOException or UnauthorizedAccessException) { }
            throw e as HistoryException ?? new HistoryException("the history copy failed: " + e.Message);
        }
    }

    private static HistoryItem? Read(string folder)
    {
        var path = System.IO.Path.Combine(folder, EntryFile);
        if (!File.Exists(path)) return null;
        try
        {
            return JsonSerializer.Deserialize<HistoryEntry>(File.ReadAllText(path), Json) is { SchemaVersion: 1, Files: not null, Reason: not null } entry
                ? new HistoryItem(folder, entry) : null;
        }
        catch (Exception e) when (e is JsonException or IOException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    private static void CopyTree(string from, string to)
    {
        Directory.CreateDirectory(to);
        foreach (var file in Directory.EnumerateFiles(from)) File.Copy(file, System.IO.Path.Combine(to, System.IO.Path.GetFileName(file)), overwrite: false);
        foreach (var dir in Directory.EnumerateDirectories(from)) CopyTree(dir, System.IO.Path.Combine(to, System.IO.Path.GetFileName(dir)));
    }

    internal static string Sha256Of(string path)
    {
        using var stream = File.OpenRead(path);
        return Convert.ToHexStringLower(SHA256.HashData(stream));
    }

    private static string Stamp(DateTime nowUtc) => nowUtc.ToUniversalTime().ToString("yyyyMMdd'T'HHmmss'Z'", CultureInfo.InvariantCulture);

    private static string Slug(string text)
    {
        var chars = text.Trim().ToLowerInvariant().Select(c => char.IsAsciiLetterOrDigit(c) ? c : '-').ToArray();
        var slug = new string(chars).Trim('-');
        while (slug.Contains("--", StringComparison.Ordinal)) slug = slug.Replace("--", "-", StringComparison.Ordinal);
        return slug.Length > 40 ? slug[..40] : slug;
    }
}
