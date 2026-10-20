// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Immutable;
using System.Runtime.InteropServices;
using System.Runtime.Versioning;
using Microsoft.Win32.SafeHandles;

namespace Infinity.Core.Diagnostics;

public enum CrashDumpAbsentReason
{
    None, NoMatchingDump, InvalidLocation, UnsafeLink, TooManyFiles, TooManyCandidates,
    Unreadable, TooLarge, InvalidDump, UnrelatedProcess, OutsideIncidentWindow,
}

public sealed record CrashDumpCapture(CrashDumpInfo? Info, ImmutableArray<byte> Bytes, CrashDumpAbsentReason Reason)
{
    public bool HasDump => Info is not null && !Bytes.IsDefaultOrEmpty && Reason == CrashDumpAbsentReason.None;
}

/// <summary>Before-launch names and modification times only; no prior dump contents are read.</summary>
public sealed record CrashDumpSnapshot
{
    internal string DirectoryPath { get; }
    internal ImmutableDictionary<string, DateTime> ExistingFiles { get; }
    public CrashDumpAbsentReason Failure { get; }

    internal CrashDumpSnapshot(string directoryPath, ImmutableDictionary<string, DateTime> existingFiles, CrashDumpAbsentReason failure)
    { DirectoryPath = directoryPath; ExistingFiles = existingFiles; Failure = failure; }
}

/// <summary>Optional, fail-closed attachment selection after an abnormal child exit.</summary>
public static class CrashDumpCollector
{
    public const int MaxDumpBytes = MinidumpInspector.MaxDumpBytes;
    public const int MaxDirectoryEntries = 256;
    public const int MaxCandidates = 8;

    public static CrashDumpSnapshot Snapshot(string installRoot)
    {
        var empty = ImmutableDictionary.Create<string, DateTime>(StringComparer.OrdinalIgnoreCase);
        try
        {
            var root = Path.GetFullPath(installRoot);
            if (root.StartsWith(@"\\", StringComparison.Ordinal)) return new("", empty, CrashDumpAbsentReason.InvalidLocation);
            var directory = Path.Combine(root, "data", "logs");
            if (!SafeAncestors(directory)) return new(directory, empty, CrashDumpAbsentReason.UnsafeLink);
            if (!Directory.Exists(directory)) return new(directory, empty, CrashDumpAbsentReason.None);
            var files = empty.ToBuilder();
            var count = 0;
            foreach (var entry in new DirectoryInfo(directory).EnumerateFileSystemInfos())
            {
                if (++count > MaxDirectoryEntries) return new(directory, empty, CrashDumpAbsentReason.TooManyFiles);
                if (!entry.Extension.Equals(".dmp", StringComparison.OrdinalIgnoreCase)) continue;
                // Snapshot records links too, but will never open them as candidates.
                files[entry.Name] = entry.LastWriteTimeUtc;
            }
            return new(directory, files.ToImmutable(), CrashDumpAbsentReason.None);
        }
        catch (Exception error) when (IsFileError(error)) { return new("", empty, CrashDumpAbsentReason.Unreadable); }
    }

