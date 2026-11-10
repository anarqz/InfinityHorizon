// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>One folder of a game's mods folder (<c>&lt;Azimuth&gt;\mods\&lt;game folder&gt;</c>, OD-233) as ExtraOptions' loader sees it (OD-126).</summary>
/// <param name="Folder">The folder name, as <c>loadorder.txt</c> lists it.</param>
/// <param name="Id">The manifest id, or the folder name without a manifest (ExtraOptions' identity rule).</param>
/// <param name="Order">Position in load order (later = higher priority); unlisted folders follow, alphabetically.</param>
/// <param name="Listed">The folder has a line in <c>loadorder.txt</c>; an unlisted one is appended, enabled, at the next start.</param>
/// <param name="ManifestError">Why ExtraOptions will refuse the manifest, or null.</param>
public sealed record ExtraOptionsMod(string Folder, string Id, string Name, string? Version, IReadOnlyList<string> Dependencies,
    bool Enabled, int Order, bool Listed, bool HasMedia, bool HasDatabase, bool HasStrings, int Plugins, string? ManifestError);

/// <summary>What the last game run's log (<c>FH2_ExtraOptions.log</c>, <c>FM5_ExtraOptions.log</c>) said about one mod.</summary>
public enum ExtraOptionsModState { Unknown, Loaded, Pending, Failed }

public sealed record ExtraOptionsModStatus(ExtraOptionsModState State, IReadOnlyList<string> Messages);

/// <summary>
/// OD-126: the launcher manages mods in ExtraOptions' own format, so a mod copied by hand and one from the catalog look the
/// same. The rules are ExtraOptions 0.6.0's (<c>shared/addons.c</c>, <c>shared/addon_layout.h</c>, <c>docs/MODS.md</c>): each game has
/// its own folder (<c>&lt;Azimuth&gt;\mods\Forza Horizon 2</c>, <c>…\Forza Motorsport 5</c>, <see cref="ExtraOptionsLayout"/>; <c>mods\Cache</c>
/// is the loader's own); every folder in it but dot folders and links is a mod; its <c>loadorder.txt</c> (UTF-8, one folder per line,
/// later = higher priority) enables a listed folder and disables one written <c># folder</c>; a folder it does not list is appended,
/// enabled, when the game starts. Change the file only while the game is closed: ExtraOptions rewrites it at start. Every method takes the
/// game (the server's id, FH2 when none is given).
/// </summary>
public static class ExtraOptionsMods
{
    public const string ModsFolder = "mods";
    public const string LoadOrderFile = "loadorder.txt";
    public const string LogFile = "FH2_ExtraOptions.log";
    public const string Fm5LogFile = "FM5_ExtraOptions.log";
    private const int MaxLoadOrder = 64 * 1024, MaxManifest = 1024 * 1024, MaxLog = 8 * 1024 * 1024;

    /// <summary><c>&lt;Azimuth&gt;\mods</c>: the game folders and <c>Cache</c> (and, before OD-233's move, the flat layout).</summary>
    public static string ModsRoot(string azimuthRoot) => Path.Combine(azimuthRoot, ModsFolder);

    /// <summary>The game's mods folder, <c>&lt;Azimuth&gt;\mods\&lt;game folder&gt;</c> (OD-233).</summary>
    public static string ModsDirectory(string azimuthRoot, string game = ModManifest.DefaultGame) =>
        Path.Combine(azimuthRoot, ModsFolder, ExtraOptionsLayout.GameFolder(game));

    /// <summary>The Azimuth-relative prefix of a game's mod folder: <c>mods/&lt;game folder&gt;/&lt;folder&gt;/</c>.</summary>
    public static string FolderPrefix(string game, string folder) => ModsFolder + "/" + ExtraOptionsLayout.GameFolder(game) + "/" + folder + "/";

    /// <summary>The log the game's ExtraOptions writes beside <c>azimuth.exe</c> at every start.</summary>
    public static string LogFileFor(string game) => game == "fm5" ? Fm5LogFile : LogFile;

