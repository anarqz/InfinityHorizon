// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;

namespace Infinity.Core.Diagnostics;

/// <summary>
/// One local crash as it goes into the "Get logs" zip (OD-241): a projection, never a serialization of the private local context
/// (no account, session, world or player ids, names, paths or free text).
/// </summary>
public static class CrashSupportReport
{
    public const string InstantFormat = "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'";
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web) { WriteIndented = true };

    public static string Create(CrashIncident incident)
    {
        ArgumentNullException.ThrowIfNull(incident);
        static string? Id(string? id) => Guid.TryParseExact(id, "D", out var parsed) && parsed != Guid.Empty ? id : null;
        return JsonSerializer.Serialize(new
        {
            format = "infinity-crash-support-v1",
            localIncidentId = Id(incident.Id),
            occurredAt = incident.OccurredAt.UtcDateTime.ToString(InstantFormat, CultureInfo.InvariantCulture),
            build = Build(incident),
            crash = Describe(incident),
            localDumpAvailable = incident.Dump is not null,
            worldContextAvailable = incident.Context.World is not null,
            observedParticipantCount = Math.Min(incident.Context.World?.Participants.Count ?? 0, 12),
            attribution = "unknown",
        }, Json);
    }

    private static object Build(CrashIncident incident) => new
    {
        game = incident.Game == "fh2" ? "fh2" : "unknown",
        gameBuild = Version(incident.AzimuthVersion),
        launcherVersion = Version(incident.LauncherVersion),
        hookVersion = Version(incident.HookVersion),
    };

    private static object Describe(CrashIncident incident)
    {
        var module = incident.Dump?.FaultModule?.ToLowerInvariant() switch
        {
            "forza_durango_release_final.exe" => "game",
            // OD-125: winmm.dll is ExtraOptions.Infinity (the hook and ExtraOptions in one module)
            "azimuth-tls.dll" or "infinity.dll" or "winmm.dll" => "hook",
            "infinity-overlay.dll" => "overlay",
            _ => "unknown",
        };
        return new
        {
            kind = incident.Dump?.ExceptionCode is null ? "abnormal_exit" : "exception",
            exitCode = incident.ExitCode,
            exceptionCode = incident.Dump?.ExceptionCode,
            faultModule = module,
            faultOffset = module != "unknown" ? incident.Dump?.FaultModuleRva?.ToString("x16", CultureInfo.InvariantCulture) : null,
        };
    }

    private static string Version(string? value) => value is { Length: >= 1 and <= 32 }
        && value.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '-' or '_' or '+') ? value : "unknown";
}