    public static CrashDumpCapture Capture(CrashDumpSnapshot snapshot, int processId, DateTimeOffset startedUtc, DateTimeOffset endedUtc)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        if (snapshot.Failure != CrashDumpAbsentReason.None) return Absent(snapshot.Failure);
        if (processId <= 0 || endedUtc < startedUtc) return Absent(CrashDumpAbsentReason.InvalidLocation);
        try
        {
            if (!SafeAncestors(snapshot.DirectoryPath)) return Absent(CrashDumpAbsentReason.UnsafeLink);
            if (!Directory.Exists(snapshot.DirectoryPath)) return Absent(CrashDumpAbsentReason.NoMatchingDump);
            var candidates = new List<FileInfo>(MaxCandidates);
            var count = 0;
            foreach (var entry in new DirectoryInfo(snapshot.DirectoryPath).EnumerateFileSystemInfos())
            {
                if (++count > MaxDirectoryEntries) return Absent(CrashDumpAbsentReason.TooManyFiles);
                if (entry is not FileInfo file || !file.Extension.Equals(".dmp", StringComparison.OrdinalIgnoreCase)) continue;
                if (snapshot.ExistingFiles.TryGetValue(file.Name, out var previous) && previous == file.LastWriteTimeUtc) continue;
                if (candidates.Count == MaxCandidates) return Absent(CrashDumpAbsentReason.TooManyCandidates);
                candidates.Add(file);
            }
            candidates.Sort((left, right) => right.LastWriteTimeUtc.CompareTo(left.LastWriteTimeUtc));
            var reason = CrashDumpAbsentReason.NoMatchingDump;
            foreach (var file in candidates)
            {
                if ((file.Attributes & FileAttributes.ReparsePoint) != 0) { reason = CrashDumpAbsentReason.UnsafeLink; continue; }
                // Timestamp precision in the minidump header is one second. File mtime alone is never identity evidence.
                if (file.LastWriteTimeUtc < startedUtc.UtcDateTime.AddSeconds(-1)
                    || file.LastWriteTimeUtc > endedUtc.UtcDateTime.AddSeconds(2))
                { reason = CrashDumpAbsentReason.OutsideIncidentWindow; continue; }
                var selected = ReadCandidate(file, snapshot.DirectoryPath, processId, startedUtc, endedUtc);
                if (selected.HasDump) return selected;
                reason = selected.Reason;
            }
            return Absent(reason);
        }
        catch (Exception error) when (IsFileError(error)) { return Absent(CrashDumpAbsentReason.Unreadable); }
    }

    private static CrashDumpCapture ReadCandidate(FileInfo file, string directory, int processId, DateTimeOffset started, DateTimeOffset ended)
    {
        try
        {
            using var stream = new FileStream(file.FullName, FileMode.Open, FileAccess.Read, FileShare.Read, 4096, FileOptions.SequentialScan);
            if (!SafeAncestors(directory) || (File.GetAttributes(file.FullName) & FileAttributes.ReparsePoint) != 0
                || (OperatingSystem.IsWindows() && !HandlePathMatches(stream.SafeFileHandle, file.FullName))) return Absent(CrashDumpAbsentReason.UnsafeLink);
            if (stream.Length > MaxDumpBytes) return Absent(CrashDumpAbsentReason.TooLarge);
            if (stream.Length < 32) return Absent(CrashDumpAbsentReason.InvalidDump);
            var bytes = GC.AllocateUninitializedArray<byte>((int)stream.Length);
            stream.ReadExactly(bytes);
            if (stream.ReadByte() != -1 || stream.Length != bytes.Length) return Absent(CrashDumpAbsentReason.InvalidDump);
            if (!MinidumpInspector.TryRead(bytes, out var info, out _)) return Absent(CrashDumpAbsentReason.InvalidDump);
            if (info!.ProcessId != (uint)processId) return Absent(CrashDumpAbsentReason.UnrelatedProcess);
            if (info.TimestampUtc.ToUnixTimeSeconds() < started.ToUnixTimeSeconds()
                || info.TimestampUtc.ToUnixTimeSeconds() > ended.ToUnixTimeSeconds()) return Absent(CrashDumpAbsentReason.OutsideIncidentWindow);
            // Ownership of this array ends here; nobody retains a mutable reference.
            return new(info, ImmutableCollectionsMarshal.AsImmutableArray(bytes), CrashDumpAbsentReason.None);
        }
        catch (Exception error) when (IsFileError(error)) { return Absent(CrashDumpAbsentReason.Unreadable); }
    }

    private static bool SafeAncestors(string path)
    {
        for (var current = new DirectoryInfo(path); current is not null; current = current.Parent)
            if (current.Exists && (current.Attributes & FileAttributes.ReparsePoint) != 0) return false;
        return true;
    }

    [SupportedOSPlatform("windows")]
    private static bool HandlePathMatches(SafeFileHandle handle, string expected)
    {
        var path = new char[1024];
        var length = Native.GetFinalPathNameByHandleW(handle, path, (uint)path.Length, 0);
        if (length == 0 || length >= path.Length) return false;
        var actual = new string(path, 0, (int)length);
        if (actual.StartsWith(@"\\?\", StringComparison.Ordinal)) actual = actual[4..];
        return string.Equals(Path.GetFullPath(expected), actual, StringComparison.OrdinalIgnoreCase);
    }

    private static CrashDumpCapture Absent(CrashDumpAbsentReason reason) => new(null, ImmutableArray<byte>.Empty, reason);
    private static bool IsFileError(Exception error) => error is IOException or UnauthorizedAccessException
        or ArgumentException or NotSupportedException or System.Security.SecurityException;

    private static class Native
    {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        internal static extern uint GetFinalPathNameByHandleW(SafeFileHandle file, [Out] char[] path, uint length, uint flags);
    }
}