    /// <summary>Every mod folder of the game, in ExtraOptions' load order. No mods folder: none.</summary>
    public static IReadOnlyList<ExtraOptionsMod> Scan(string azimuthRoot, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var mods = ModsDirectory(azimuthRoot, game);
        if (!Directory.Exists(mods)) return [];
        var folders = new DirectoryInfo(mods).EnumerateDirectories()
            .Where(d => !d.Name.StartsWith('.') && !d.Attributes.HasFlag(FileAttributes.ReparsePoint))
            .Select(d => d.Name).Order(StringComparer.OrdinalIgnoreCase).ToList();
        var order = new List<(string Folder, bool Enabled, bool Listed)>();
        foreach (var line in ReadLoadOrder(azimuthRoot, game).Lines)
        {
            if (Entry(line) is not { } entry) continue;
            var folder = folders.FirstOrDefault(f => f.Equals(entry.Folder, StringComparison.OrdinalIgnoreCase));
            if (folder is not null && !order.Any(o => o.Folder.Equals(folder, StringComparison.OrdinalIgnoreCase)))
                order.Add((folder, entry.Enabled, true));
        }
        order.AddRange(folders.Where(f => !order.Any(o => o.Folder.Equals(f, StringComparison.OrdinalIgnoreCase))).Select(f => (f, true, false)));
        return [.. order.Select((o, i) => Describe(Path.Combine(mods, o.Folder), o.Folder, o.Enabled, i, o.Listed))];
    }

    /// <summary>
    /// Enables (<c>folder</c>) or disables (<c># folder</c>) a mod in <c>loadorder.txt</c>, keeping every other line, comment,
    /// the BOM and the line endings; an unlisted folder gets a line at the end. True when the file changed.
    /// </summary>
    public static bool SetEnabled(string azimuthRoot, string folder, bool enabled, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        if (string.IsNullOrWhiteSpace(folder) || folder.IndexOfAny(['/', '\\', '\r', '\n']) >= 0 || folder.StartsWith('.'))
            throw new ArgumentException("not a mod folder name", nameof(folder));
        var file = ReadLoadOrder(azimuthRoot, game);
        var wanted = enabled ? folder : "# " + folder;
        var at = file.Lines.FindIndex(l => Entry(l) is { } e && e.Folder.Equals(folder, StringComparison.OrdinalIgnoreCase));
        if (at >= 0)
        {
            if (Entry(file.Lines[at])!.Value.Enabled == enabled) return false;
            file.Lines[at] = wanted;
        }
        else
        {
            if (file.Lines.Count > 0 && file.Lines[^1].Length == 0) file.Lines.RemoveAt(file.Lines.Count - 1);
            file.Lines.Add(wanted);
        }
        WriteLoadOrder(azimuthRoot, file, game);
        return true;
    }

    /// <summary>
    /// Owner 2026-10-28: removes one folder of <c>mods\</c> and its <c>loadorder.txt</c> line: a manually installed mod the player
    /// deletes, or what an uninstalled catalog mod still left in its own folder. Game closed (the caller's check). A linked folder,
    /// or anything linked inside it, is refused, so only the folder's own files go, through the one safe delete.
    /// </summary>
    public static void DeleteFolder(string azimuthRoot, string folder, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        if (string.IsNullOrWhiteSpace(folder) || folder.IndexOfAny(['/', '\\', '\r', '\n', ':']) >= 0 || folder.StartsWith('.'))
            throw new ArgumentException("not a mod folder name", nameof(folder));
        var directory = new DirectoryInfo(Path.Combine(ModsDirectory(azimuthRoot, game), folder));
        if (directory.Exists)
        {
            var everything = new EnumerationOptions { RecurseSubdirectories = true, AttributesToSkip = 0 };
            if (directory.LinkTarget is not null || directory.EnumerateFileSystemInfos("*", everything).Any(i => i.LinkTarget is not null))
                throw new IOException("a linked mod folder is not deleted");
            foreach (var file in directory.EnumerateFiles("*", everything).ToList()) ManagedModFiles.RemoveFile(file.FullName);
            foreach (var inner in directory.EnumerateDirectories("*", everything).OrderByDescending(d => d.FullName.Length).ToList()) inner.Delete();
            directory.Delete();
        }
        var order = ReadLoadOrder(azimuthRoot, game);
        if (order.Lines.RemoveAll(l => Entry(l) is { } e && e.Folder.Equals(folder, StringComparison.OrdinalIgnoreCase)) > 0) WriteLoadOrder(azimuthRoot, order, game);
    }

