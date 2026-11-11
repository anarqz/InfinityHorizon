// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;
using Infinity.Core.Mods;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// One enabled <c>mods\</c> folder of the plan: the folder, the id ExtraOptions logs it by, its lobby entry, and whether it makes the session
/// <c>modded</c> (OD-236: false only for a launcher-installed mod whose catalog entry says <c>countsAsModded: false</c>).
/// </summary>
public sealed record ExtraOptionsPlannedMod(string Folder, string ModId, ModEntry Entry, bool CountsAsModded = true);

/// <summary>
/// Owner 2026-10-28: "Infinity should read which mods are loaded by ExtraOptions and send this when a lobby is opened as the list of
/// mods the player is using". The session's lobby set comes from ExtraOptions' own loader (OD-126), whoever installed the mod:
/// <list type="bullet">
/// <item><see cref="Planned"/>, before the launch: every enabled folder of the game's mods folder (OD-233) with something to load, less
/// the folders the caller names cosmetic (catalog mods whose manifest changes nothing for other players). OD-233 retired OD-166's parity
/// exemptions: no other list leaves a mod out. It sets the gate's <c>modded</c> flag.</item>
/// <item><see cref="State"/>: not ready (no join, never vanilla) until this run's log (<see cref="ExtraOptionsMods.LogFileFor"/>) answers for
/// every planned mod (<see cref="Refresh"/>); then ready with the mods ExtraOptions reports loaded, a failed one left out. Nothing
/// loaded fails the set. <see cref="Expire"/> settles the rest at the supervisor's deadline.</item>
/// </list>
/// Each entry is the mod's ExtraOptions id (in the contract's id form), its <c>mod.json</c> version, and a SHA-256 over exactly the
/// files ExtraOptions loads (<c>mod.json</c>, <c>database.json</c>, <c>strings.json</c>, <c>media\</c>, the plugin DLLs), so only
/// players with the identical files match. What a plugin writes for itself (its settings) is not part of it.
/// </summary>
public sealed class ExtraOptionsModSetSource : IModSetSource
{
    private readonly string _azimuthRoot;
    private readonly string _game;
    private readonly IReadOnlyList<ExtraOptionsPlannedMod> _plan;
    private readonly DateTime _launchUtc;
    private readonly object _lock = new();
    private ModSetState _state;

    /// <param name="launchUtc">When the game was started: an older log is the previous run's and never answers.</param>
    public ExtraOptionsModSetSource(string azimuthRoot, IReadOnlyList<ExtraOptionsPlannedMod> plan, DateTime launchUtc, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(plan);
        _azimuthRoot = azimuthRoot;
        _game = game;
        _plan = [.. plan.OrderBy(p => p.Entry.Id, StringComparer.Ordinal)];
        _launchUtc = launchUtc;
        Planned = ModSets.Checked(_plan.Select(p => p.Entry).ToList());
        _state = _plan.Count == 0 ? ModSetState.Vanilla : ModSetState.NotReady;
    }

    /// <summary>The planned folders (the log is matched against them).</summary>
    public IReadOnlyList<ExtraOptionsPlannedMod> Plan => _plan;

    public IReadOnlyList<ModEntry> Planned { get; }

    /// <summary>OD-236: a planned mod counts as modded (a hand-copied folder always does).</summary>
    public bool Modded => _plan.Any(p => p.CountsAsModded);

    public ModSetState State { get { lock (_lock) return _state; } }

    public event EventHandler<ModSetState>? Changed;

    /// <summary>
    /// Reads this run's log. True once the set is settled (ready or failed); false while the log is the previous run's, or a planned
    /// mod is still pending or not named yet. Raises <see cref="Changed"/> when it settles.
    /// </summary>
    public bool Refresh()
    {
        if (State.Phase != ModSetPhase.NotReady) return true;
        var verdicts = Verdicts();
        if (verdicts is null || verdicts.Any(v => v.State is ExtraOptionsModState.Pending or ExtraOptionsModState.Unknown)) return false;
        return Settle(verdicts);
    }

    /// <summary>
    /// The deadline: a set still not ready settles with what the log says. A mod ExtraOptions never named (nothing failed for it) counts
    /// as loaded; a pending or failed one does not. False when it was settled already.
    /// </summary>
    public bool Expire(string reason)
    {
        ArgumentNullException.ThrowIfNull(reason);
        if (State.Phase != ModSetPhase.NotReady) return false;
        var verdicts = Verdicts();
        if (verdicts is null) return Set(ModSetState.FailedWith(reason));
        return Settle([.. verdicts.Select(v => v.State == ExtraOptionsModState.Unknown ? v with { State = ExtraOptionsModState.Loaded } : v)]);
    }

