// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Process;

/// <summary>
/// Game caches that belong to one mode (BUG-0001, docs/launcher/ONLINE_OFFLINE.md §11): the C#
/// side of <c>crates/fi-launcher-core/src/mode_cache.rs</c>, same rules and the same crash cases.
/// <para>
/// The game keeps its downloaded drivatar cache and its career drivatar cache in Azimuth's
/// per-title temporary folder, which online and offline sessions share; an online session's cache
/// left there makes the offline game read profiles its spawner will not use (a player found it on
/// 2026-10-18; DIABLO still had the Beta's on 2026-10-19). The launcher owns the switch and keeps
/// the two sets apart. Each cache file is live (where the game reads it), in <c>offline/</c> (the
/// offline set, set aside during an online session) or in <c>discard/</c> (what an online session
/// wrote). Every step is a rename on one volume, and every rule is decided from where the files
/// are, so any step can run again after a crash. Every call needs the game closed.
/// </para>
/// </summary>
public sealed class ModeCaches
{
    /// <summary>Forza Horizon 2's two caches, relative to the Azimuth folder.</summary>
    public static readonly IReadOnlyList<string> Fh2Caches =
    [
        "data/titles/1A96545B/tmp/Turn10Temp.scratch/DrivatarGlobalCache/DownloadCache.bin",
        "data/titles/1A96545B/tmp/Turn10Temp.scratch/DrivatarCareerCache.xml",
    ];

    /// <summary>The stash and its journal (<c>{"online": true|false}</c>), relative to the Azimuth folder.</summary>
    public const string Fh2StashDir = "data/fi-launcher/mode-cache";

    private sealed record Journal(bool Online);

    private readonly string _gameRoot;
    private readonly string _stashRoot;
    private readonly IReadOnlyList<string> _caches;

    /// <param name="caches">Relative to <paramref name="gameRoot"/>, written with '/': plain names only (no "..", drive or root).</param>
    public ModeCaches(string gameRoot, string stashRoot, IEnumerable<string> caches)
    {
        ArgumentNullException.ThrowIfNull(caches);
        _gameRoot = gameRoot ?? throw new ArgumentNullException(nameof(gameRoot));
        _stashRoot = stashRoot ?? throw new ArgumentNullException(nameof(stashRoot));
        _caches = [.. caches.Select(Relative)];
    }

    /// <summary>The Forza Horizon 2 set for the Azimuth folder <paramref name="azimuth"/>.</summary>
    public static ModeCaches Fh2(string azimuth) =>
        new(azimuth, Path.Combine(azimuth, Relative(Fh2StashDir)), Fh2Caches);

    public string StashRoot => _stashRoot;

    private string JournalPath => Path.Combine(_stashRoot, "journal.json");

    /// <summary>
    /// Whether an online session started and was not ended. An unreadable journal counts as
    /// online: ending a session that was not running only costs a cache the game rebuilds.
    /// </summary>
    public bool Online()
    {
        if (!File.Exists(JournalPath))
        {
            return false;
        }
        try
        {
            // As serde reads it: an object with a boolean "online", else unreadable.
            var json = JsonSerializer.Deserialize<JsonElement>(File.ReadAllText(JournalPath));
            return json.ValueKind != JsonValueKind.Object || !json.TryGetProperty("online", out var online)
                || online.ValueKind is not (JsonValueKind.True or JsonValueKind.False) || online.GetBoolean();
        }
        catch (JsonException)
        {
            return true;
        }
    }

    /// <summary>Finishes whatever switch a crash interrupted; whether an online session was ended.</summary>
    public bool Recover()
    {
        if (Online())
        {
            EndOnline();
            return true;
        }
        // a half-done BeginOnline: put back what it moved aside
        foreach (var cache in _caches)
        {
            var (live, stashed) = (Live(cache), Stashed(cache));
            if (File.Exists(stashed))
            {
                if (File.Exists(live))
                {
                    File.Delete(stashed); // offline play rebuilt it since: the live one is newer
                }
                else
                {
                    Move(stashed, live);
                }
            }
        }
        return false;
    }

    /// <summary>Before an online launch: the offline set goes into the stash, then the journal says online.</summary>
    public void BeginOnline()
    {
        Recover();
        var discard = Path.Combine(_stashRoot, "discard");
        if (Directory.Exists(discard))
        {
            Directory.Delete(discard, recursive: true);
        }
        foreach (var cache in _caches)
        {
            if (File.Exists(Live(cache)))
            {
                Move(Live(cache), Stashed(cache));
            }
        }
        WriteJournal(true);
    }

    /// <summary>After the online game exited: its caches leave, and the offline set comes back.</summary>
    public void EndOnline()
    {
        foreach (var cache in _caches)
        {
            var (live, stashed, discarded) = (Live(cache), Stashed(cache), Discarded(cache));
            if (File.Exists(stashed))
            {
                if (File.Exists(live))
                {
                    Move(live, discarded);
                }
                Move(stashed, live);
            }
            else if (!File.Exists(discarded) && File.Exists(live))
            {
                Move(live, discarded); // written online, and offline never had it
            }
            // else: done already (its online copy is in discard/), or there was nothing
        }
        WriteJournal(false);
    }

    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);

    private string Live(string cache) => Path.Combine(_gameRoot, cache);

    private string Stashed(string cache) => Path.Combine(_stashRoot, "offline", cache);

    private string Discarded(string cache) => Path.Combine(_stashRoot, "discard", cache);

    private void WriteJournal(bool online)
    {
        Directory.CreateDirectory(_stashRoot);
        var temp = JournalPath + ".tmp";
        File.WriteAllText(temp, JsonSerializer.Serialize(new Journal(online), JsonOptions));
        File.Move(temp, JournalPath, overwrite: true);
    }

    /// <summary>A rename on one volume that replaces <paramref name="to"/>; no copy fallback (a copy can be left half-written).</summary>
    private static void Move(string from, string to)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(to)!);
        if (Path.GetPathRoot(Path.GetFullPath(from)) != Path.GetPathRoot(Path.GetFullPath(to)))
        {
            throw new IOException($"{from} and {to} are on different volumes");
        }
        File.Move(from, to, overwrite: true);
    }

    /// <summary><paramref name="text"/> as a relative path of plain names, in this OS's separators.</summary>
    private static string Relative(string text)
    {
        var parts = text.Split('/', StringSplitOptions.RemoveEmptyEntries);
        if (parts.Length == 0 || text.StartsWith('/') || text.Contains(':', StringComparison.Ordinal) || text.Contains('\\', StringComparison.Ordinal)
            || parts.Any(p => p is "." or ".."))
        {
            throw new ArgumentException($"not a plain relative cache path: {text}", nameof(text));
        }
        return Path.Combine(parts);
    }
}