    /// <summary>
    /// The last game run's verdict per mod (keyed by id and by folder), from the <c>MODS:</c> and <c>PLUGINS:</c> lines of the game's log
    /// beside <c>azimuth.exe</c> (<see cref="LogFileFor"/>; ExtraOptions recreates it every start). No log: empty.
    /// <para>
    /// The lines are ExtraOptions 0.6.0's: <c>MODS: &lt;id or folder&gt;: &lt;message&gt;</c> (<c>shared/addons.c</c> <c>report</c>) and
    /// <c>PLUGINS: &lt;…&gt;\mods\&lt;game folder&gt;\&lt;folder&gt;\plugins\&lt;dll&gt; | &lt;result&gt;</c> (<c>shared/plugins.c</c> <c>log_plugin</c>); a
    /// path of the old flat layout (<c>…\mods\&lt;folder&gt;\…</c>, an older log) still names its folder.
    /// </para>
    /// </summary>
    public static IReadOnlyDictionary<string, ExtraOptionsModStatus> ReadStatus(string azimuthRoot, IReadOnlyList<ExtraOptionsMod> mods, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var log = Path.Combine(azimuthRoot, LogFileFor(game));
        var found = new Dictionary<string, (ExtraOptionsModState State, List<string> Messages)>(StringComparer.OrdinalIgnoreCase);
        string[] lines;
        try
        {
            if (!File.Exists(log) || new FileInfo(log).Length > MaxLog) return new Dictionary<string, ExtraOptionsModStatus>();
            using var stream = new FileStream(log, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            using var reader = new StreamReader(stream, new UTF8Encoding(false));
            lines = reader.ReadToEnd().Split('\n');
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return new Dictionary<string, ExtraOptionsModStatus>(); }
        foreach (var raw in lines)
        {
            var line = raw.TrimEnd('\r');
            string? key = null, message = null;
            if (line.StartsWith("MODS: ", StringComparison.Ordinal))
            {
                var rest = line[6..];
                var colon = rest.IndexOf(": ", StringComparison.Ordinal);
                if (colon <= 0) continue; // a line about the whole loader
                key = rest[..colon]; message = rest[(colon + 2)..];
            }
            else if (line.StartsWith("PLUGINS: ", StringComparison.Ordinal) && line.IndexOf(" | ", StringComparison.Ordinal) is var bar and > 9)
            {
                // PLUGINS: <...>\mods\<game folder>\<folder>\plugins[\<dll>] | <result> (0.6.0); <...>\mods\<folder>\... before it
                var path = line[9..bar].Replace('/', '\\');
                var gameMark = "\\" + ModsFolder + "\\" + ExtraOptionsLayout.GameFolder(game) + "\\";
                var mark = path.LastIndexOf(gameMark, StringComparison.OrdinalIgnoreCase);
                string tail;
                if (mark >= 0) tail = path[(mark + gameMark.Length)..];
                else
                {
                    mark = path.LastIndexOf("\\" + ModsFolder + "\\", StringComparison.OrdinalIgnoreCase);
                    if (mark < 0) continue;
                    tail = path[(mark + ModsFolder.Length + 2)..];
                }
                var slash = tail.IndexOf('\\');
                key = slash > 0 ? tail[..slash] : tail;
                if (key.Length == 0 || ExtraOptionsLayout.Reserved(key)) continue; // another game's plugin, or the loader's own folder
                message = "plugin " + Path.GetFileName(path) + ": " + line[(bar + 3)..];
            }
            if (key is null || message is null) continue;
            var mod = mods.FirstOrDefault(m => m.Id.Equals(key, StringComparison.OrdinalIgnoreCase) || m.Folder.Equals(key, StringComparison.OrdinalIgnoreCase));
            var name = mod?.Folder ?? key;
            if (!found.TryGetValue(name, out var entry)) entry = (ExtraOptionsModState.Unknown, []);
            entry.Messages.Add(message);
            entry.State = Merge(entry.State, Classify(message));
            found[name] = entry;
        }
        var result = new Dictionary<string, ExtraOptionsModStatus>(StringComparer.OrdinalIgnoreCase);
        foreach (var (folder, entry) in found)
        {
            var status = new ExtraOptionsModStatus(entry.State, entry.Messages);
            result[folder] = status;
            if (mods.FirstOrDefault(m => m.Folder.Equals(folder, StringComparison.OrdinalIgnoreCase)) is { } mod) result.TryAdd(mod.Id, status);
        }
        return result;
    }

    /// <summary>
    /// Before a launch, game closed. First <see cref="ArrangeLoadOrder"/>. Hardcore: every enabled mod not in
    /// <paramref name="allowedInHardcore"/> (folders: the catalog's mods that change nothing for other players) is
    /// disabled and remembered in <paramref name="stateFile"/>; any other profile: the mods a Hardcore launch disabled, and that
    /// are still disabled, are enabled again. Returns one log line per change.
    /// </summary>
    /// <param name="first">The mods kept at the top of the load order (<see cref="ModManifest.LoadFirst"/>), by id or folder.</param>
    public static IReadOnlyList<string> PrepareForLaunch(string azimuthRoot, bool hardcore, IReadOnlySet<string> allowedInHardcore, string stateFile,
        IEnumerable<string>? first = null, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(allowedInHardcore);
        ArgumentNullException.ThrowIfNull(stateFile);
        var notes = new List<string>(ArrangeLoadOrder(azimuthRoot, first, game));
        var held = ReadHeld(stateFile);
        var mods = Scan(azimuthRoot, game);
        if (hardcore)
        {
            foreach (var mod in mods.Where(m => m.Enabled && !allowedInHardcore.Contains(m.Folder)))
            {
                SetEnabled(azimuthRoot, mod.Folder, false, game);
                if (!held.Contains(mod.Folder, StringComparer.OrdinalIgnoreCase)) held.Add(mod.Folder);
                notes.Add("extraoptions mods: " + mod.Folder + " off for Hardcore");
            }
            WriteHeld(stateFile, held);
            return notes;
        }
        foreach (var folder in held)
        {
            if (mods.FirstOrDefault(m => m.Folder.Equals(folder, StringComparison.OrdinalIgnoreCase)) is { Enabled: false } mod)
            {
                SetEnabled(azimuthRoot, mod.Folder, true, game);
                notes.Add("extraoptions mods: " + mod.Folder + " on again after Hardcore");
            }
        }
        // Emptied rather than deleted: mod code removes files only through ManagedModFiles' remove path.
        if (held.Count > 0) WriteHeld(stateFile, [], empty: true);
        return notes;
    }

    /// <summary>
    /// OD-233, FM5 before a launch (game closed, after <see cref="ArrangeLoadOrder"/>): its mods folder is launcher-managed, catalog only.
    /// Every enabled folder not in <paramref name="allowed"/> (Noob: the launcher's installed mods; Hardcore: those of them for both modes)
    /// is turned off (<c># folder</c>) and remembered in <paramref name="heldFile"/>; a folder the launcher turned off goes back on once it
    /// is allowed (it became managed, or the launch is Noob again). Only what the launcher itself turned off is remembered: a folder the
    /// player turned off stays theirs, and a remembered one that is gone or that the player turned on by hand is forgotten. Returns one log
    /// line per change.
    /// </summary>
    public static IReadOnlyList<string> KeepCatalogOnly(string azimuthRoot, IReadOnlySet<string> allowed, string heldFile, string game)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(allowed);
        ArgumentNullException.ThrowIfNull(heldFile);
        var notes = new List<string>();
        var mods = Scan(azimuthRoot, game);
        var held = ReadHeld(heldFile);
        var kept = new List<string>();
        foreach (var folder in held)
        {
            var mod = mods.FirstOrDefault(m => m.Folder.Equals(folder, StringComparison.OrdinalIgnoreCase));
            if (mod is null || mod.Enabled) continue; // gone, or the player turned it on: no longer the launcher's hold
            if (allowed.Contains(mod.Folder))
            {
                SetEnabled(azimuthRoot, mod.Folder, true, game);
                notes.Add("extraoptions mods: " + mod.Folder + " on again (" + game + ")");
            }
            else kept.Add(mod.Folder);
        }
        foreach (var mod in mods.Where(m => m.Enabled && !allowed.Contains(m.Folder)))
        {
            SetEnabled(azimuthRoot, mod.Folder, false, game);
            if (!kept.Contains(mod.Folder, StringComparer.OrdinalIgnoreCase)) kept.Add(mod.Folder);
            notes.Add("extraoptions mods: " + mod.Folder + " off (" + game + " plays the launcher's mods only)");
        }
        if (kept.Count > 0 || held.Count > 0) WriteHeld(heldFile, kept, empty: true);
        return notes;
    }

