// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Text.Json;
using Infinity.Core.Diagnostics;

namespace Infinity.App;

/// <summary>
/// OD-241: the crash screen has exactly two buttons. "Ignore" marks the crash handled (it never shows again, and the same .dmp is never
/// rebuilt into one, <see cref="CrashIncidentStore.MarkHandled"/>); "Get logs" runs the shared collector (<see cref="CollectLogsAsync"/>)
/// and then marks it handled. Nothing is sent to a server. Several crashes since the last time show as one screen (the newest, with the
/// count of the earlier ones) and are handled together.
/// </summary>
public sealed partial class ShellViewModel
{
    private CrashIncident? _crash;
    private CrashIncident[] _crashEarlier = [];
    private bool _crashOpen;
    private string? _crashStatus;
    private CrashIncidentStore CrashStore => new(_stateDirectory);

    public bool HasCrashReport => _crash is not null && InShell;
    public bool ShowCrashReport => HasCrashReport && _crashOpen && !_onMods && !_onSetup && !_onAccount && !_onBugReport;
    public bool CanHandleCrash => HasCrashReport && !_busy;
    public string CrashStatus => _crashStatus ?? string.Empty;
    public bool HasCrashStatus => _crashStatus is { Length: > 0 };
    public string CrashCode => _crash is null ? string.Empty : $"0x{_crash.ExitCode:x8}";
    public string CrashLocation => _crash?.Dump is { FaultModule: { } module, FaultModuleRva: { } rva }
        ? $"{module} + 0x{rva:x}" : S["crash.location_unknown"];
    public string CrashWhen => _crash is null ? string.Empty : _crash.OccurredAt.ToLocalTime().ToString("g", CultureInfo.GetCultureInfo(S.Locale));
    public string CrashParticipants => _crash?.Context.World is { } world
        ? string.Join("\n", world.Participants.Select(p => $"{p.Gamertag} · {p.Xuid}")) : S["crash.no_world"];
    public string CrashContextNote => _crash?.Context.World is { } world
        ? S.Format("crash.roster_note", ("time", world.ObservedAt.ToLocalTime().ToString("g", CultureInfo.GetCultureInfo(S.Locale))))
        : S["crash.roster_missing"];
    public bool HasCrashEarlier => _crashEarlier.Length > 0;
    public string CrashEarlierNote => _crashEarlier.Length == 0 ? string.Empty
        : S.Format(_crashEarlier.Length == 1 ? "crash.earlier_one" : "crash.earlier_many", ("count", _crashEarlier.Length.ToString(CultureInfo.CurrentCulture)));

    /// <summary>The game just crashed (the supervisor stored the incident): the screen with every crash not handled yet.</summary>
    private void PresentCrash(CrashIncident incident)
    {
        if (_session?.Me?.AccountId is not { } account || account != incident.OwnerAccountId) return;
        IReadOnlyList<CrashIncident> pending;
        try { pending = CrashStore.Pending(account); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { Log("crash: local reports unreadable (" + e.GetType().Name + ")"); pending = []; }
        if (!pending.Any(i => i.Id == incident.Id)) pending = [incident, .. pending];
        ShowCrashes(pending);
    }

    /// <summary>Sign-in: a crash the player has not handled yet (Ignore or Get logs) shows once more; a handled one never does.</summary>
    private void LoadPendingCrash()
    {
        ClearCrashScreen();
        if (_session?.Me?.AccountId is not { } account) return;
        try
        {
            var pending = CrashStore.Pending(account);
            Log($"crash: {pending.Count} unhandled local report(s)");
            ShowCrashes(pending);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { Log("crash: local reports unreadable (" + e.GetType().Name + ")"); }
    }

    private void ShowCrashes(IReadOnlyList<CrashIncident> pending)
    {
        ClearCrashScreen();
        if (pending.Count == 0) { Changed(string.Empty); return; }
        _crash = pending[0];
        _crashEarlier = pending.Skip(1).ToArray();
        _crashOpen = true;
        _onMods = _onSetup = _onAccount = _onBugReport = _onNews = false;
        Log($"crash: showing {CrashIncidentStore.HandledKey(_crash)} code 0x{_crash.ExitCode:x8} (+{_crashEarlier.Length} earlier)");
        Changed(string.Empty);
    }

    private void ClearCrashScreen()
    {
        _crash = null; _crashEarlier = []; _crashOpen = false; _crashStatus = null;
    }

    public void ReviewCrash() { if (HasCrashReport) { _onMods = _onSetup = _onAccount = _onBugReport = _onNews = false; _crashOpen = true; Changed(string.Empty); } }

    /// <summary>"Ignore": the crash (and the earlier ones on the screen) never shows again.</summary>
    public void IgnoreCrash()
    {
        if (!CanHandleCrash) return;
        if (MarkCrashesHandled("ignored")) LoadPendingCrash();
        Changed(string.Empty);
    }

    /// <summary>"Get logs": the shared collector, then the crash is handled (a failed collection keeps the screen, with why).</summary>
    public async Task GetCrashLogsAsync()
    {
        if (!CanHandleCrash) return;
        if (!await CollectLogsAsync(LogBundleKind.Crash)) { _crashStatus = Message; Changed(string.Empty); return; }
        if (MarkCrashesHandled("logs collected")) LoadPendingCrash();
        Changed(string.Empty);
    }

    private bool MarkCrashesHandled(string how)
    {
        if (_crash is not { } shown) return false;
        var all = new[] { shown }.Concat(_crashEarlier).ToArray();
        try
        {
            foreach (var incident in all) CrashStore.MarkHandled(incident);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            // never a silent dismissal: the screen stays and says it would come back
            Log("crash: handled list not saved (" + e.GetType().Name + ")");
            _crashStatus = S["crash.handled_failed"];
            return false;
        }
        foreach (var incident in all) Log("crash: " + how + " " + CrashIncidentStore.HandledKey(incident));
        return true;
    }
}
