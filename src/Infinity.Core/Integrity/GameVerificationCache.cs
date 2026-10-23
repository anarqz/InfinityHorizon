// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Immutable;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Integrity;

/// <summary>
/// Remembered setup presentation, and (OD-113) the digests of unchanged listed files so a background re-check does not
/// rehash them. Never an online permission or a substitute for the server's check: every check still submits a scan.
/// </summary>
public sealed class GameVerificationCache(string stateDirectory)
{
    private sealed record Stamp(string Path, long Length, long Modified, string? Sha256 = null);
    private sealed record Entry(string Scope, Stamp[] Files);
    private readonly string _path = Path.Combine(stateDirectory, "game-verification-cache.json");
    private Entry? _entry;
    private bool _loaded;

    public static string Scope(params string[] parts) => Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(parts))));

    public bool IsCurrent(string scope, string mount)
    {
        try { return Current(scope, mount) is not null; }
        catch (Exception error) when (CriticalFileReader.FileError(error) || error is JsonException) { return false; }
    }

    /// <summary>The remembered digests of <paramref name="scope"/> whose files are unchanged (size and modification time); empty otherwise.</summary>
    public ImmutableArray<KnownDigest> Known(string scope, string mount)
    {
        try
        {
            return Current(scope, mount) is { } entry
                ? [.. entry.Files.Where(f => CriticalFileReader.Digest(f.Sha256)).Select(f => new KnownDigest(f.Path, f.Length, f.Modified, f.Sha256!))] : [];
        }
        catch (Exception error) when (CriticalFileReader.FileError(error) || error is JsonException) { return []; }
    }

    private Entry? Current(string scope, string mount)
    {
        if (!_loaded)
        {
            _loaded = true;
            if (!CriticalFileReader.SafeAncestors(_path) || !File.Exists(_path)) return null;
            using var input = File.OpenRead(_path);
            var buffer = new byte[8193];
            var count = input.ReadAtLeast(buffer, buffer.Length, throwOnEndOfStream: false);
            if (count > 8192) return null;
            _entry = JsonSerializer.Deserialize<Entry>(buffer.AsSpan(0, count));
        }
        return _entry is { Files.Length: >= 2 and <= CriticalFileReader.MaxFiles } entry && entry.Scope == scope
            && entry.Files.All(f => f is not null && f.Length is >= 0 and <= CriticalFileReader.MaxFileBytes && CriticalFileReader.RelativePath(f.Path) && Matches(mount, f))
            && entry.Files.Select(f => f.Path).Distinct(StringComparer.OrdinalIgnoreCase).Count() == entry.Files.Length
            && entry.Files.Any(f => f.Path == GameContentLocation.Executable) ? entry : null;
    }

    /// <summary>
    /// Remembers a compatible check's digests with the stamps they were hashed at. Nothing is remembered when any file
    /// changed since (its current size or modification time differs), so a stale digest is never paired with a new stamp.
    /// </summary>
    public void Remember(string scope, string mount, IReadOnlyList<KnownDigest> digests)
    {
        try
        {
            if (digests is not { Count: >= 2 and <= CriticalFileReader.MaxFiles }
                || digests.Any(d => d is null || !CriticalFileReader.RelativePath(d.Path) || !CriticalFileReader.Digest(d.Sha256))) return;
            var files = digests.Select(d => new Stamp(d.Path, d.Length, d.ModifiedTicks, d.Sha256)).ToArray();
            if (!files.All(f => Matches(mount, f))) return;
            Write(new Entry(scope, files));
        }
        catch (Exception error) when (CriticalFileReader.FileError(error)) { /* A cache failure never changes the server verdict. */ }
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
            Write(new Entry(scope, files));
        }
        catch (Exception error) when (CriticalFileReader.FileError(error)) { /* A cache failure never changes the server verdict. */ }
    }

    private void Write(Entry entry)
    {
        var bytes = JsonSerializer.SerializeToUtf8Bytes(entry);
        Directory.CreateDirectory(stateDirectory);
        if (bytes.Length > 8192 || !CriticalFileReader.SafeAncestors(_path) || !CriticalFileReader.SafeAncestors(_path + ".tmp")) return;
        File.WriteAllBytes(_path + ".tmp", bytes);
        File.Move(_path + ".tmp", _path, overwrite: true);
        _entry = entry; _loaded = true;
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
