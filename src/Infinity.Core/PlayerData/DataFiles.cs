// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;

namespace Infinity.Core.PlayerData;

/// <summary>Confines maintenance to one install; never follows directory or file links.</summary>
internal sealed class DataFiles(string root)
{
    internal string Root { get; } = Path.TrimEndingDirectorySeparator(Path.GetFullPath(root));
    internal const long MaxTreeBytes = 512L * 1024 * 1024;
    internal const int MaxTreeFiles = 256;

    internal string Check(string path)
    {
        var full = Path.GetFullPath(path);
        if (!string.Equals(full, Root, StringComparison.OrdinalIgnoreCase) &&
            !full.StartsWith(Root + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase))
            throw new IOException("Data path is outside the selected installation.");
        for (var current = full; current is not null; current = Path.GetDirectoryName(current))
        {
            if ((File.Exists(current) || Directory.Exists(current)) &&
                (File.GetAttributes(current) & FileAttributes.ReparsePoint) != 0)
                throw new IOException("Linked paths are not supported for data maintenance.");
        }
        return full;
    }

    internal IReadOnlyList<string> Tree(string path, CancellationToken cancellation = default)
    {
        Check(path);
        var files = new List<string>();
        var pending = new Stack<(string Path, int Depth)>();
        pending.Push((path, 0));
        long bytes = 0;
        var directories = 0;
        while (pending.TryPop(out var item))
        {
            cancellation.ThrowIfCancellationRequested();
            Check(item.Path);
            if (item.Depth > 8 || ++directories > 128) throw new IOException("Save folder is too complex.");
            foreach (var file in Directory.EnumerateFiles(item.Path))
            {
                Check(file);
                files.Add(file);
                bytes = checked(bytes + new FileInfo(file).Length);
                if (files.Count > MaxTreeFiles || bytes > MaxTreeBytes) throw new IOException("Save folder exceeds maintenance limits.");
            }
            foreach (var directory in Directory.EnumerateDirectories(item.Path)) pending.Push((directory, item.Depth + 1));
        }
        return files.Order(StringComparer.OrdinalIgnoreCase).ToArray();
    }

    internal string Fingerprint(string path, CancellationToken cancellation = default)
    {
        using var digest = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        foreach (var file in Tree(path, cancellation))
        {
            cancellation.ThrowIfCancellationRequested();
            digest.AppendData(Encoding.UTF8.GetBytes(Path.GetRelativePath(path, file).Replace('\\', '/') + "\0"));
            using var input = new FileStream(Check(file), FileMode.Open, FileAccess.Read, FileShare.Read);
            digest.AppendData(SHA256.HashData(input));
        }
        return Convert.ToHexStringLower(digest.GetHashAndReset());
    }

    internal void Copy(string from, string to, string expected, CancellationToken cancellation = default)
    {
        Check(from); Check(to);
        if (File.Exists(to) || Directory.Exists(to)) throw new IOException("Backup destination already exists.");
        var files = Tree(from, cancellation);
        Directory.CreateDirectory(to);
        if (Directory.Exists(Path.Combine(from, "data"))) Directory.CreateDirectory(Check(Path.Combine(to, "data")));
        foreach (var file in files)
        {
            cancellation.ThrowIfCancellationRequested();
            var target = Check(Path.Combine(to, Path.GetRelativePath(from, file)));
            Directory.CreateDirectory(Path.GetDirectoryName(target)!);
            using (var source = new FileStream(Check(file), FileMode.Open, FileAccess.Read, FileShare.Read))
            using (var output = new FileStream(target, FileMode.CreateNew, FileAccess.Write, FileShare.None))
            {
                source.CopyTo(output);
                output.Flush(flushToDisk: true);
            }
            File.SetLastWriteTimeUtc(target, File.GetLastWriteTimeUtc(file));
        }
        if (Fingerprint(from, cancellation) != expected || Fingerprint(to, cancellation) != expected)
            throw new IOException("Save changed while copying. Originals and partial backups were kept.");
    }

    internal void Move(string from, string to)
    {
        Check(from); Check(to);
        if (File.Exists(to) || Directory.Exists(to)) throw new IOException("Data destination already exists.");
        Directory.CreateDirectory(Check(Path.GetDirectoryName(to)!));
        if (Directory.Exists(from)) Directory.Move(from, to);
        else File.Move(from, to);
    }
}
