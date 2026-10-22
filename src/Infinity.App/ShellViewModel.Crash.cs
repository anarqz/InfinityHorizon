// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.Diagnostics;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    private CrashIncident? _crash;
    private CrashSubmission? _crashSubmission;
    private bool _crashOpen, _crashConsent, _crashManualDumpConsent, _crashSending, _crashNewMetadataAction, _crashSent;
    private string? _crashStatus;
    private CancellationTokenSource? _crashSendStop;
    private ManualDumpExport? _crashDumpExport;
    private CrashIncidentStore CrashStore => new(_stateDirectory);

    public bool HasCrashReport => _crash is not null && SignedIn;
    public bool ShowCrashReport => HasCrashReport && _crashOpen && !_onMyData && !_onHelp && !_onMods && !_onSetup && !_onAccount && !_onBugReport && !ShowPickSave;
    public bool CrashConsent { get => _crashConsent; set { _crashConsent = value; Changed(string.Empty); } }
    public bool CrashManualDumpConsent { get => _crashManualDumpConsent; set { _crashManualDumpConsent = value; Changed(string.Empty); } }
    public bool CanConsentCrash => CrashIdle && !_crashSent;
    public bool CanChooseManualCrashDump => CrashIdle && _crash?.Dump is not null;
    public bool CanExportCrash => HasCrashReport && !_crashSending && _crashDumpExport is null;
    public bool CanSaveCrashDump => CanExportCrash && _crash?.Dump is not null && _crashManualDumpConsent && !Playing;
    public Uri? CrashPrivateSupportUrl { get; } = PrivateSupportLink.Parse(Environment.GetEnvironmentVariable("FI_PRIVATE_SUPPORT_URL") ?? "https://discord.gg/NzB447nZYt");
    public bool CanOpenCrashSupport => CrashPrivateSupportUrl is not null;
    public string CrashSupportNote => S[CanOpenCrashSupport ? "crash.private_support_help" : "crash.private_support_unset"];
    public string CrashMigrationNote => _crashNewMetadataAction ? S["crash.legacy_action"] : string.Empty;
    public bool CanSendCrash => _crashConsent && CanExportCrash && !_crashSent && !Playing;
    public bool CrashIdle => !_crashSending && _crashDumpExport is null;
    public string CrashStatus => _crashStatus ?? string.Empty;
    public string CrashCode => _crash is null ? string.Empty : $"0x{_crash.ExitCode:x8}";
    public string CrashLocation => _crash?.Dump is { FaultModule: { } module, FaultModuleRva: { } rva }
        ? $"{module} + 0x{rva:x}" : S["crash.location_unknown"];
    public string CrashParticipants => _crash?.Context.World is { } world
        ? string.Join("\n", world.Participants.Select(p => $"{p.Gamertag} · {p.Xuid}")) : S["crash.no_world"];
    public string CrashContextNote => _crash?.Context.World is { } world
        ? S.Format("crash.roster_note", ("time", world.ObservedAt.ToLocalTime().ToString("g", CultureInfo.GetCultureInfo(S.Locale))))
        : S["crash.roster_missing"];
    public string CrashDumpNote => _crash?.Dump is null ? S["crash.no_dump"] : S["crash.dump_privacy"];

    private void PresentCrash(CrashIncident incident)
    {
        if (_session?.Me?.AccountId != incident.OwnerAccountId) return;
        ClearCrashScreen();
        _crash = incident; _crashOpen = true; _crashConsent = _crashManualDumpConsent = _crashNewMetadataAction = _crashSent = false; _crashStatus = null;
        _onMyData = _onHelp = _onMods = _onSetup = _onAccount = _onBugReport = false;
        try { _crashSubmission = CrashStore.Prepared(incident.Id); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { _crashSubmission = null; }
        if (_crashSubmission is not null && !_crashSubmission.IsCurrentMetadata)
        { _crashSubmission = null; _crashNewMetadataAction = true; }
        try { _crashSent = _crashSubmission is not null && CrashStore.SentReportId(incident.Id) == _crashSubmission.Request.ReportId; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { _crashSent = false; }
        if (_crashSent) _crashStatus = S["crash.sent"];
        Changed(string.Empty);
    }

    private void LoadPendingCrash()
    {
        ClearCrashScreen();
        if (_session?.Me?.AccountId is not { } account) return;
        try
        {
            var pending = CrashStore.Pending(account);
            if (pending.Count != 0)
            {
                bool NeedsSending(CrashIncident item)
                {
                    try { return CrashStore.SentReportId(item.Id) is null; }
                    catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return true; }
                }
                PresentCrash(pending.FirstOrDefault(NeedsSending) ?? pending[0]);
                if (_crashSent) _crashOpen = false;
            }
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { Log("crash: pending diagnostics unavailable"); }
    }

    private void ClearCrashScreen()
    {
        _crashSendStop?.Cancel();
        _crashDumpExport?.Stop.Cancel(); _crashDumpExport = null;
        _crash = null; _crashSubmission = null; _crashConsent = _crashManualDumpConsent = _crashOpen = _crashNewMetadataAction = _crashSent = false;
        _crashStatus = null;
    }

    public void ReviewCrash() { if (HasCrashReport) { _onMyData = _onHelp = _onMods = _onSetup = _onAccount = _onBugReport = false; _crashOpen = true; Changed(string.Empty); } }
    public void LaterCrash() { if (!CrashIdle) return; _crashOpen = _crashConsent = _crashManualDumpConsent = false; Changed(string.Empty); }

    public string? CopyableCrashReport() => CanExportCrash && _crash is { } incident
        ? CrashSupportReport.Create(incident, _crashSubmission?.Request.ReportId) : null;

    public void CrashExportResult(bool success) { _crashStatus = S[success ? "crash.exported" : "crash.export_failed"]; Changed(string.Empty); }

    internal sealed class ManualDumpExport(CrashIncident incident, AccountSession session) : IDisposable
    {
        internal CrashIncident Incident { get; } = incident;
        internal AccountSession Session { get; } = session;
        internal CancellationTokenSource Stop { get; } = new();
        public void Dispose() => Stop.Dispose();
    }

    internal ManualDumpExport? BeginManualCrashDumpExport()
    {
        if (!CanSaveCrashDump || _crash is not { } incident || _session is not { } session || session.Me?.AccountId != incident.OwnerAccountId) return null;
        var operation = new ManualDumpExport(incident, session);
        _crashDumpExport = operation; Changed(string.Empty);
        return operation;
    }

    internal bool IsCurrentCrashExport(ManualDumpExport operation) => ReferenceEquals(_crashDumpExport, operation)
        && ReferenceEquals(_session, operation.Session) && _crash?.Id == operation.Incident.Id && !operation.Stop.IsCancellationRequested;

    internal async Task<byte[]?> ReadCrashDumpForManualExportAsync(ManualDumpExport operation)
    {
        if (!IsCurrentCrashExport(operation)) return null;
        var bytes = await Task.Run(() => CrashStore.ReadDump(operation.Incident), operation.Stop.Token);
        return IsCurrentCrashExport(operation) ? bytes : null;
    }

    internal void FinishManualCrashDumpExport(ManualDumpExport operation, bool? success)
    {
        var current = IsCurrentCrashExport(operation);
        if (ReferenceEquals(_crashDumpExport, operation)) _crashDumpExport = null;
        if (current)
        {
            _crashManualDumpConsent = false;
            if (success is { } completed) CrashExportResult(completed);
        }
        Changed(string.Empty);
        operation.Dispose();
    }
    public void DiscardCrash()
    {
        if (!CrashIdle || _crash is null) return;
        try { CrashStore.Delete(_crash.Id); LoadPendingCrash(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { _crashStatus = S["crash.local_failed"]; }
        Changed(string.Empty);
    }

    public async Task SendCrashAsync()
    {
        if (!CanSendCrash || _crash is not { } incident || _session is not { } session || session.Me?.AccountId != incident.OwnerAccountId) return;
        _crashSending = true; _crashStatus = S["crash.sending"]; Changed(string.Empty);
        using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(45));
        _crashSendStop = bound;
        try
        {
            if (_crashSubmission is null)
            {
                var draft = CrashSubmission.Prepare(incident, _crashConsent, _crashNewMetadataAction ? Guid.CreateVersion7().ToString() : null);
                CrashStore.Prepare(draft, incident.Id);
                _crashSubmission = draft;
            }
            await _crashSubmission.SendAsync(session.Api, session.Tokens, bound.Token);
            if (!ReferenceEquals(_session, session) || _crash?.Id != incident.Id) return;
            CrashStore.MarkSent(incident.Id, _crashSubmission.Request.ReportId);
            _crashSent = true; _crashConsent = false; _crashStatus = Message = S["crash.sent"];
        }
        catch (ApiException e)
        { if (ReferenceEquals(_session, session) && _crash?.Id == incident.Id) _crashStatus = S[e.Code == ErrorCodes.AccountsFrozen ? "crash.frozen" : e.Code == ErrorCodes.CrashDumpsDisabled ? "crash.dumps_disabled" : "crash.send_failed"] + " (" + e.Code + ")"; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or HttpRequestException or OperationCanceledException or JsonException or InvalidOperationException)
        { if (ReferenceEquals(_session, session) && _crash?.Id == incident.Id) _crashStatus = S["crash.send_failed"]; }
        finally { if (ReferenceEquals(_crashSendStop, bound)) _crashSendStop = null; _crashSending = false; Changed(string.Empty); }
    }
}
