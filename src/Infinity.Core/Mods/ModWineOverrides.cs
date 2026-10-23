// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using Infinity.Core.Process;

namespace Infinity.Core.Mods;

/// <summary>The per-application Wine registry override (HKCU AppDefaults\azimuth.exe\DllOverrides) for one module.</summary>
internal interface IModWineOverrides
{
    bool Detected { get; }
    /// <summary>Sets (<paramref name="want"/>) or removes the launcher's native-first value; returns what happened, for the log.</summary>
    string Set(string module, bool want);
}

internal sealed class SystemWineOverrides : IModWineOverrides
{
    public static SystemWineOverrides Instance { get; } = new();
    public bool Detected => WineHost.Detected;
    public string Set(string module, bool want) => WineHost.EnsureNativeDllOverride(module, want);
}

public sealed partial class ManagedModFiles
{
    private string WineRecordPath => Path.Combine(_bucket, "wine-overrides.json");

    /// <summary>The modules enabled mods of this Azimuth folder load native-first under Wine (the launch environment's WINEDLLOVERRIDES).</summary>
    public IReadOnlyList<string> NativeDllOverrides() => InstalledManifests().Where(m => Receipt(m) is { Enabled: true })
        .SelectMany(m => m.WineDllOverrides.Select(w => w.Module)).Distinct(StringComparer.OrdinalIgnoreCase).ToArray();

    /// <summary>
    /// L12: the AppDefaults values the launcher wrote are recorded; a module no enabled mod declares
    /// any more is removed (only when the value is still the launcher's own), a declared one is set.
    /// Registry failures are logged by the caller's next launch, never thrown.
    /// </summary>
    public IReadOnlyList<string> ReconcileWineOverrides()
    {
        if (!WineOverrides.Detected) return [];
        var log = new List<string>();
        try
        {
            var wanted = InstalledManifests().Where(m => Receipt(m) is { Enabled: true })
                .SelectMany(m => m.WineDllOverrides.Where(w => w.AppDefaults).Select(w => w.Module)).ToHashSet(StringComparer.OrdinalIgnoreCase);
            var owned = ReadWineRecord();
            foreach (var module in wanted) { log.Add(module + ": " + WineOverrides.Set(module, true)); owned.Add(module); }
            foreach (var module in owned.Where(m => !wanted.Contains(m)).ToArray()) { log.Add(module + ": " + WineOverrides.Set(module, false)); owned.Remove(module); }
            WriteWineRecord(owned);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException or ModException) { log.Add("failed: " + error.GetType().Name); }
        return log;
    }

    private void AdoptWineOverrides(IEnumerable<ModWineOverride> overrides)
    {
        if (!WineOverrides.Detected) return;
        var owned = ReadWineRecord();
        foreach (var item in overrides.Where(w => w.AppDefaults)) owned.Add(item.Module);
        WriteWineRecord(owned);
    }

    private HashSet<string> ReadWineRecord()
    {
        if (!File.Exists(WineRecordPath)) return new(StringComparer.OrdinalIgnoreCase);
        RejectLinks(WineRecordPath);
        var modules = JsonSerializer.Deserialize<string[]>(File.ReadAllBytes(WineRecordPath)) ?? [];
        return modules.Where(m => m.Length is > 0 and <= 64 && m.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '-')).ToHashSet(StringComparer.OrdinalIgnoreCase);
    }

    private void WriteWineRecord(HashSet<string> modules) => Atomic(WineRecordPath, JsonSerializer.SerializeToUtf8Bytes(modules.Order(StringComparer.OrdinalIgnoreCase).ToArray()));
}
