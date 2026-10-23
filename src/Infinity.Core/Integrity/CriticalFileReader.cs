// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Immutable;
using System.Runtime.InteropServices;
using System.Runtime.Versioning;
using System.Security.Cryptography;
using Microsoft.Win32.SafeHandles;

namespace Infinity.Core.Integrity;

public enum InstallationState { Compatible, Incompatible, Inconclusive }
public sealed record InstallationFinding(string Root, string Path, string Reason, string? Sha256 = null, long? ByteLength = null);
public sealed record CriticalFile(string FileId, string Path, long? ExpectedByteLength = null);
public sealed record CriticalObservation(string FileId, long? ByteLength, string? Sha256);
/// <summary>A listed file's digest with the size and modification time it was hashed at (OD-113: an unchanged file is not rehashed).</summary>
public sealed record KnownDigest(string Path, long Length, long ModifiedTicks, string Sha256);

/// <summary>Hashes only the short explicit game-relative list. Never enumerates directories or modules.</summary>
public static class CriticalFileReader
{
    public const int MaxFiles = 16;
    public const long MaxFileBytes = 256L * 1024 * 1024;

    public static Task<CriticalFileSnapshot> ReadAsync(string mount, IReadOnlyList<CriticalFile> files, CancellationToken cancellation = default)
        => ReadAsync(mount, files, known: null, cancellation);

    /// <summary>
    /// As <see cref="ReadAsync(string, IReadOnlyList{CriticalFile}, CancellationToken)"/>; a listed file whose size and
    /// modification time equal a <paramref name="known"/> entry of its path reuses that digest instead of being rehashed
    /// (local evidence only: the server still compares every submitted digest).
    /// </summary>
    public static async Task<CriticalFileSnapshot> ReadAsync(string mount, IReadOnlyList<CriticalFile> files, IReadOnlyCollection<KnownDigest>? known,
        CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(files);
        if (files.Count is < 1 or > MaxFiles) throw new InvalidDataException("Invalid critical-file list");
        var root = ContentPath.ResolveDirectory(mount);
        if (!ContentPath.SafeWithin(root, root))
            throw new InvalidDataException("Invalid game-content root");
        var ids = new HashSet<string>(StringComparer.Ordinal);
        var paths = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var file in files)
            if (file is null || !FileId(file.FileId) || !RelativePath(file.Path) || !ids.Add(file.FileId) || !paths.Add(file.Path)
                || file.ExpectedByteLength is < 0 or > MaxFileBytes)
                throw new InvalidDataException("Invalid critical-file list");
        var observations = ImmutableArray.CreateBuilder<CriticalObservation>();
        var findings = ImmutableArray.CreateBuilder<InstallationFinding>();
        var held = new List<HeldFile>();
        var digests = ImmutableArray.CreateBuilder<KnownDigest>();
        var reused = 0;
        try
        {
            foreach (var file in files)
            {
                cancellation.ThrowIfCancellationRequested();
                var full = Path.Combine(root, file.Path.Replace('/', Path.DirectorySeparatorChar));
                try
                {
                    if (!ContentPath.SafeWithin(root, full)) { Absent(file, "unsafe_path"); continue; }
                    var input = new FileStream(full, FileMode.Open, FileAccess.Read, FileShare.Read, 128 * 1024, FileOptions.Asynchronous | FileOptions.SequentialScan);
                    var retained = false;
                    try
                    {
                        if (!ContentPath.SafeWithin(root, full) || (OperatingSystem.IsWindows() && !HandleMatches(input.SafeFileHandle, full))) { Absent(file, "unsafe_path"); continue; }
                        var length = input.Length;
                        if (length > MaxFileBytes || file.ExpectedByteLength is { } expected && length != expected)
                        {
                            observations.Add(new(file.FileId, length <= 9007199254740991L ? length : null, null));
                            findings.Add(new("game_content", file.Path, length > MaxFileBytes ? "scan_limit" : "unapproved_file", ByteLength: length));
                            continue;
                        }
                        var modified = File.GetLastWriteTimeUtc(full);
                        var cached = known?.FirstOrDefault(k => k is not null && k.Path == file.Path && k.Length == length
                            && k.ModifiedTicks == modified.Ticks && Digest(k.Sha256));
                        var sha = cached?.Sha256 ?? Convert.ToHexStringLower(await SHA256.HashDataAsync(input, cancellation).ConfigureAwait(false));
                        if (input.Length != length || File.GetLastWriteTimeUtc(full) != modified) { Absent(file, "changed_during_scan"); continue; }
                        if (cached is not null) reused++;
                        observations.Add(new(file.FileId, length, sha));
                        digests.Add(new(file.Path, length, modified.Ticks, sha));
                        held.Add(new(full, input, length, modified)); retained = true;
                    }
                    finally { if (!retained) await input.DisposeAsync().ConfigureAwait(false); }
                }
                catch (FileNotFoundException) { Absent(file, "missing"); }
                catch (DirectoryNotFoundException) { Absent(file, "missing"); }
                catch (Exception error) when (FileError(error)) { Absent(file, "unreadable"); }
            }
            var result = new CriticalFileSnapshot(root, observations.ToImmutable(), findings.ToImmutable(), held, digests.ToImmutable(), reused);
            held = [];
            return result;
        }
        finally { foreach (var file in held) file.Stream.Dispose(); }