    private sealed record Verdict(ExtraOptionsPlannedMod Mod, ExtraOptionsModState State);

    /// <summary>Each planned mod's state in this run's log, or null while the log is missing or the previous run's.</summary>
    private List<Verdict>? Verdicts()
    {
        var log = new FileInfo(Path.Combine(_azimuthRoot, ExtraOptionsMods.LogFileFor(_game)));
        try { if (!log.Exists || log.LastWriteTimeUtc < _launchUtc.AddSeconds(-5)) return null; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
        IReadOnlyDictionary<string, ExtraOptionsModStatus> status;
        try { status = ExtraOptionsMods.ReadStatus(_azimuthRoot, ExtraOptionsMods.Scan(_azimuthRoot, _game), _game); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException) { return null; }
        return [.. _plan.Select(p => new Verdict(p,
            status.TryGetValue(p.Folder, out var byFolder) ? byFolder.State
            : status.TryGetValue(p.ModId, out var byId) ? byId.State : ExtraOptionsModState.Unknown))];
    }

    private bool Settle(List<Verdict> verdicts)
    {
        var loaded = verdicts.Where(v => v.State == ExtraOptionsModState.Loaded).Select(v => v.Mod.Entry).ToList();
        return Set(loaded.Count == 0 ? ModSetState.FailedWith("not_loaded") : ModSetState.ReadyWith(loaded));
    }

    private bool Set(ModSetState next)
    {
        lock (_lock)
        {
            if (_state.Phase != ModSetPhase.NotReady) return false;
            _state = next;
        }
        Changed?.Invoke(this, next);
        return true;
    }

    /// <summary>
    /// The plan before a launch (game closed, after the load order was prepared): every enabled folder of the game's mods folder with
    /// loadable content and a manifest ExtraOptions accepts, less <paramref name="cosmeticFolders"/>. Reads and hashes the files: call it off
    /// the UI thread.
    /// </summary>
    /// <param name="hash">
    /// False: the files are not read (<see cref="Unhashed"/> stands for each fingerprint), the plan a launch declares when hashing failed.
    /// T-1110-NOMODMATCH: the set is shown to other players only, so a launch never stops for its mods: more than
    /// <see cref="MatchmakingLimits.ModsMax"/> declares the first ones by id (those that count as modded first, so the cut never hides the
    /// <c>modded</c> flag), and files that cannot be read are declared unhashed.
    /// </param>
    /// <param name="notModdedFolders">
    /// OD-236: the folders of launcher-installed mods that do not count as modded (the catalog's <c>countsAsModded: false</c>); every other
    /// planned folder, hand-copied ones included, counts (<see cref="ExtraOptionsPlannedMod.CountsAsModded"/>).
    /// </param>
    public static IReadOnlyList<ExtraOptionsPlannedMod> PlanFor(string azimuthRoot, IReadOnlySet<string> cosmeticFolders, string game = ModManifest.DefaultGame,
        bool hash = true, IReadOnlySet<string>? notModdedFolders = null)
    {
        var plan = PlanAll(azimuthRoot, cosmeticFolders, game, hash, notModdedFolders);
        return plan.Count <= MatchmakingLimits.ModsMax ? plan
            : [.. plan.OrderBy(p => p.CountsAsModded ? 0 : 1).ThenBy(p => p.Entry.Id, StringComparer.Ordinal).Take(MatchmakingLimits.ModsMax)];
    }

    /// <summary>The fingerprint of a mod whose files could not be read (SHA-256 of nothing): display only.</summary>
    public const string Unhashed = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// <summary>How many mods <see cref="PlanFor"/> would declare without the cap (for the log).</summary>
    public static int CountFor(string azimuthRoot, IReadOnlySet<string> cosmeticFolders, string game = ModManifest.DefaultGame) =>
        PlanAll(azimuthRoot, cosmeticFolders, game, hash: false, null).Count;

    private static List<ExtraOptionsPlannedMod> PlanAll(string azimuthRoot, IReadOnlySet<string> cosmeticFolders, string game, bool hash,
        IReadOnlySet<string>? notModdedFolders)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(cosmeticFolders);
        var plan = new List<ExtraOptionsPlannedMod>();
        var ids = new HashSet<string>(StringComparer.Ordinal);
        foreach (var mod in ExtraOptionsMods.Scan(azimuthRoot, game))
        {
            if (!Loadable(mod)) continue;
            if (cosmeticFolders.Contains(mod.Folder)) continue;
            // two folders whose ids read the same in the contract's form: the later one gets -2, -3, ...
            var baseId = LobbyId(mod.Id); var id = baseId;
            for (var n = 2; !ids.Add(id); n++)
            {
                var suffix = "-" + n.ToString(System.Globalization.CultureInfo.InvariantCulture);
                id = (baseId.Length + suffix.Length > 64 ? baseId[..(64 - suffix.Length)] : baseId) + suffix;
            }
            var entry = new ModEntry { Id = id, Version = LobbyVersion(mod.Version),
                Fingerprint = hash ? Fingerprint(Path.Combine(ExtraOptionsMods.ModsDirectory(azimuthRoot, game), mod.Folder)) : Unhashed };
            plan.Add(new ExtraOptionsPlannedMod(mod.Folder, mod.Id, entry, notModdedFolders?.Contains(mod.Folder) != true));
        }
        return plan;
    }

