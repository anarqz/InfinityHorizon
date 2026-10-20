// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using Infinity.Core.Api;
using Infinity.Core.Auth;

namespace Infinity.Core.Diagnostics;

/// <summary>Immutable retry envelope. A repeat Send preserves consent, hashes and idempotency keys.</summary>
public sealed record CrashSubmission(CrashCreate Request, string DumpKey)
{
    public static CrashSubmission Prepare(CrashIncident incident, bool metadataConsent, byte[]? consentedDump)
    {
        ArgumentNullException.ThrowIfNull(incident);
        if (!metadataConsent) throw new InvalidOperationException("Metadata consent is required");
        if (consentedDump is not null && !MinidumpInspector.IsMinimizedUpload(consentedDump)) throw new InvalidDataException("Unsupported dump");
        var module = incident.Dump?.FaultModule?.ToLowerInvariant() switch
        {
            "forza_durango_release_final.exe" => "game",
            "azimuth-tls.dll" or "infinity.dll" => "hook",
            "infinity-overlay.dll" => "overlay",
            _ => "unknown",
        };
        var descriptor = consentedDump is null ? null : new CrashDumpDescriptor((uint)consentedDump.Length, Convert.ToHexStringLower(SHA256.HashData(consentedDump)));
        // Server reconstructs the roster. Local rejections are not relabeled as causal packet/content hints.
        var request = new CrashCreate(1, incident.Id, new(true, descriptor is not null),
            new(incident.OccurredAt.UtcDateTime.ToString("O", CultureInfo.InvariantCulture), incident.SessionId, incident.Context.World?.WorldId),
            new(incident.Game, Version(incident.AzimuthVersion), Version(incident.LauncherVersion), Version(incident.HookVersion)),
            new(incident.Dump?.ExceptionCode is null ? "abnormal_exit" : "exception", incident.ExitCode, incident.Dump?.ExceptionCode,
                module, module != "unknown" ? incident.Dump?.FaultModuleRva?.ToString("x16", CultureInfo.InvariantCulture) : null), [], descriptor);
        return new(request, Guid.CreateVersion7().ToString());
    }

    private static string Version(string? value) => value is { Length: >= 1 and <= 32 }
        && value.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '-' or '_' or '+') ? value : "unknown";

    public async Task<bool> SendAsync(ApiClient api, TokenManager tokens, byte[]? dump, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(api); ArgumentNullException.ThrowIfNull(tokens);
        if (!Request.Consent.Metadata) throw new InvalidOperationException("Metadata consent is required");
        if (Request.Dump is { } expected && (dump is null || dump.Length != expected.ByteLength
            || Convert.ToHexStringLower(SHA256.HashData(dump)) != expected.Sha256)) throw new InvalidDataException("Attachment changed");
        var accepted = await tokens.WithAccessAsync(access => api.CreateCrashReportAsync(access, Request, cancellation), cancellation).ConfigureAwait(false);
        if (accepted.ReportId != Request.ReportId || accepted.State != "queued") throw new InvalidDataException("Invalid crash receipt");
        if (Request.Dump is null) return true;
        if (accepted.DumpUpload is null) throw new InvalidDataException("Missing attachment authorization");
        if (DateTimeOffset.TryParse(accepted.DumpUpload.ExpiresAt, CultureInfo.InvariantCulture, out var expires)
            && expires <= api.ServerNow(TimeProvider.System))
        {
            return await AttachmentResultAsync(api, tokens, cancellation).ConfigureAwait(false)
                ?? throw new InvalidDataException("Expired attachment authorization");
        }
        try
        {
            await tokens.WithAccessAsync(async access =>
            {
                await api.UploadCrashDumpAsync(access, Request.ReportId, DumpKey, accepted.DumpUpload, dump!, cancellation).ConfigureAwait(false);
                return true;
            }, cancellation).ConfigureAwait(false);
        }
        catch (HttpRequestException) when (!cancellation.IsCancellationRequested)
        {
            // A lost response may follow a committed upload. Consult the owner receipt before retrying.
            var result = await AttachmentResultAsync(api, tokens, cancellation).ConfigureAwait(false);
            if (result is { } completed) return completed;
            throw;
        }
        catch (ApiException e) when (e.Code == "CRASH.EXPIRED")
        {
            return await AttachmentResultAsync(api, tokens, cancellation).ConfigureAwait(false)
                ?? throw new InvalidDataException("Expired attachment authorization");
        }
        return true;
    }

    private async Task<bool?> AttachmentResultAsync(ApiClient api, TokenManager tokens, CancellationToken cancellation)
    {
        var status = await tokens.WithAccessAsync(access => api.CrashReportAsync(access, Request.ReportId, cancellation), cancellation).ConfigureAwait(false);
        if (status.ReportId != Request.ReportId || status.State is not ("queued" or "analyzed")
            || status.Classification is not ("unknown" or "suspected_contributor" or "confirmed_evidence")
            || status.DumpStatus is not ("stored" or "expired" or "awaiting"))
            throw new InvalidDataException("Invalid crash status");
        return status.DumpStatus switch { "stored" => true, "expired" => false, _ => null };
    }
}
