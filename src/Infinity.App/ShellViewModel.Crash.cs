// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Diagnostics;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    private CrashIncident? _crash;
    private CrashSubmission? _crashSubmission;
    private bool _crashOpen, _crashConsent, _crashDumpConsent, _crashSending;
    private string? _crashStatus;
    private CancellationTokenSource? _crashSendStop;
    private CrashIncidentStore CrashStore => new(_stateDirectory);

    public bool HasCrashReport => _crash is not null && SignedIn;
    public bool ShowCrashReport => HasCrashReport && _crashOpen && !_onMods && !_onSetup && !_onAccount && !_onBugReport && !ShowPickSave;
    public bool CrashConsent { get => _crashConsent; set { _crashConsent = value; Changed(string.Empty); } }
    public bool CrashDumpConsent { get => _crashDumpConsent; set { if (CanChooseCrashDump) _crashDumpConsent = value; Changed(string.Empty); } }
    public bool CanChooseCrashDump => _crash?.Dump is not null && _crashSubmission is null && !_crashSending;
    public bool CanSendCrash => _crashConsent && HasCrashReport && !_crashSending && !Playing;
    public bool CrashIdle => !_crashSending;
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
        _crash = incident; _crashOpen = true; _crashConsent = _crashDumpConsent = false; _crashStatus = null;
        _onMods = _onSetup = _onAccount = _onBugReport = false;
        try { _crashSubmission = CrashStore.Prepared(incident.Id); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { _crashSubmission = null; }
        if (_crashSubmission is not null) _crashDumpConsent = _crashSubmission.Request.Consent.Minidump;
        Changed(string.Empty);
    }

    private void LoadPendingCrash()
    {
        ClearCrashScreen();
        if (_session?.Me?.AccountId is not { } account) return;
        try { var pending = CrashStore.Pending(account); if (pending.Count != 0) PresentCrash(pending[0]); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { Log("crash: pending diagnostics unavailable"); }
    }

    private void ClearCrashScreen()
    {
        _crashSendStop?.Cancel();
        _crash = null; _crashSubmission = null; _crashConsent = _crashDumpConsent = _crashOpen = false;
        _crashStatus = null;
    }

    public void ReviewCrash() { if (HasCrashReport) { _onMods = _onSetup = _onAccount = _onBugReport = false; _crashOpen = true; Changed(string.Empty); } }
    public void LaterCrash() { if (_crashSending) return; _crashOpen = _crashConsent = false; Changed(string.Empty); }
    public void DiscardCrash()
    {
        if (_crashSending || _crash is null) return;
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
            var dump = _crashDumpConsent ? await Task.Run(() => CrashStore.ReadDump(incident)) : null;
            if (!ReferenceEquals(_session, session) || _crash?.Id != incident.Id || bound.IsCancellationRequested) return;
            if (_crashDumpConsent && (dump is null || !MinidumpInspector.IsMinimizedUpload(dump)))
            { _crashStatus = S["crash.dump_unsupported"]; return; }
            if (_crashSubmission is null)
            {
                _crashSubmission = CrashSubmission.Prepare(incident, _crashConsent, dump);
                CrashStore.Prepare(_crashSubmission);
            }
            var complete = await _crashSubmission.SendAsync(session.Api, session.Tokens, dump, bound.Token);
            if (!ReferenceEquals(_session, session) || _crash?.Id != incident.Id) return;
            CrashStore.Delete(incident.Id);
            LoadPendingCrash(); Message = S[complete ? "crash.sent" : "crash.sent_without_dump"];
        }
        catch (ApiException e)
        { if (ReferenceEquals(_session, session)) _crashStatus = S[e.Code == "ACCOUNTS.FROZEN" ? "crash.frozen" : "crash.send_failed"] + " (" + e.Code + ")"; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or HttpRequestException or OperationCanceledException or JsonException or InvalidOperationException)
        { if (ReferenceEquals(_session, session)) _crashStatus = S["crash.send_failed"]; }
        finally { if (ReferenceEquals(_crashSendStop, bound)) _crashSendStop = null; _crashSending = false; Changed(string.Empty); }
    }
}
