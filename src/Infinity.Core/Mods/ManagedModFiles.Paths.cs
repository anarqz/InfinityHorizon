// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace Infinity.Core.Mods;

/// <summary>
/// Path rules and the only file primitives managed mod code may use. Every removal of a managed,
/// game or state file goes through <see cref="RemoveFile"/>: under Wine a file symlink is invisible
/// to Win32 (no reparse attribute, no link target) and <c>File.Delete</c> on it deletes the target
/// outside the root (SAURON, Proton 11, 2026-10-23); replacing the path with a fresh empty file first
/// removes the link itself.
/// </summary>
public sealed partial class ManagedModFiles
{
    private string Target(string root, string relative)
    {
        if (relative.EndsWith(".slt", StringComparison.OrdinalIgnoreCase) || relative.EndsWith(".db", StringComparison.OrdinalIgnoreCase)
            || relative.EndsWith(".sqlite", StringComparison.OrdinalIgnoreCase) || relative.EndsWith(".sqlite3", StringComparison.OrdinalIgnoreCase))
            throw new ModException("databases require session coordination", ModErrors.Unsupported);
        var target = Safe(root switch { "azimuth" => _azimuth, "game" => _game ?? throw new ModException("game folder not selected", ModErrors.Unsupported), _ => throw new ModException("invalid root") }, relative);
        RejectProtected(target);
        return target;
    }

    private void RejectProtected(string target)
    {
        var canonical = Canonical(target);
        foreach (var protectedPath in _protected)
            if (Within(protectedPath, target) || Within(protectedPath, canonical))
                throw new ModException("protected launcher path", ModErrors.Protected);
    }

    /// <summary>Plain and canonical forms of the launcher's own paths, of Azimuth's host and data, and of the state folder.</summary>
    private static List<string> ProtectedPaths(string azimuth, string? game, string state, IEnumerable<string> launcherRoots)
    {
        var paths = new List<string> { Path.Combine(azimuth, "data"), Path.Combine(azimuth, "azimuth.exe"), Path.Combine(azimuth, "infinity.dll"), state };
        foreach (var root in launcherRoots)
        {
            var full = Path.TrimEndingDirectorySeparator(Path.GetFullPath(root));
            var selected = new[] { azimuth, game }.OfType<string>().ToArray();
            if (selected.Any(r => string.Equals(Path.TrimEndingDirectorySeparator(r), full, StringComparison.OrdinalIgnoreCase)))
            {
                // The launcher extracted into the Azimuth (or game) folder itself: its own release layout stays protected.
                paths.AddRange(Infinity.Core.Update.UpdatePackageVerifier.PayloadRoots.Select(name => Path.Combine(full, name)));
                if (Environment.ProcessPath is { } process && string.Equals(Path.GetDirectoryName(process), full, StringComparison.OrdinalIgnoreCase)) paths.Add(process);
                continue;
            }
            // A launcher folder that holds the selected folders protects only its own files outside them (checked by the other roots' rules).
            if (selected.Any(r => Within(full, r))) continue;
            paths.Add(full);
        }
        return paths.SelectMany(p => new[] { p, Canonical(p) }).Distinct(StringComparer.OrdinalIgnoreCase).ToList();
    }

    private string Safe(string root, string relative)
    {
        var target = Resolve(root, relative);
        RejectLinks(target);
        return target;
    }

    private static string Resolve(string root, string relative)
    {
        ModManifest.ValidatePath(relative);
        var target = Path.GetFullPath(Path.Combine(root, relative));
        if (!target.StartsWith(root.TrimEnd('\\', '/') + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase)) throw new ModException("path escaped root");
        return target;
    }

