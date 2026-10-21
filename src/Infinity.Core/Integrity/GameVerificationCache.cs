// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Integrity;

/// <summary>Remembered setup presentation only. Never an online permission or a substitute for a fresh server check.</summary>
public sealed class GameVerificationCache(string stateDirectory)
{
    private sealed record Stamp(string Path, long Length, long Modified);
    private sealed record Entry(string Scope, Stamp[] Files);
    private readonly string _path = Path.Combine(stateDirectory, "game-verification-cache.json");
    private Entry? _entry;
    private bool _loaded;

    public static string Scope(params string[] parts) => Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(parts))));

    public bool IsCurrent(string scope, string mount)
    {
        try
        {
            if (!_loaded)
            {
                _loaded = true;
                if (!CriticalFileReader.SafeAncestors(_path) || !File.Exists(_path)) return false;
                using var input = File.OpenRead(_path);
                var buffer = new byte[8193];
                var count = input.ReadAtLeast(buffer, buffer.Length, throwOnEndOfStream: false);
                if (count > 8192) return false;
                _entry = JsonSerializer.Deserialize<Entry>(buffer.AsSpan(0, count));
            }
            return _entry is { Files.Length: >= 2 and <= CriticalFileReader.MaxFiles } entry && entry.Scope == scope
                && entry.Files.All(f => f is not null && f.Length is >= 0 and <= CriticalFileReader.MaxFileBytes && CriticalFileReader.RelativePath(f.Path) && Matches(mount, f))
                && entry.Files.Select(f => f.Path).Distinct(StringComparer.OrdinalIgnoreCase).Count() == entry.Files.Length
                && entry.Files.Any(f => f.Path == GameContentLocation.Executable);
        }
        catch (Exception error) when (CriticalFileReader.FileError(error) || error is JsonException) { return false; }
    }

    public void Remember(string scope, string mount, IReadOnlyList<string> paths)
    {
        try
        {
            if (paths.Count is < 2 or > CriticalFileReader.MaxFiles || paths.Any(p => !CriticalFileReader.RelativePath(p))) return;
            var files = paths.Select(p =>
            {
                var path = Path.Combine(mount, p.Replace('/', Path.DirectorySeparatorChar));
                if (!ContentPath.SafeWithin(mount, path)) throw new IOException("Unsafe cache input");
                var info = new FileInfo(path);
                return new Stamp(p, info.Length, info.LastWriteTimeUtc.Ticks);
            }).ToArray();
            var entry = new Entry(scope, files);
            var bytes = JsonSerializer.SerializeToUtf8Bytes(entry);
            Directory.CreateDirectory(stateDirectory);
            if (bytes.Length > 8192 || !CriticalFileReader.SafeAncestors(_path) || !CriticalFileReader.SafeAncestors(_path + ".tmp")) return;
            File.WriteAllBytes(_path + ".tmp", bytes);
            File.Move(_path + ".tmp", _path, overwrite: true);
            _entry = entry; _loaded = true;
        }
        catch (Exception error) when (CriticalFileReader.FileError(error)) { /* A cache failure never changes the server verdict. */ }
    }

    public void Invalidate()
    {
        _entry = null; _loaded = true;
        try { if (CriticalFileReader.SafeAncestors(_path)) File.Delete(_path); }
        catch (Exception error) when (CriticalFileReader.FileError(error)) { }
    }

    private static bool Matches(string mount, Stamp stamp)
    {
        var path = Path.Combine(mount, stamp.Path.Replace('/', Path.DirectorySeparatorChar));
        var info = new FileInfo(path);
        return ContentPath.SafeWithin(mount, path) && info.Exists && info.Length == stamp.Length && info.LastWriteTimeUtc.Ticks == stamp.Modified;
    }
}