    /// <summary>
    /// The folder a catalog manifest installs in its game's mods folder (every <c>azimuth</c> file under
    /// <c>mods/&lt;game folder&gt;/&lt;folder&gt;/</c>, OD-233), or null for a mod that is not an ExtraOptions mod folder. Its game is
    /// <see cref="ModManifest.ModsGame"/>.
    /// </summary>
    public static string? FolderOf(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        var folders = manifest.Files.Where(f => f.Root == "azimuth").Select(f => ModsSubfolder(f.Destination, manifest.ModsGame)).Distinct(StringComparer.OrdinalIgnoreCase).ToList();
        return folders.Count == 1 ? folders[0] : null;
    }

    /// <summary>
    /// Before a catalog mod is installed or updated (OD-126, OD-232): an ExtraOptions mod lives only in its game's
    /// <c>mods/&lt;game folder&gt;/&lt;id&gt;/</c> (OD-233), and its own <c>mod.json</c> agrees with the Infinity manifest (id, version,
    /// dependencies), since ExtraOptions loads by the one and the launcher installs by the other. <paramref name="read"/> returns a package
    /// file's verified bytes by destination.
    /// </summary>
    public static void CheckPackage(ModManifest manifest, Func<string, byte[]?> read)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        ArgumentNullException.ThrowIfNull(read);
        var azimuth = manifest.Files.Where(f => f.Root == "azimuth").ToList();
        var folder = FolderOf(manifest);
        if (folder is null || manifest.Files.Any(f => f.Root != "azimuth"))
            throw new ModException("an ExtraOptions mod installs only into its own mods folder", ModErrors.Unsupported);
        if (!folder.Equals(manifest.Id, StringComparison.Ordinal))
            throw new ModException("the ExtraOptions mod folder must be the mod's id", ModErrors.Unsupported);
        var dependencies = manifest.Dependencies.Select(d => d.Id).Where(d => d != ManagedModFiles.BuiltInExtraOptions)
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        var file = azimuth.FirstOrDefault(f => f.Destination.Equals(FolderPrefix(manifest.ModsGame, folder) + "mod.json", StringComparison.OrdinalIgnoreCase))
            ?? throw new ModException("an ExtraOptions mod needs its mod.json", ModErrors.Unsupported);
        var bytes = read(file.Destination) ?? throw new ModException("the mod's mod.json is missing from the package", ModErrors.Unsupported);
        string? id = null, version = null; var declared = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        try
        {
            using var document = JsonDocument.Parse(bytes);
            var root = document.RootElement;
            if (root.ValueKind == JsonValueKind.Object && root.TryGetProperty("format_version", out var format) && format.ValueKind == JsonValueKind.Number && format.GetDouble() == 1)
            {
                if (root.TryGetProperty("id", out var i) && i.ValueKind == JsonValueKind.String) id = i.GetString();
                if (root.TryGetProperty("version", out var v) && v.ValueKind == JsonValueKind.String) version = v.GetString();
                if (root.TryGetProperty("dependencies", out var d) && d.ValueKind == JsonValueKind.Array)
                    foreach (var item in d.EnumerateArray()) declared.Add(item.ValueKind == JsonValueKind.String ? item.GetString()! : "?");
            }
        }
        catch (JsonException) { }
        if (id is null || !ValidId(id)) throw new ModException("the mod's mod.json is not an ExtraOptions manifest", ModErrors.Unsupported);
        if (!id.Equals(manifest.Id, StringComparison.Ordinal))
            throw new ModException("the mod's mod.json names another id than the Infinity manifest", ModErrors.Unsupported);
        if (version is not null && version != manifest.Version)
            throw new ModException("the mod's mod.json has another version than the Infinity manifest", ModErrors.Unsupported);
        if (!declared.SetEquals(dependencies))
            throw new ModException("the mod's mod.json and the Infinity manifest declare different dependencies", ModErrors.Unsupported);
    }

    /// <summary>
    /// Before a launch, game closed (OD-126): every folder gets its line (an unlisted one is appended, enabled, as ExtraOptions
    /// would), and a mod listed before one of its dependencies moves to just after the last of them, since ExtraOptions refuses
    /// a mod whose dependency loads later. Other lines, comments and the remaining order stay. Returns one log line per move.
    /// <para>
    /// OD-232: the mods named in <paramref name="first"/> (by id or folder; <see cref="ModManifest.LoadFirst"/>, the ZephyrSkies Commons)
    /// go first: right after the file's leading comment and blank lines, ordered by id (ordinal, so <c>fm5-zs-fm5-common</c> before
    /// <c>zs-fm6-zs-fm6-common</c>), each keeping its enabled or <c># disabled</c> state. Every other line keeps its relative order; then
    /// dependencies still go before dependents. A second run changes nothing.
    /// </para>
    /// </summary>
    public static IReadOnlyList<string> ArrangeLoadOrder(string azimuthRoot, IEnumerable<string>? first = null, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var mods = Scan(azimuthRoot, game);
        if (mods.Count == 0) return [];
        var file = ReadLoadOrder(azimuthRoot, game);
        var notes = new List<string>();
        var changed = false;
        if (file.Lines.Count > 0 && file.Lines[^1].Length == 0) file.Lines.RemoveAt(file.Lines.Count - 1);
        foreach (var mod in mods.Where(m => !m.Listed)) { file.Lines.Add(mod.Folder); changed = true; }
        int LineOf(ExtraOptionsMod mod) => file.Lines.FindIndex(l => Entry(l) is { } e && e.Folder.Equals(mod.Folder, StringComparison.OrdinalIgnoreCase));
        ExtraOptionsMod? Named(string name) => mods.FirstOrDefault(m => m.Id.Equals(name, StringComparison.OrdinalIgnoreCase))
            ?? mods.FirstOrDefault(m => m.Folder.Equals(name, StringComparison.OrdinalIgnoreCase));
        var top = (first ?? []).Select(Named).OfType<ExtraOptionsMod>().DistinctBy(m => m.Folder, StringComparer.OrdinalIgnoreCase)
            .OrderBy(m => m.Id, StringComparer.Ordinal).ThenBy(m => m.Folder, StringComparer.Ordinal).ToList();
        if (top.Count > 0)
        {
            var indices = top.Select(LineOf).Where(i => i >= 0).ToList();
            var block = indices.ToHashSet();
            // The leading comments and blank lines (never a line of an existing mod folder) stay above.
            bool Header(string line) => line.Trim(' ', '\t') is var text && (text.Length == 0 || text.StartsWith('#'))
                && !(Entry(line) is { } e && mods.Any(m => m.Folder.Equals(e.Folder, StringComparison.OrdinalIgnoreCase)));
            var start = 0;
            while (start < file.Lines.Count && !block.Contains(start) && Header(file.Lines[start])) start++;
            var arranged = file.Lines.Take(start).Concat(indices.Select(i => file.Lines[i]))
                .Concat(file.Lines.Select((line, i) => (line, i)).Where(x => x.i >= start && !block.Contains(x.i)).Select(x => x.line)).ToList();
            if (!arranged.SequenceEqual(file.Lines, StringComparer.Ordinal))
            {
                file.Lines.Clear(); file.Lines.AddRange(arranged);
                notes.Add("extraoptions mods: " + string.Join(", ", top.Select(m => m.Folder)) + " first in loadorder.txt");
                changed = true;
            }
        }
        // A mod in a dependency cycle has no valid place (ExtraOptions refuses it anyway): it stays where it is, so the file
        // does not change at every launch.
        var cyclic = mods.Where(m =>
        {
            var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            var next = new Stack<ExtraOptionsMod>(m.Dependencies.Select(Named).OfType<ExtraOptionsMod>());
            while (next.TryPop(out var d))
            {
                if (d.Folder.Equals(m.Folder, StringComparison.OrdinalIgnoreCase)) return true;
                if (seen.Add(d.Folder)) foreach (var x in d.Dependencies.Select(Named).OfType<ExtraOptionsMod>()) next.Push(x);
            }
            return false;
        }).Select(m => m.Folder).ToHashSet(StringComparer.OrdinalIgnoreCase);
        // At most one move per mod per pass; the order settles within mods.Count passes.
        for (var pass = 0; pass < mods.Count; pass++)
        {
            var moved = false;
            foreach (var mod in mods.Where(m => !cyclic.Contains(m.Folder)))
            {
                var at = LineOf(mod);
                var last = mod.Dependencies.Select(Named).Where(d => d is not null && d.Folder != mod.Folder).Select(d => LineOf(d!)).DefaultIfEmpty(-1).Max();
                if (at < 0 || last <= at) continue;
                var line = file.Lines[at];
                file.Lines.RemoveAt(at);
                file.Lines.Insert(last, line); // `last` shifted down by one when the line above it left
                notes.Add("extraoptions mods: " + mod.Folder + " moved after its dependencies in loadorder.txt");
                moved = changed = true;
            }
            if (!moved) break;
        }
        if (changed) WriteLoadOrder(azimuthRoot, file, game);
        return notes;
    }

    /// <summary>The <c>&lt;folder&gt;</c> of a <c>mods/&lt;game folder&gt;/&lt;folder&gt;/…</c> destination of the game, or null.</summary>
    private static string? ModsSubfolder(string destination, string game)
    {
        var path = destination.Replace('\\', '/');
        var prefix = ModsFolder + "/" + ExtraOptionsLayout.GameFolder(game) + "/";
        if (!path.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) return null;
        var rest = path[prefix.Length..];
        var slash = rest.IndexOf('/');
        return slash > 0 ? rest[..slash] : null;
    }

    private static ExtraOptionsModState Classify(string message) => message switch
    {
        "media addon ready" or "database transaction committed in memory" => ExtraOptionsModState.Loaded,
        "database queued; media awaits successful merge" => ExtraOptionsModState.Pending,
        _ when message.StartsWith("plugin ", StringComparison.Ordinal) =>
            message.EndsWith(": initialized", StringComparison.Ordinal) || message.EndsWith(": loaded (no optional initializer)", StringComparison.Ordinal)
            || message.EndsWith(": in-memory database callback complete", StringComparison.Ordinal) || message.EndsWith(": private media directory ready", StringComparison.Ordinal)
                ? ExtraOptionsModState.Loaded : ExtraOptionsModState.Failed,
        _ => ExtraOptionsModState.Failed,
    };

    /// <summary>A failure anywhere wins; a committed merge resolves a queued database.</summary>
    private static ExtraOptionsModState Merge(ExtraOptionsModState current, ExtraOptionsModState next) =>
        current == ExtraOptionsModState.Failed || next == ExtraOptionsModState.Failed ? ExtraOptionsModState.Failed
        : next == ExtraOptionsModState.Loaded ? ExtraOptionsModState.Loaded
        : current == ExtraOptionsModState.Unknown ? next : current;

    private static ExtraOptionsMod Describe(string directory, string folder, bool enabled, int order, bool listed)
    {
        string id = folder, name = folder; string? version = null, error = null;
        var dependencies = new List<string>();
        string? database = "database.json", media = "media", strings = "strings.json";
        var manifest = Path.Combine(directory, "mod.json");
        if (!File.Exists(manifest)) manifest = Path.Combine(directory, "addon.json");
        if (File.Exists(manifest))
        {
            try
            {
                if (new FileInfo(manifest).Length > MaxManifest) throw new JsonException("manifest too large");
                using var document = JsonDocument.Parse(File.ReadAllBytes(manifest));
                var root = document.RootElement;
                if (root.ValueKind != JsonValueKind.Object) throw new JsonException("not an object");
                if (!root.TryGetProperty("format_version", out var format) || format.ValueKind != JsonValueKind.Number || format.GetDouble() != 1
                    || !root.TryGetProperty("id", out var idValue) || idValue.ValueKind != JsonValueKind.String || !ValidId(idValue.GetString()!))
                    error = "unsupported manifest format or invalid ID";
                else id = idValue.GetString()!;
                if (root.TryGetProperty("name", out var n) && n.ValueKind == JsonValueKind.String && n.GetString() is { Length: > 0 } text) name = text;
                else if (error is null) name = id;
                if (root.TryGetProperty("version", out var v) && v.ValueKind == JsonValueKind.String) version = v.GetString();
                if (root.TryGetProperty("dependencies", out var d) && d.ValueKind == JsonValueKind.Array)
                    dependencies.AddRange(d.EnumerateArray().Where(x => x.ValueKind == JsonValueKind.String).Select(x => x.GetString()!));
                database = PathField(root, "database", database);
                media = PathField(root, "media", media);
                strings = PathField(root, "strings", strings);
            }
            catch (Exception e) when (e is JsonException or IOException or UnauthorizedAccessException) { error = "invalid mod manifest"; }
        }
        var pluginsDirectory = Path.Combine(directory, "plugins");
        var plugins = 0;
        try { if (Directory.Exists(pluginsDirectory)) plugins = Directory.EnumerateFiles(pluginsDirectory, "*.dll").Count(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        return new(folder, id, name, version, dependencies, enabled, order, listed,
            media is not null && Directory.Exists(Path.Combine(directory, media)),
            database is not null && File.Exists(Path.Combine(directory, database)),
            strings is not null && File.Exists(Path.Combine(directory, strings)), plugins, error);
    }

    /// <summary>A manifest path field: a string names the file, null turns the part off, absent keeps the default.</summary>
    private static string? PathField(JsonElement root, string field, string? fallback) =>
        !root.TryGetProperty(field, out var value) ? fallback
        : value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    /// <summary>ExtraOptions' id rule: lowercase ASCII letters, digits and single inner hyphens, at most 128.</summary>
    internal static bool ValidId(string id) =>
        id.Length is > 0 and <= 128 && id[0] != '-' && id[^1] != '-' && !id.Contains("--", StringComparison.Ordinal)
        && id.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');

    /// <summary>A load-order line's folder and whether it is enabled; null for a blank line or a plain comment.</summary>
    internal static (string Folder, bool Enabled)? Entry(string line)
    {
        var text = line.Trim(' ', '\t');
        var enabled = true;
        if (text.StartsWith('#')) { enabled = false; text = text[1..].Trim(' ', '\t'); }
        if (text.Length == 0 || text.IndexOfAny(['/', '\\', ':']) >= 0 || text.StartsWith('.')) return null;
        return (text, enabled);
    }

    private sealed record LoadOrder(List<string> Lines, bool Bom, string NewLine);

    private static LoadOrder ReadLoadOrder(string azimuthRoot, string game)
    {
        var path = Path.Combine(ModsDirectory(azimuthRoot, game), LoadOrderFile);
        if (!File.Exists(path)) return new([], false, "\r\n");
        var bytes = File.ReadAllBytes(path);
        if (bytes.Length > MaxLoadOrder) throw new InvalidDataException("loadorder.txt is larger than ExtraOptions reads (64 KiB)");
        var bom = bytes.Length >= 3 && bytes[0] == 0xef && bytes[1] == 0xbb && bytes[2] == 0xbf;
        var text = new UTF8Encoding(false, true).GetString(bytes, bom ? 3 : 0, bytes.Length - (bom ? 3 : 0));
        var newLine = text.Contains("\r\n", StringComparison.Ordinal) || !text.Contains('\n') ? "\r\n" : "\n";
        return new([.. text.Replace("\r\n", "\n").Split('\n')], bom, newLine);
    }

    private static void WriteLoadOrder(string azimuthRoot, LoadOrder file, string game)
    {
        var directory = ModsDirectory(azimuthRoot, game);
        Directory.CreateDirectory(directory);
        var lines = file.Lines;
        var text = string.Join(file.NewLine, lines) + (lines.Count > 0 && lines[^1].Length > 0 ? file.NewLine : "");
        var bytes = new UTF8Encoding(file.Bom).GetPreamble().Concat(new UTF8Encoding(false).GetBytes(text)).ToArray();
        if (bytes.Length > MaxLoadOrder) throw new InvalidDataException("loadorder.txt would exceed ExtraOptions' 64 KiB");
        var path = Path.Combine(directory, LoadOrderFile);
        var temporary = path + ".fi-new";
        File.WriteAllBytes(temporary, bytes);
        File.Move(temporary, path, overwrite: true);
    }

    private static List<string> ReadHeld(string stateFile)
    {
        try { return File.Exists(stateFile) ? JsonSerializer.Deserialize<List<string>>(File.ReadAllBytes(stateFile)) ?? [] : []; }
        catch (JsonException) { return []; }
    }

    private static void WriteHeld(string stateFile, List<string> held, bool empty = false)
    {
        if (held.Count == 0 && !empty) return;
        Directory.CreateDirectory(Path.GetDirectoryName(stateFile)!);
        var temporary = stateFile + ".fi-new";
        File.WriteAllBytes(temporary, JsonSerializer.SerializeToUtf8Bytes(held));
        File.Move(temporary, stateFile, overwrite: true);
    }
}