    /// <summary>
    /// Refuses a reparse point between <paramref name="path"/> and its managed root. The selected
    /// Azimuth and game folders themselves may be links (a junctioned library, H1); the state and
    /// package-cache roots may be one only under Wine, which reports every mount point as a link.
    /// </summary>
    private void RejectLinks(string path)
    {
        var full = Path.GetFullPath(path);
        var boundary = new[] { _azimuth, _game, _state, _packageCache }.Where(root => root is not null && Within(root, full))
            .MaxBy(root => root!.Length) ?? throw new ModException("path escaped root");
        var selected = string.Equals(boundary, _azimuth, StringComparison.OrdinalIgnoreCase) || string.Equals(boundary, _game, StringComparison.OrdinalIgnoreCase);
        RejectLinks(full, boundary, selected || Process.WineHost.Detected);
    }

    private static void RejectLinks(string path, string boundary, bool linkedBoundary)
    {
        boundary = Path.TrimEndingDirectorySeparator(Path.GetFullPath(boundary));
        for (var current = Path.GetFullPath(path); current is not null; current = Path.GetDirectoryName(current))
        {
            var root = string.Equals(Path.TrimEndingDirectorySeparator(current), boundary, StringComparison.OrdinalIgnoreCase);
            if (!(root && linkedBoundary) && IsLink(current))
                throw new ModException("linked paths are not supported for managed mods", ModErrors.Linked);
            if (root) return;
        }
        throw new ModException("path escaped root");
    }

    private static bool IsLink(string path)
    {
        if (Path.GetDirectoryName(path) is null) return false; // a drive or filesystem root
        if (new FileInfo(path).LinkTarget is not null) return true;
        return (File.Exists(path) || Directory.Exists(path)) && (File.GetAttributes(path) & FileAttributes.ReparsePoint) != 0;
    }

