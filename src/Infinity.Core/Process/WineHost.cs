// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Runtime.InteropServices;

namespace Infinity.Core.Process;

/// <summary>Wine detection and manifest-selected native DLL overrides.</summary>
public static class WineHost
{
    public static bool Detected => How is not null;

    /// <summary>How Wine was recognised (for the log), or null on Windows.</summary>
    public static string? How { get; } = Detect();

    /// <summary>
    /// The game's <c>WINEDLLOVERRIDES</c> with <c>native-first</c> added to <paramref name="inherited"/>
    /// (Proton's own entries kept); null when nothing needs to change.
    /// </summary>
    public static string? WithNativeDll(string? inherited, string module)
    {
        ValidateModule(module);
        var entries = (inherited ?? string.Empty).Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        if (entries.Any(e => e.StartsWith(module + "=", StringComparison.OrdinalIgnoreCase) || e.Split('=')[0].Split(',').Any(n => n.Equals(module, StringComparison.OrdinalIgnoreCase))))
        {
            return null; // the player (or Proton) already decided for the module
        }
        return string.Join(';', entries.Append(module + "=n,b"));
    }

    /// <summary>The per-application override key Wine reads when azimuth.exe loads (HKCU, the prefix's registry).</summary>
    public const string AppDefaultsKey = @"Software\Wine\AppDefaults\azimuth.exe\DllOverrides";

    /// <summary>What the launcher writes for the module: the selected proxy DLL first, Wine's own if it is gone.</summary>
    public const string NativeFirst = "native,builtin";

    /// <summary>
    /// SAURON (2026-10-19): a CreateProcess environment block does not reach Wine's load-order code
    /// in the child, so WINEDLLOVERRIDES alone never loads the selected proxy DLL. The per-app
    /// registry override does, and only for azimuth.exe: set when <paramref name="want"/> (the proxy DLL
    /// is next to azimuth.exe), removed when not, if it is the launcher's own value. Returns what
    /// happened, for the log.
    /// </summary>
    public static string EnsureNativeDllOverride(string module, bool want)
    {
        ValidateModule(module);
        if (!OperatingSystem.IsWindows())
        {
            return "not windows";
        }
        try
        {
            using var key = Microsoft.Win32.Registry.CurrentUser.CreateSubKey(AppDefaultsKey, writable: true);
            var now = key.GetValue(module) as string;
            if (want)
            {
                if (string.Equals(now, NativeFirst, StringComparison.OrdinalIgnoreCase))
                {
                    return "already";
                }
                key.SetValue(module, NativeFirst, Microsoft.Win32.RegistryValueKind.String);
                return now is null ? "set" : $"set (was {now})";
            }
            if (string.Equals(now, NativeFirst, StringComparison.OrdinalIgnoreCase))
            {
                key.DeleteValue(module, throwOnMissingValue: false);
                return "removed";
            }
            return now is null ? "none" : $"left ({now}, not ours)";
        }
        catch (Exception e) when (e is UnauthorizedAccessException or System.Security.SecurityException or IOException)
        {
            return "failed: " + e.Message;
        }
    }

    private static void ValidateModule(string module)
    {
        if (module.Length is < 1 or > 64 || module.Any(c => !char.IsAsciiLetterOrDigit(c) && c != '_' && c != '-')) throw new ArgumentException("invalid native module", nameof(module));
    }
    private static string? Detect()
    {
        if (!OperatingSystem.IsWindows())
        {
            return null;
        }
        // Proton can hide wine_get_version (HideWineExports): Wine's own environment and registry too
        var ntdll = GetModuleHandleW("ntdll.dll");
        if (ntdll != IntPtr.Zero && GetProcAddress(ntdll, "wine_get_version") != IntPtr.Zero)
        {
            return "ntdll wine_get_version";
        }
        foreach (var name in new[] { "WINEPREFIX", "WINELOADER", "WINEDLLOVERRIDES", "STEAM_COMPAT_DATA_PATH" })
        {
            if (!string.IsNullOrEmpty(Environment.GetEnvironmentVariable(name)))
            {
                return "environment " + name;
            }
        }
        using var wine = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(@"Software\Wine");
        return wine is not null ? @"registry HKCU\Software\Wine" : null;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
    private static extern IntPtr GetModuleHandleW(string name);

    [DllImport("kernel32.dll", CharSet = CharSet.Ansi, ExactSpelling = true, BestFitMapping = false, ThrowOnUnmappableChar = true)]
    [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
    private static extern IntPtr GetProcAddress(IntPtr module, string name);
}
