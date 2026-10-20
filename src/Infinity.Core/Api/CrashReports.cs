// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net.Http.Headers;
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
public sealed record CrashDumpUpload(string Path, string ExpiresAt, uint MaxBytes);
public sealed record CrashAccepted(string ReportId, string State, CrashDumpUpload? DumpUpload);
public sealed record CrashView(string ReportId, string State, string Classification, string DumpStatus, string RetainedUntil);

public sealed partial class ApiClient
{
    public Task<CrashAccepted> CreateCrashReportAsync(string access, CrashCreate create, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(create);
        if (!create.Consent.Metadata || create.Consent.Minidump != (create.Dump is not null)
            || JsonSerializer.SerializeToUtf8Bytes(create, Json).Length > 16 * 1024 || !Guid.TryParseExact(create.ReportId, "D", out _))
            throw new InvalidDataException("Invalid crash submission");
        return SendAsync<CrashAccepted>(HttpMethod.Post, Core, "/crash/v1/reports", create, access, create.ReportId, cancellation);
    }

    public Task<CrashView> CrashReportAsync(string access, string reportId, CancellationToken cancellation = default) =>
        SendAsync<CrashView>(HttpMethod.Get, Core, $"/crash/v1/reports/{Uri.EscapeDataString(reportId)}", null, access, false, cancellation);

    public async Task UploadCrashDumpAsync(string access, string reportId, string idempotencyKey, CrashDumpUpload upload,
        byte[] bytes, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(upload);
        ArgumentNullException.ThrowIfNull(bytes);
        var path = $"/crash/v1/reports/{reportId}/minidump";
        if (!Guid.TryParseExact(reportId, "D", out _) || !Guid.TryParseExact(idempotencyKey, "D", out _)
            || upload.Path != path || bytes.Length > upload.MaxBytes || upload.MaxBytes > Diagnostics.MinidumpInspector.MaxUploadDumpBytes
            || !DateTimeOffset.TryParse(upload.ExpiresAt, System.Globalization.CultureInfo.InvariantCulture, out var expires)
            || expires <= ServerNow(TimeProvider.System) || !Diagnostics.MinidumpInspector.IsMinimizedUpload(bytes))
            throw new InvalidDataException("Unsupported crash attachment authorization");
        using var request = new HttpRequestMessage(HttpMethod.Put, new Uri(Core, path));
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", access);
        request.Headers.TryAddWithoutValidation("X-Fi-Client", _client);
        request.Headers.TryAddWithoutValidation("Idempotency-Key", idempotencyKey);
        request.Content = new ByteArrayContent(bytes);
        request.Content.Headers.ContentType = new MediaTypeHeaderValue("application/octet-stream");
        using var response = await _http.SendAsync(request, cancellation).ConfigureAwait(false);
        await Read<JsonElement?>(response, cancellation).ConfigureAwait(false);
    }
}