        void Absent(CriticalFile file, string reason)
        {
            observations.Add(new(file.FileId, null, null));
            findings.Add(new("game_content", file.Path, reason));
        }
    }

    internal static bool FileId(string? value) => value is { Length: > 0 and <= 32 } && value.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '-');
    internal static bool Digest(string? sha) => sha is { Length: 64 } && sha.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f');
    public static bool RelativePath(string? path) => path is { Length: > 0 and <= 240 } && !path.Contains('\\')
        && path.Split('/').All(segment => segment.Length > 0 && segment is not "." and not ".." && !segment.EndsWith('.') && !segment.EndsWith(' ')
            && segment.All(c => c >= 32 && c < 127 && "<>:\"|?*".IndexOf(c) < 0) && !Reserved(segment.Split('.')[0].TrimEnd(' ', '.')));
    private static bool Reserved(string name) => name.Equals("CON", StringComparison.OrdinalIgnoreCase) || name.Equals("PRN", StringComparison.OrdinalIgnoreCase)
        || name.Equals("AUX", StringComparison.OrdinalIgnoreCase) || name.Equals("NUL", StringComparison.OrdinalIgnoreCase)
        || name.Equals("CONIN$", StringComparison.OrdinalIgnoreCase) || name.Equals("CONOUT$", StringComparison.OrdinalIgnoreCase)
        || name.Length == 4 && (name.StartsWith("COM", StringComparison.OrdinalIgnoreCase) || name.StartsWith("LPT", StringComparison.OrdinalIgnoreCase)) && name[3] is >= '0' and <= '9';
    internal static bool SafeAncestors(string path)
    {
        for (FileSystemInfo? current = File.Exists(path) ? new FileInfo(path) : new DirectoryInfo(path); current is not null;
            current = current is FileInfo file ? file.Directory : ((DirectoryInfo)current).Parent)
            if (current.Exists && (current.Attributes & FileAttributes.ReparsePoint) != 0) return false;
        return true;
    }
    [SupportedOSPlatform("windows")]
    internal static bool HandleMatches(SafeFileHandle handle, string expected)
    {
        try { return Path.GetFullPath(expected).Equals(ContentPath.FinalPath(handle), StringComparison.OrdinalIgnoreCase); }
        catch (Exception error) when (FileError(error)) { return false; }
    }
    internal static bool FileError(Exception error) => error is IOException or InvalidDataException or UnauthorizedAccessException or ArgumentException
        or NotSupportedException or System.Security.SecurityException;
    internal sealed record HeldFile(string Path, FileStream Stream, long Length, DateTime Modified);
}

public sealed class CriticalFileSnapshot : IDisposable
{
    private readonly object _sync = new();
    private readonly List<CriticalFileReader.HeldFile> _held;
    private bool _disposed, _handlesReleased;
    private readonly string _root;
    public ImmutableArray<CriticalObservation> Files { get; }
    public ImmutableArray<InstallationFinding> Findings { get; }
    /// <summary>The hashed files' digests with the stamps they were taken at, for <see cref="GameVerificationCache"/>.</summary>
    public ImmutableArray<KnownDigest> Digests { get; }
    /// <summary>How many listed files reused a known digest instead of being rehashed.</summary>
    public int ReusedDigests { get; }
    internal CriticalFileSnapshot(string root, ImmutableArray<CriticalObservation> files, ImmutableArray<InstallationFinding> findings, List<CriticalFileReader.HeldFile> held,
        ImmutableArray<KnownDigest> digests = default, int reusedDigests = 0)
    { _root = root; Files = files; Findings = findings; _held = held; Digests = digests.IsDefault ? [] : digests; ReusedDigests = reusedDigests; }
    public bool IsCurrent()
    {
        lock (_sync)
        {
            if (_disposed || !Findings.IsEmpty || Files.Any(file => file.Sha256 is null)) return false;
            try
            {
                return _held.All(file => ContentPath.SafeWithin(_root, file.Path) && new FileInfo(file.Path).Length == file.Length
                    && File.GetLastWriteTimeUtc(file.Path) == file.Modified
                    && (_handlesReleased || !OperatingSystem.IsWindows() || CriticalFileReader.HandleMatches(file.Stream.SafeFileHandle, file.Path)));
            }
            catch (Exception error) when (CriticalFileReader.FileError(error) || error is ObjectDisposedException) { return false; }
        }
    }
    // Startup observation only: never keep a sharing lock against the game/SQLite reader.
    internal void ReleaseReadHandles()
    {
        lock (_sync)
        {
            if (_handlesReleased) return;
            _handlesReleased = true;
            foreach (var file in _held) file.Stream.Dispose();
        }
    }
    public void Dispose()
    {
        lock (_sync)
        {
            if (_disposed) return;
            _disposed = true; ReleaseReadHandles();
        }
    }
}
