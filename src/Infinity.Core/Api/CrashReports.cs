// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Api;

// Exact C# mirror of fi-protocol::crash and CRASH_API.md v1. No client verdict or account ids.
public sealed record CrashConsent(bool Metadata, bool Minidump);
public sealed record CrashReportIncident(string OccurredAt, string SessionId, string? WorldId);
public sealed record CrashBuild(string Game, string GameBuild, string LauncherVersion, string HookVersion);
public sealed record CrashDiagnostic(string Kind, uint? ExitCode, uint? ExceptionCode, string FaultModule, string? FaultOffset);
public sealed record CrashHint(string ParticipantXuid, string Kind, string? ContentSha256);
public sealed record CrashDumpDescriptor(uint ByteLength, string Sha256);
public sealed record CrashCreate(byte SchemaVersion, string ReportId, CrashConsent Consent, CrashReportIncident Incident,
    CrashBuild Build, CrashDiagnostic Crash, IReadOnlyList<CrashHint> Observations, CrashDumpDescriptor? Dump);
public sealed record CrashCapabilities(byte SchemaVersion, bool MetadataEnabled, bool MinidumpsEnabled, uint MaxMetadataBytes, uint MaxObservations);
public sealed record CrashAccepted(string ReportId, string State, JsonElement? DumpUpload);
public sealed record CrashView(string ReportId, string State, string Classification, string DumpStatus, string RetainedUntil);

public sealed partial class ApiClient
{
    public Task<CrashAccepted> CreateCrashReportAsync(string access, CrashCreate create, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(create);
        if (!create.Consent.Metadata || create.Consent.Minidump || create.Dump is not null
            || JsonSerializer.SerializeToUtf8Bytes(create, Json).Length > 16 * 1024
            || !Guid.TryParseExact(create.ReportId, "D", out var id) || id == Guid.Empty)
            throw new InvalidDataException("Invalid crash submission");
        return SendAsync<CrashAccepted>(HttpMethod.Post, Core, "/crash/v1/reports", create, access, create.ReportId, cancellation);
    }

    public Task<CrashCapabilities> CrashCapabilitiesAsync(string access, CancellationToken cancellation = default) =>
        SendAsync<CrashCapabilities>(HttpMethod.Get, Core, "/crash/v1/capabilities", null, access, false, cancellation);

    public async Task<CrashView> CrashReportAsync(string access, string reportId, CancellationToken cancellation = default)
    {
        if (!Guid.TryParseExact(reportId, "D", out var id) || id == Guid.Empty) throw new InvalidDataException("Invalid report id");
        var status = await SendAsync<CrashView>(HttpMethod.Get, Core, $"/crash/v1/reports/{reportId}", null, access, false, cancellation).ConfigureAwait(false);
        if (status.ReportId != reportId || status.State is not ("queued" or "analyzed")
            || status.Classification is not ("unknown" or "needs_review" or "reviewed") || status.DumpStatus != "disabled")
            throw new InvalidDataException("Invalid crash status");
        return status;
    }
}