    /// <summary>Enabled, a manifest ExtraOptions accepts, and something to load.</summary>
    private static bool Loadable(ExtraOptionsMod mod) =>
        mod.Enabled && mod.ManifestError is null && (mod.HasMedia || mod.HasDatabase || mod.HasStrings || mod.Plugins > 0);

    /// <summary>The contract's id form (MATCHMAKING_API.md §2.7): lower case, <c>[a-z0-9._-]</c>, a letter or digit first, at most 64.</summary>
    public static string LobbyId(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        var chars = id.ToLowerInvariant().Select(c => char.IsAsciiDigit(c) || char.IsAsciiLetterLower(c) || c is '.' or '_' or '-' ? c : '-').ToArray();
        var text = new string(chars);
        if (text.Length == 0 || !(char.IsAsciiDigit(text[0]) || char.IsAsciiLetterLower(text[0]))) text = "m" + text;
        return text.Length > 64 ? text[..64] : text;
    }

    /// <summary>The contract's version form: <c>[A-Za-z0-9._+-]</c>, 1 to 16 characters; none: <c>0</c>.</summary>
    public static string LobbyVersion(string? version)
    {
        var text = new string((version ?? "").Where(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '_' or '+' or '-').ToArray());
        return text.Length == 0 ? "0" : text.Length > 16 ? text[..16] : text;
    }

    /// <summary>
    /// SHA-256 (lower-case hex) over one line per loadable file, sorted: <c>path\tlength\tsha256\n</c>, the path relative to the mod
    /// folder, lower case, with <c>/</c>. Linked entries are skipped (ExtraOptions' scan skips them too).
    /// </summary>
    public static string Fingerprint(string modFolder)
    {
        ArgumentNullException.ThrowIfNull(modFolder);
        var root = new DirectoryInfo(modFolder);
        var files = new List<FileInfo>();
        foreach (var name in new[] { "mod.json", "database.json", "strings.json" })
            if (new FileInfo(Path.Combine(root.FullName, name)) is { Exists: true, LinkTarget: null } file) files.Add(file);
        var everything = new EnumerationOptions { RecurseSubdirectories = true, AttributesToSkip = FileAttributes.ReparsePoint };
        if (new DirectoryInfo(Path.Combine(root.FullName, "media")) is { Exists: true, LinkTarget: null } media) files.AddRange(media.EnumerateFiles("*", everything));
        if (new DirectoryInfo(Path.Combine(root.FullName, "plugins")) is { Exists: true, LinkTarget: null } plugins)
            files.AddRange(plugins.EnumerateFiles("*.dll", everything));
        var lines = files.Select(f => (Path: Path.GetRelativePath(root.FullName, f.FullName).Replace('\\', '/').ToLowerInvariant(), File: f))
            .OrderBy(f => f.Path, StringComparer.Ordinal)
            .Select(f => f.Path + "\t" + f.File.Length.ToString(System.Globalization.CultureInfo.InvariantCulture) + "\t" + FileHash(f.File) + "\n");
        return Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(string.Concat(lines))));
    }

    /// <summary>File hashes by path, length and write time: a large media pack is read once per launcher run.</summary>
    private static readonly ConcurrentDictionary<(string Path, long Length, long Ticks), string> Hashes = new();

    private static string FileHash(FileInfo file)
    {
        var key = (file.FullName.ToUpperInvariant(), file.Length, file.LastWriteTimeUtc.Ticks);
        if (Hashes.TryGetValue(key, out var known)) return known;
        using var stream = new FileStream(file.FullName, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete, 1 << 20);
        var hash = Convert.ToHexStringLower(SHA256.HashData(stream));
        if (Hashes.Count > 4096) Hashes.Clear();
        Hashes[key] = hash;
        return hash;
    }
}