    internal static bool Within(string root, string path)
    {
        root = Path.TrimEndingDirectorySeparator(Path.GetFullPath(root));
        path = Path.GetFullPath(path);
        return string.Equals(Path.TrimEndingDirectorySeparator(path), root, StringComparison.OrdinalIgnoreCase)
            || path.StartsWith(Path.EndsInDirectorySeparator(root) ? root : root + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>
    /// The final long form of <paramref name="path"/>: on Windows the deepest existing ancestor is
    /// opened and named by <c>GetFinalPathNameByHandle</c> (8.3 aliases such as <c>DATA~1</c>
    /// expanded, junctions resolved); elsewhere each link is resolved. Under Wine a file symlink keeps
    /// its own name, so this cannot see through it; <see cref="RemoveFile"/> covers that case for writes.
    /// </summary>
    internal static string Canonical(string path)
    {
        var full = Path.TrimEndingDirectorySeparator(Path.GetFullPath(path));
        var existing = full; var rest = new Stack<string>();
        while (!File.Exists(existing) && !Directory.Exists(existing))
        {
            var parent = Path.GetDirectoryName(existing);
            if (parent is null) return full;
            rest.Push(Path.GetFileName(existing)); existing = parent;
        }
        var resolved = OperatingSystem.IsWindows() ? FinalPath(existing) : ResolveLinks(existing);
        if (resolved is null) return full;
        while (rest.Count > 0) resolved = Path.Combine(resolved, rest.Pop());
        return Path.TrimEndingDirectorySeparator(resolved);
    }

    private static string ResolveLinks(string full)
    {
        if (Path.GetDirectoryName(full) is not { } parent) return full;
        var current = Path.Combine(ResolveLinks(parent), Path.GetFileName(full));
        try { return new FileInfo(current).ResolveLinkTarget(returnFinalTarget: true) is { } target ? ResolveLinks(Path.GetFullPath(target.FullName)) : current; }
        catch (IOException) { return current; }
    }

    private static string? FinalPath(string existing)
    {
        if (!OperatingSystem.IsWindows()) return null;
        using var handle = CreateFileW(existing, 0, 7, IntPtr.Zero, 3, 0x02000000, IntPtr.Zero);
        if (handle.IsInvalid) return null;
        var buffer = new char[32768];
        var length = GetFinalPathNameByHandleW(handle, buffer, (uint)buffer.Length, 0);
        if (length == 0 || length >= buffer.Length) return null;
        var text = new string(buffer, 0, (int)length);
        if (text.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase)) return @"\\" + text[8..];
        return text.StartsWith(@"\\?\", StringComparison.Ordinal) && text.Length > 6 && text[5] == ':' ? text[4..] : text;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
    [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
    private static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
    [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
    private static extern uint GetFinalPathNameByHandleW(SafeFileHandle file, [Out] char[] path, uint length, uint flags);

    /// <summary>
    /// The only deletion of a file in a managed, game or state folder: a fresh empty sibling replaces
    /// the path (so a file link the platform does not report is replaced, never followed), then the
    /// plain file is deleted. A missing path is nothing to do.
    /// </summary>
    internal static void RemoveFile(string path)
    {
        if (!File.Exists(path) && new FileInfo(path).LinkTarget is null) return;
        var staging = Path.Combine(Path.GetDirectoryName(path)!, "." + Guid.NewGuid().ToString("N") + ".fi-delete.tmp");
        using (new FileStream(staging, FileMode.CreateNew, FileAccess.Write, FileShare.None)) { }
        try { File.Move(staging, path, overwrite: true); }
        catch
        {
            File.Delete(staging); // our own new, empty, plain file
            throw;
        }
        RemoveFileHook?.Invoke(path);
        File.Delete(path);
    }

    /// <summary>Tests only: observes <see cref="RemoveFile"/> after the replacement, before the final delete.</summary>
    internal static readonly AsyncLocal<Action<string>?> RemoveFileObserver = new();
    private static Action<string>? RemoveFileHook => RemoveFileObserver.Value;

    private static string? Current(string path) => File.Exists(path) ? ModManifest.Hash(File.ReadAllBytes(path)) : null;

    private string Store(byte[] bytes)
    {
        var digest = ModManifest.Hash(bytes);
        var path = Safe(BackupRoot, "objects/" + digest);
        if (!File.Exists(path)) Atomic(path, bytes);
        else if (Current(path) != digest) throw new ModException("backup object damaged", ModErrors.Digest);
        return digest;
    }

    private byte[] ObjectBytes(string digest)
    {
        if (digest.Length != 64 || digest.Any(c => !char.IsAsciiHexDigit(c))) throw new ModException("invalid backup reference", ModErrors.Digest);
        var path = Safe(BackupRoot, "objects/" + digest);
        if (!File.Exists(path)) throw new ModException("backup object missing", ModErrors.Digest);
        var bytes = File.ReadAllBytes(path);
        if (ModManifest.Hash(bytes) != digest) throw new ModException("backup object damaged", ModErrors.Digest);
        return bytes;
    }

    private void Put(string path, string? digest)
    {
        if (digest is null) RemoveFile(path);
        else Atomic(path, ObjectBytes(digest));
    }

    private void Atomic(string path, byte[] bytes)
    {
        RejectLinks(path);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var staging = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        try
        {
            FaultPoint?.Invoke("write:" + Path.GetFileName(path));
            using (var stream = new FileStream(staging, FileMode.CreateNew, FileAccess.Write, FileShare.None))
            { stream.Write(bytes); stream.Flush(true); }
            File.Move(staging, path, overwrite: true);
        }
        finally { if (File.Exists(staging)) RemoveFile(staging); }
    }

    /// <summary>True for a crash leftover of <see cref="Atomic"/> or <see cref="RemoveFile"/>: <c>name.&lt;32 hex&gt;.tmp</c> or <c>.&lt;32 hex&gt;.fi-delete.tmp</c>.</summary>
    internal static bool IsStagingLeftover(string name)
    {
        static bool Hex(string text) => text.Length == 32 && text.All(char.IsAsciiHexDigitLower);
        if (name.StartsWith('.') && name.EndsWith(".fi-delete.tmp", StringComparison.Ordinal)) return Hex(name[1..^14]);
        if (!name.EndsWith(".tmp", StringComparison.Ordinal)) return false;
        var stem = name[..^4]; var dot = stem.LastIndexOf('.');
        return dot > 0 && Hex(stem[(dot + 1)..]);
    }
}
