// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace Infinity.Core.Integrity;

/// <summary>Resolves the user's selected content boundary, including Wine's Unix/DOS namespace.</summary>
public static class ContentPath
{
    public static string NormalizeSelection(string path)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        if (path.Contains('\0')) throw new InvalidDataException("Invalid content path");
        if (OperatingSystem.IsWindows() && path.StartsWith('/') && Process.WineHost.Detected)
        {
            IntPtr translated;
            try { translated = Native.wine_get_dos_file_name(Encoding.UTF8.GetBytes(path + "\0")); }
            catch (EntryPointNotFoundException) { throw new InvalidDataException("Wine path conversion is unavailable"); }
            if (translated == IntPtr.Zero) throw new InvalidDataException("Wine could not resolve the selected folder");
            try { path = Marshal.PtrToStringUni(translated) ?? throw new InvalidDataException("Invalid Wine content path"); }
            finally { Native.HeapFree(Native.GetProcessHeap(), 0, translated); }
        }
        if (!Path.IsPathFullyQualified(path)) throw new InvalidDataException("An absolute content path is required");
        return LocalPath(path);
    }

    public static string ResolveDirectory(string directory)
    {
        var path = NormalizeSelection(directory);
        if (!Directory.Exists(path)) throw new DirectoryNotFoundException("Content folder is missing");
        var selected = new DirectoryInfo(path);
        if ((selected.Attributes & FileAttributes.ReparsePoint) != 0)
        {
            FileSystemInfo? target;
            try
            {
                target = selected.ResolveLinkTarget(returnFinalTarget: true);
            }
            catch (IOException) when (Process.WineHost.Detected)
            {
                // Wine may expose a Unix directory link without a Windows reparse buffer.
                // Open the explicitly selected root normally; descendants keep their own guard.
                target = null;
            }
            if (target is DirectoryInfo) path = NormalizeSelection(target.FullName);
            else if (!Process.WineHost.Detected) throw new InvalidDataException("Selected content folder could not be resolved");
        }
        if (!OperatingSystem.IsWindows()) return path;
        using var handle = Native.CreateFileW(path, 0, 3, IntPtr.Zero, 3, 0x02000000, IntPtr.Zero);
        if (handle.IsInvalid) throw new IOException("Content folder could not be opened");
        return FinalPath(handle);
    }

    internal static string FinalPath(SafeFileHandle handle)
    {
        var buffer = new char[32768];
        var length = Native.GetFinalPathNameByHandleW(handle, buffer, (uint)buffer.Length, 0);
        if (length == 0 || length >= buffer.Length) throw new IOException("Content handle path is unavailable");
        return LocalPath(new string(buffer, 0, (int)length));
    }

    private static string LocalPath(string path)
    {
        // Wine's local Unix namespace also works when the prefix exposes no DOS mapping.
        if (OperatingSystem.IsWindows() && Process.WineHost.Detected && path.StartsWith(@"\\?\unix\", StringComparison.Ordinal))
            return Path.TrimEndingDirectorySeparator(Path.GetFullPath(path));
        if (path.StartsWith(@"\\?\", StringComparison.Ordinal)) path = path[4..];
        // No network, volume or arbitrary device namespace is accepted.
        if (path.StartsWith(@"\\", StringComparison.Ordinal) || path.StartsWith("UNC\\", StringComparison.OrdinalIgnoreCase)
            || OperatingSystem.IsWindows() && (path.Length < 3 || !char.IsAsciiLetter(path[0]) || path[1] != ':' || path[2] is not ('\\' or '/')))
            throw new InvalidDataException("A local content folder is required");
        return Path.TrimEndingDirectorySeparator(Path.GetFullPath(path));
    }

    /// <summary>Redirects outside the explicitly selected boundary are irrelevant; redirects inside it remain forbidden.</summary>
    internal static bool SafeWithin(string root, string path)
    {
        var comparison = OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal;
        root = Path.TrimEndingDirectorySeparator(Path.GetFullPath(root));
        path = Path.GetFullPath(path);
        if (!path.Equals(root, comparison) && !path.StartsWith(Path.EndsInDirectorySeparator(root) ? root : root + Path.DirectorySeparatorChar, comparison)) return false;
        for (FileSystemInfo? current = File.Exists(path) ? new FileInfo(path) : new DirectoryInfo(path); current is not null;
            current = current is FileInfo file ? file.Directory : ((DirectoryInfo)current).Parent)
        {
            if (Path.TrimEndingDirectorySeparator(current.FullName).Equals(root, comparison))
                return current.Exists && ((current.Attributes & FileAttributes.ReparsePoint) == 0 || Process.WineHost.Detected);
            if (current.Exists && (current.Attributes & FileAttributes.ReparsePoint) != 0) return false;
        }
        return false;
    }

    private static class Native
    {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        internal static extern SafeFileHandle CreateFileW(string path, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        internal static extern uint GetFinalPathNameByHandleW(SafeFileHandle handle, [Out] char[] path, uint length, uint flags);
        [DllImport("kernel32.dll", CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        internal static extern IntPtr wine_get_dos_file_name([In] byte[] path);
        [DllImport("kernel32.dll", ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        internal static extern IntPtr GetProcessHeap();
        [DllImport("kernel32.dll", ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool HeapFree(IntPtr heap, uint flags, IntPtr memory);
    }
}
