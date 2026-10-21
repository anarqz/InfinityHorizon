// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Immutable;
using System.Globalization;
using System.Net;
using Infinity.Core.Api;

namespace Infinity.Core.Integrity;

/// <summary>One short startup observation and server decision. No inventory, attestation or periodic scan.</summary>
public sealed class OnlineContentSession : IDisposable
{
    private readonly ApiClient _api;
    private readonly Func<CancellationToken, Task<string>> _access;
    private readonly CriticalFileSnapshot? _snapshot;
    private readonly TimeProvider _clock;
    private readonly DateTimeOffset _serverAnchor;
    private readonly long _anchorStamp;
    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly CancellationTokenSource _stop = new();
    private ContentPermission? _permission;
    private long _permissionStamp;
    private TimeSpan _permissionLifetime;
    private volatile bool _revoked, _disposed;
    public ContentCapabilities Capabilities { get; }
    public ContentManifest? Manifest => Capabilities.Manifest;
    public string? SessionId { get; private set; }
    public string Reason => _revoked ? "missing_scan" : _permission?.Reason ?? "policy_unavailable";
    public InstallationState State => !_revoked && _permission is { State: "compatible" } ? InstallationState.Compatible
        : _permission?.State == "incompatible" ? InstallationState.Incompatible : InstallationState.Inconclusive;
    public string? CarDbSha256 => _snapshot?.Files.FirstOrDefault(file => file.FileId == "car_database")?.Sha256;
    public ImmutableArray<InstallationFinding> Findings
    {
        get
        {
            var result = (_snapshot?.Findings ?? []).ToBuilder();
            if (_permission is { } permission && Manifest is { } manifest)
                foreach (var item in permission.Files.Where(item => item.State != "compatible"))
                {
                    var path = manifest.Files.Single(file => file.FileId == item.FileId).Path;
                    if (result.Any(finding => finding.Path == path)) continue;
                    var observed = _snapshot?.Files.FirstOrDefault(file => file.FileId == item.FileId);
                    result.Add(new("game_content", path, item.State == "incompatible" ? "unapproved_file" : "unreadable", observed?.Sha256, observed?.ByteLength));
                }
            return result.ToImmutable();
        }
    }
    private DateTimeOffset ServerNow => _serverAnchor + _clock.GetElapsedTime(_anchorStamp);

    private OnlineContentSession(ApiClient api, Func<CancellationToken, Task<string>> access, ContentCapabilities capabilities,
        CriticalFileSnapshot? snapshot, TimeProvider clock)
    {
        _api = api; _access = access; Capabilities = capabilities; _snapshot = snapshot; _clock = clock;
        _serverAnchor = api.ServerNow(clock); _anchorStamp = clock.GetTimestamp();
    }

    public static async Task<OnlineContentSession> CheckAsync(ApiClient api, Func<CancellationToken, Task<string>> access,
        string mount, string? sessionId = null, TimeProvider? clock = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(api); ArgumentNullException.ThrowIfNull(access);
        clock ??= TimeProvider.System;
        var capabilities = await api.ContentCapabilitiesAsync(await access(cancellation).ConfigureAwait(false), "fh2", cancellation).ConfigureAwait(false);
        Validate(capabilities);
        var snapshot = capabilities.Manifest is { } manifest
            ? await CriticalFileReader.ReadAsync(mount, manifest.Files.Select(file => new CriticalFile(file.FileId, file.Path, (long)file.ByteLength)).ToArray(), cancellation).ConfigureAwait(false) : null;
        var result = new OnlineContentSession(api, access, capabilities, snapshot, clock);
        try
        {
            if (snapshot is not null) await result.SubmitAsync(sessionId ?? Guid.CreateVersion7(result.ServerNow).ToString(), cancellation).ConfigureAwait(false);
            snapshot?.ReleaseReadHandles();
            return result;
        }
        catch { result.Dispose(); throw; }
    }

