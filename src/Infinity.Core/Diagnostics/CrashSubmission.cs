// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Auth;

namespace Infinity.Core.Diagnostics;

/// <summary>Immutable metadata action. An older attachment action is never silently downgraded.</summary>
public sealed record CrashSubmission(CrashCreate Request)
{
    public const string InstantFormat = "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'";

    public bool IsCurrentMetadata => Request is
    {
        SchemaVersion: 1, Consent: { Metadata: true, Minidump: false }, Dump: null,
        Incident: { } incident, Build: not null, Crash: not null, Observations: { Count: <= 16 }
    }
        && Guid.TryParseExact(Request.ReportId, "D", out var id) && id != Guid.Empty
        && DateTimeOffset.TryParseExact(incident.OccurredAt, InstantFormat, CultureInfo.InvariantCulture,
            DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out _);

    public static CrashSubmission Prepare(CrashIncident incident, bool metadataConsent, string? reportId = null)
    {
        ArgumentNullException.ThrowIfNull(incident);
        if (!metadataConsent) throw new InvalidOperationException("Metadata consent is required");
        var request = new CrashCreate(1, reportId ?? incident.Id, new(true, false),
            new(incident.OccurredAt.UtcDateTime.ToString(InstantFormat, CultureInfo.InvariantCulture), incident.SessionId, incident.Context.World?.WorldId),
            Build(incident), Describe(incident), [], null);
        return new(request);
    }

    internal static CrashBuild Build(CrashIncident incident) => new(incident.Game == "fh2" ? "fh2" : "unknown",
        Version(incident.AzimuthVersion), Version(incident.LauncherVersion), Version(incident.HookVersion));

    internal static CrashDiagnostic Describe(CrashIncident incident)
    {
        var module = incident.Dump?.FaultModule?.ToLowerInvariant() switch
        {
            "forza_durango_release_final.exe" => "game",
            "azimuth-tls.dll" or "infinity.dll" => "hook",
            "infinity-overlay.dll" => "overlay",
            _ => "unknown",
        };
        return new(incident.Dump?.ExceptionCode is null ? "abnormal_exit" : "exception", incident.ExitCode, incident.Dump?.ExceptionCode,
            module, module != "unknown" ? incident.Dump?.FaultModuleRva?.ToString("x16", CultureInfo.InvariantCulture) : null);
    }

    private static string Version(string? value) => value is { Length: >= 1 and <= 32 }
        && value.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '-' or '_' or '+') ? value : "unknown";

    public async Task SendAsync(ApiClient api, TokenManager tokens, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(api); ArgumentNullException.ThrowIfNull(tokens);
        if (!IsCurrentMetadata) throw new InvalidDataException("A separate metadata-only action is required");
        var capabilities = await tokens.WithAccessAsync(access => api.CrashCapabilitiesAsync(access, cancellation), cancellation).ConfigureAwait(false);
        if (capabilities.SchemaVersion != 1 || !capabilities.MetadataEnabled || capabilities.MinidumpsEnabled
            || capabilities.MaxMetadataBytes is 0 or > 16384 || capabilities.MaxObservations > 16
            || JsonSerializer.SerializeToUtf8Bytes(Request, ApiClient.Json).Length > capabilities.MaxMetadataBytes
            || Request.Observations.Count > capabilities.MaxObservations)
            throw new InvalidDataException("Unsupported crash metadata capabilities");
        var accepted = await tokens.WithAccessAsync(access => api.CreateCrashReportAsync(access, Request, cancellation), cancellation).ConfigureAwait(false);
        if (accepted.ReportId != Request.ReportId || accepted.State != "queued" || accepted.DumpUpload is not null)
            throw new InvalidDataException("Invalid metadata receipt");
    }
}