    public async Task PrepareAdmissionAsync(string sessionId, CancellationToken cancellation)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _stop.Token);
        await _gate.WaitAsync(bound.Token).ConfigureAwait(false);
        try
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            if (SessionId != sessionId || !Allowed()) await SubmitAsync(sessionId, bound.Token).ConfigureAwait(false);
            if (!Allowed()) throw new ApiException(HttpStatusCode.Forbidden, State == InstallationState.Incompatible ? ErrorCodes.ContentIncompatible : ErrorCodes.ContentInconclusive, "Critical-file check refused", null);
        }
        finally { _gate.Release(); }
    }

    // Called by the existing three-second local UI lease, with no extra HTTP or hashing.
    public Task<bool> OnlineAllowedAsync(CancellationToken cancellation)
    { cancellation.ThrowIfCancellationRequested(); return Task.FromResult(Allowed()); }
    public void Revoke() => _revoked = true;

    private async Task SubmitAsync(string sessionId, CancellationToken cancellation)
    {
        if (Manifest is not { } manifest || _snapshot is null) throw new ApiException(HttpStatusCode.ServiceUnavailable, ErrorCodes.ContentPolicyUnavailable, "Critical-file reference unavailable", null);
        SessionId = sessionId;
        var unchanged = _snapshot.IsCurrent();
        var readableBefore = _snapshot.Files.All(file => file.Sha256 is not null);
        var files = _snapshot.Files.Select(file => readableBefore && !unchanged
            ? new ContentObservedFile(file.FileId, null, null) : new ContentObservedFile(file.FileId, (ulong?)file.ByteLength, file.Sha256)).ToArray();
        var request = new ContentScan(Guid.CreateVersion7(ServerNow).ToString(), sessionId, "fh2", manifest.ManifestId, files);
        _permission = null;
        DateTimeOffset? responseDate = null;
        var responseStamp = _clock.GetTimestamp();
        var response = await _api.SubmitContentScanObservedAsync(await _access(cancellation).ConfigureAwait(false), request,
            date => { responseDate = date; responseStamp = _clock.GetTimestamp(); }, cancellation).ConfigureAwait(false);
        if (response.ScanId != request.ScanId) throw new InvalidDataException("Wrong content receipt");
        // Bound against this POST's clock, independent of the capabilities GET's round-trip delay.
        var permissionStamp = _clock.GetTimestamp();
        var permissionNow = responseDate is { } date ? date + _clock.GetElapsedTime(responseStamp, permissionStamp) : ServerNow;
        ValidatePermission(response.Permission, responseDate ?? permissionNow, permissionNow);
        _permission = response.Permission;
        _permissionStamp = permissionStamp;
        _permissionLifetime = Timestamp(_permission.ExpiresAt, out var expires)
            ? TimeSpan.FromTicks(Math.Clamp((expires - permissionNow).Ticks, 0, TimeSpan.FromHours(12).Ticks)) : TimeSpan.Zero;
        if (readableBefore && !_snapshot.IsCurrent()) _revoked = true;
    }

    private bool Allowed() => !_disposed && !_revoked && _snapshot?.IsCurrent() == true && Manifest is { } manifest
        && _permission is { State: "compatible", OnlineAllowed: true, Reason: "compatible", ScanId: { } scanId } permission
        && ScanId(scanId) && permission.SessionId == SessionId && permission.Files.Count == manifest.Files.Count
        && permission.Files.All(file => file.State == "compatible") && _clock.GetElapsedTime(_permissionStamp) < _permissionLifetime;

    private void ValidatePermission(ContentPermission value, DateTimeOffset responseDate, DateTimeOffset now)
    {
        if (value is null || value.SessionId != SessionId || value.ScanId is null || !ScanId(value.ScanId)
            || !StateName(value.State) || value.Reason is not ("compatible" or "policy_unavailable" or "policy_changed" or "missing_scan" or "scan_expired" or "incompatible_files" or "inconclusive_scan")
            || value.Files is not { Count: <= CriticalFileReader.MaxFiles }) throw new InvalidDataException("Invalid content permission");
        var ids = new HashSet<string>(StringComparer.Ordinal);
        foreach (var file in value.Files)
            if (file is null || !StateName(file.State) || !ids.Add(file.FileId) || Manifest?.Files.Any(expected => expected.FileId == file.FileId) != true)
                throw new InvalidDataException("Invalid content result");
        if (value.OnlineAllowed && (value.State != "compatible" || value.Reason != "compatible" || value.Files.Count != Manifest?.Files.Count
            || value.Files.Any(file => file.State != "compatible") || !Timestamp(value.ExpiresAt, out var expires) || expires <= now
            // HTTP Date has one-second resolution; the local lease is still capped at exactly twelve hours.
            || expires >= responseDate.AddHours(12).AddSeconds(1)))
            throw new InvalidDataException("Invalid online permission");
    }

    private static void Validate(ContentCapabilities value)
    {
        if (value.SchemaVersion != 1 || value.Game != "fh2" || value.Enforcement is not ("required" or "local_test_bypass") || value.MaxFiles != 16 || value.MaxScanBytes != 8192)
            throw new InvalidDataException("Unsupported critical-file capabilities");
        if (value.Manifest is not { } manifest) return;
        if (!Uuid(manifest.ManifestId) || manifest.Game != "fh2" || manifest.GameBuild is not { Length: > 0 and <= 80 }
            || !manifest.GameBuild.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '-' or '.' or '+') || manifest.Files is not { Count: >= 2 and <= CriticalFileReader.MaxFiles })
            throw new InvalidDataException("Invalid critical-file manifest");
        var ids = new HashSet<string>(StringComparer.Ordinal); var paths = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var file in manifest.Files)
            if (file is null || !CriticalFileReader.FileId(file.FileId) || !ids.Add(file.FileId) || !CriticalFileReader.RelativePath(file.Path)
                || !paths.Add(file.Path) || file.ByteLength > (ulong)CriticalFileReader.MaxFileBytes) throw new InvalidDataException("Invalid critical-file entry");
        if (!ids.Contains("game_executable") || !ids.Contains("car_database")) throw new InvalidDataException("Missing critical-file entries");
    }
    private static bool StateName(string? value) => value is "compatible" or "incompatible" or "inconclusive";
    private static bool Uuid(string? value) => Guid.TryParseExact(value, "D", out var id) && id != Guid.Empty;
    private static bool ScanId(string? value) => Uuid(value) && Guid.Parse(value!).Version == 7 && (value![19] | 0x20) is '8' or '9' or 'a' or 'b';
    private static bool Timestamp(string? value, out DateTimeOffset time) => DateTimeOffset.TryParseExact(value, "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'",
        CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out time);
    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true; _stop.Cancel(); _snapshot?.Dispose();
    }
}
