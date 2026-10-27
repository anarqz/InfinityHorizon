// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Matchmaking;
using Infinity.Core.Process;
using Infinity.Core.Supervisor;
using Infinity.Core.Update;

namespace Infinity.App;

/// <summary>
/// OD-109: the dedicated lobby. The top bar's globe (admins only) opens a right pane; Create admits a
/// dedicated play session and hosts a world from this launcher with no game (<see cref="DedicatedHost"/>).
/// While it runs, the in-game overlay offers no lobby actions (<c>worlds.dedicated_running</c>) and the
/// account, game and profile cannot change. Stop, sign-out guards, online closure, a required update,
/// Windows session end and closing the launcher all delete the world and leave the session.
/// </summary>
public sealed partial class ShellViewModel
{
    private DedicatedHost? _dedicated;
    private DedicatedStatus _dedicatedStatus = DedicatedStatus.Idle;
    private bool _showLobbyPane, _lobbyStarting;
    private string _lobbyName = string.Empty;
    private byte _lobbyAudience, _lobbyLatency;
    private int _lobbyMaxPlayers = 12;
    private string? _lobbyLocalError;
    private LauncherLock? _instanceLock;
    // The play session's overlay bridge (OD-54), read off the UI thread.
    private volatile WorldsBridge? _playWorlds;
    /// <summary>The running online launch's lobby mod set (N-H2 reports go to it when it is a runtime source).</summary>
    private IModSetSource? _playModSet;

    /// <summary>The in-game overlay hosts (or is opening) a world: both would need the relay's UDP port.</summary>
    private bool PlayHostsWorld => _playWorlds?.HostsOrOpensWorld == true;

    /// <summary>The account role as <c>GET /launcher/v1/me</c> reads it (CORE_API: exactly <c>admin</c>).</summary>
    public bool IsAdmin
    {
        get
        {
            var admin = string.Equals(_session?.Me?.Role, "admin", StringComparison.Ordinal);
            InspectLobbyAdmin(ref admin);
            return admin;
        }
    }

    /// <summary>Thread-safe: the play session's bridge reads it off the UI thread.</summary>
    public bool DedicatedActive
    {
        get
        {
            var active = _dedicated?.Active == true;
            InspectDedicated(ref active);
            return active;
        }
    }

    partial void InspectLobbyAdmin(ref bool admin);
    partial void InspectDedicated(ref bool active);
    partial void InspectLobbyProfile(ref Profile? profile);
    partial void InspectLobbyMatchmaking(ref Uri? matchmaking);

    public bool ShowLobbyButton => SignedIn && (IsAdmin || DedicatedActive);

    public bool ShowLobbyPane => _showLobbyPane && ShowLobbyButton && _externalMod is null;

    public string LobbyName
    {
        get => _lobbyName;
        set { _lobbyName = value.Length > 48 ? value[..48] : value; Changed(); Changed(nameof(CanCreateLobby)); }
    }

    public bool LobbyAudienceEveryone { get => _lobbyAudience == 0; set => SetAudience(value, 0); }
    public bool LobbyAudienceSupporters { get => _lobbyAudience == 1; set => SetAudience(value, 1); }
    public bool LobbyAudienceFriends { get => _lobbyAudience == 2; set => SetAudience(value, 2); }
    public bool LobbyAudiencePrivate { get => _lobbyAudience == 3; set => SetAudience(value, 3); }

    public bool LobbyLatencyAny { get => _lobbyLatency == 0; set => SetLatency(value, 0); }
    public bool LobbyLatency150 { get => _lobbyLatency == 1; set => SetLatency(value, 1); }
    public bool LobbyLatency100 { get => _lobbyLatency == 2; set => SetLatency(value, 2); }
    public bool LobbyLatency60 { get => _lobbyLatency == 3; set => SetLatency(value, 3); }

    public string LobbyMaxPlayersText => _lobbyMaxPlayers.ToString(CultureInfo.InvariantCulture);
    public bool CanLobbyFewer => CanEditLobby && _lobbyMaxPlayers > 2;
    public bool CanLobbyMore => CanEditLobby && _lobbyMaxPlayers < 12;

    /// <summary>The mode is the selected profile's (read-only): matchmaking refuses a world of another mode.</summary>
    public string LobbyMode => LobbyProfile?.Mode switch
    {
        "hardcore" => S["signin.form_hardcore"],
        "noob" => S["signin.form_noob"],
        _ => "—",
    };

    public string LobbyRaces => S["lobby.races_on"];

    public bool CanEditLobby => !DedicatedActive && !_lobbyStarting;

    public bool CanCreateLobby => LobbyBlocker is null && !DedicatedActive && !_lobbyStarting && !_busy;

    public bool CanStopLobby => DedicatedActive && _dedicatedStatus.Phase != DedicatedPhase.Stopping;

    /// <summary>Why Create is unavailable right now (shown above the form), or empty.</summary>
    public string LobbyHint => DedicatedActive ? string.Empty : LobbyBlocker is { } key ? (key.StartsWith("online.", StringComparison.Ordinal) ? OnlineServiceMessage : S[key]) : string.Empty;

    public bool HasLobbyHint => LobbyHint.Length > 0;

    public string LobbyStateText => S[_dedicatedStatus.Phase switch
    {
        DedicatedPhase.Starting => "lobby.state_starting",
        DedicatedPhase.Running => "lobby.state_running",
        DedicatedPhase.Restarting => "lobby.state_restarting",
        DedicatedPhase.Stopping => "lobby.state_stopping",
        _ => _lobbyStarting ? "lobby.state_starting" : "lobby.state_idle",
    }];

    public bool LobbyRunning => _dedicatedStatus.Phase == DedicatedPhase.Running;

    public string LobbyCode => _dedicatedStatus.Code ?? "—";

    public bool ShowLobbyCode => _dedicatedStatus.Code is not null;

    public string LobbyPlayers => _dedicatedStatus.Active
        ? _dedicatedStatus.Players.ToString(CultureInfo.InvariantCulture) + " / " + _dedicatedStatus.MaxPlayers.ToString(CultureInfo.InvariantCulture)
        : "—";

    public string LobbyUptime => _dedicatedStatus.Phase == DedicatedPhase.Running
        ? (_dedicatedStatus.Uptime.TotalDays >= 1 ? ((int)_dedicatedStatus.Uptime.TotalDays).ToString(CultureInfo.InvariantCulture) + "d " : string.Empty)
            + _dedicatedStatus.Uptime.ToString(@"hh\:mm\:ss", CultureInfo.InvariantCulture)
        : "—";

    public string LobbyLastError => _lobbyLocalError ?? _dedicatedStatus.LastError?.Replace("\n", " · ", StringComparison.Ordinal) ?? string.Empty;

    public bool HasLobbyError => LobbyLastError.Length > 0;

    private Profile? LobbyProfile
    {
        get
        {
            var profile = _session?.Me?.Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey && p.Game == _gate.Game);
            InspectLobbyProfile(ref profile);
            return profile;
        }
    }

    private Uri? LobbyMatchmaking
    {
        get
        {
            var matchmaking = _session?.Info.Services?.Matchmaking is { } origin && Uri.TryCreate(origin, UriKind.Absolute, out var mm) ? mm : null;
            InspectLobbyMatchmaking(ref matchmaking);
            return matchmaking;
        }
    }

    /// <summary>The first reason this launcher cannot create a dedicated lobby now, as a catalog key.</summary>
    private string? LobbyBlocker =>
        !IsAdmin ? "lobby.role_required"
        : !OnlineServiceAllowsPlay ? "online.unavailable"
        : !LauncherUpdateAllowsPlay || LauncherUpdateBusy ? "lobby.update_required"
        : HasDeletionPending ? "lobby.deletion_pending"
        : !Ready || LobbyProfile is null ? "lobby.no_profile"
        : LobbyMatchmaking is null ? "lobby.unavailable"
        : PlayHostsWorld ? "lobby.overlay_hosting"
        : null;

    private void SetAudience(bool value, byte audience)
    {
        if (!value || !CanEditLobby) return;
        _lobbyAudience = audience;
        Changed(string.Empty);
    }

    private void SetLatency(bool value, byte latency)
    {
        if (!value || !CanEditLobby) return;
        _lobbyLatency = latency;
        Changed(string.Empty);
    }

    public void LobbyFewer() { if (CanLobbyFewer) { _lobbyMaxPlayers--; Changed(string.Empty); } }

    public void LobbyMore() { if (CanLobbyMore) { _lobbyMaxPlayers++; Changed(string.Empty); } }

    public void ToggleLobbyPane()
    {
        if (!ShowLobbyButton) return;
        _showLobbyPane = !_showLobbyPane;
        if (_showLobbyPane && _lobbyName.Length == 0) _lobbyName = "Infinity";
        // One right sidebar at a time: next to a mod panel the centre column is squeezed (the draft settings survive).
        if (_showLobbyPane) { _showModInfo = false; _showModOptions = false; _externalMod = null; _showModerationPane = false; }
        Changed(string.Empty);
    }

    public void CloseLobbyPane()
    {
        if (!_showLobbyPane) return;
        _showLobbyPane = false;
        Changed(string.Empty);
    }

    public async Task StartLobbyAsync()
    {
        _lobbyLocalError = null;
        if (!CanCreateLobby)
        {
            if (LobbyBlocker is not null) _lobbyLocalError = LobbyHint;
            Changed(string.Empty);
            return;
        }
        if (!await AcquireInstanceLockAsync(attempts: 1))
        {
            _lobbyLocalError = S["lobby.another_launcher"];
            Changed(string.Empty);
            return;
        }
        var session = _session;
        var profile = LobbyProfile;
        _lobbyStarting = true;
        Changed(string.Empty);
        try
        {
            if (!await RefreshOnlineServiceForPlayAsync() || !await RefreshLauncherPolicyAsync()
                || !ReferenceEquals(_session, session) || profile is null || LobbyProfile != profile || LobbyBlocker is { })
            {
                _lobbyLocalError = LobbyBlocker is not null ? LobbyHint : S["lobby.start_failed"];
                return;
            }
            if (profile.Mode == "hardcore")
            {
                // OD-112 follow-up: a Hardcore lobby carries no mod that affects multiplayer (its set would make it modded).
                await RefreshModsAsync();
                if (HardcoreRefusedMods is { Count: > 0 } refused) { _lobbyLocalError = HardcoreRefusal(refused); return; }
            }
            // OD-112: a dedicated host runs no game, so its lobby set is the enabled set of its launch plan (off the UI thread).
            var mods = await LaunchPlanModSetSource.FromManagedModsAsync(() => _install is null ? null : ManagedMods());
            if (mods.State.Phase == ModSetPhase.Failed) Log("lobby: mod set unavailable (" + mods.State.Reason + ")");
            if (!ReferenceEquals(_session, session) || LobbyProfile != profile) { _lobbyLocalError = S["lobby.start_failed"]; return; }
            var host = NewDedicatedHost(profile, mods);
            if (host is null) { _lobbyLocalError = S["lobby.unavailable"]; return; }
            _dedicated = host;
            Log($"lobby: start requested (audience {_lobbyAudience}, {_lobbyMaxPlayers} guests, latency {_lobbyLatency})");
            var status = await host.StartAsync(new DedicatedLobbySettings(_lobbyName, _lobbyAudience, (byte)_lobbyMaxPlayers, _lobbyLatency));
            _dedicatedStatus = status;
            Message = status.Phase == DedicatedPhase.Running ? S["lobby.started_toast"] : status.LastError?.Replace("\n", " · ", StringComparison.Ordinal);
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or InvalidOperationException or ArgumentException)
        {
            Log("lobby: start failed (" + error.GetType().Name + ")");
            _lobbyLocalError = S["lobby.start_failed"];
        }
        finally
        {
            _lobbyStarting = false;
            Changed(string.Empty);
        }
    }

    public async Task StopLobbyAsync()
    {
        _lobbyLocalError = null;
        if (_dedicated is not { } host) return;
        Log("lobby: stop requested");
        await host.StopAsync();
        Changed(string.Empty);
    }

    /// <summary>
    /// Bounded and synchronous for shutdown paths: 12 s by default; Windows session end passes a shorter
    /// bound (it allows about 5 s). An unfinished stop keeps running; the world lapses 45 s after its
    /// last announce and the session on its liveness window.
    /// </summary>
    internal void StopDedicatedForExit(TimeSpan? bound = null)
    {
        if (_dedicated is not { Active: true } host) return;
        Log("lobby: launcher closing; stopping the dedicated lobby");
        try { Task.Run(host.StopAsync).Wait(bound ?? TimeSpan.FromSeconds(12)); }
        catch (AggregateException) { }
    }

    private void StopDedicated(string reasonKey)
    {
        if (_dedicated is not { Active: true } host) return;
        Log("lobby: stopping (" + reasonKey + ")");
        _lobbyLocalError = S[reasonKey];
        _ = host.StopAsync();
        Changed(string.Empty);
    }

    /// <summary>A world bridge for the dedicated session: the overlay's (WorldsFor) without social, integrity or game readiness.</summary>
    private DedicatedHost? NewDedicatedHost(Profile profile, IModSetSource mods)
    {
        if (_session is not { } session || LobbyMatchmaking is not { } mm) return null;
        var relayKeys = IntegratedRelayKeys(mm);
        var devLan = DevLanAdmission(mm);
        TimeProvider? trusted = ProofClock(session);
        var relayReports = RelayReports;
        return CreateDedicatedHost(new GateClient(session.Api, session.Tokens), (ticket, activity) =>
        {
            var portInUse = false;
            var matchmaking = new MatchmakingClient(_config.Production ? ProductionMatchmakingHttp.Value : Http, mm, ClientHeader, ticket);
            return new WorldsBridge(matchmaking, key => S[key], trusted, profile.Mode,
                players => StartRelay(players, profile.Xuid, relayKeys, devLan, trusted,
                    error => portInUse = error is System.Net.Sockets.SocketError.AddressAlreadyInUse or System.Net.Sockets.SocketError.AccessDenied),
                () => HostRelay.LanAddresses(mm), profile.Xuid, Log,
                updateRelayWorld: static (relay, world, cancellation) => relay is IntegratedWorldRelay integrated
                    ? integrated.UpdateWorldAsync(world, cancellation) : Task.CompletedTask,
                refreshRelayAdmissions: (relay, cancellation) => relay is IntegratedWorldRelay integrated
                    ? integrated.RefreshAdmissionsAsync(matchmaking, cancellation) : Task.CompletedTask,
                checkWorldAccess: DedicatedWorldAccessAsync,
                onlineActivity: activity, idleWhenNoWorld: false, requireGameReadiness: false, closeWhenAlone: false)
            {
                SocketProofRequired = SocketProofRequired, SocketProofKeys = relayKeys,
                TrustedServerNow = session.Api.TrustedServerNow,
                RelayFailureKey = () => portInUse ? "lobby.port_in_use" : null,
                ModSet = mods,
                RelayReports = relayReports.ReportAsync,
            };
        }, profile, mods);
    }

    /// <summary>The host and its shell wiring (the harness's lifecycle check passes synthetic gate and bridges).</summary>
    private DedicatedHost CreateDedicatedHost(IGate gate, Func<Func<string>, Infinity.Core.Online.OnlineActivity, WorldsBridge> bridges, Profile profile,
        IModSetSource? mods = null)
    {
        var host = new DedicatedHost(gate, bridges, key => S[key], new DedicatedHostOptions(profile.Game, profile.Xuid, Version)
        {
            Mods = mods,
            HostingDenied = DedicatedHostingDeniedForStart,
            Log = Log,
        });
        host.Changed += status => _ui.Post(_ => DedicatedChanged(host, status), null);
        host.OnlinePaused += pause => _ui.Post(_ => ObserveOnlinePause(pause), null);
        host.UpdateRequired += () => _ui.Post(_ => RequireLauncherUpdate(), null);
        return host;
    }

    private void DedicatedChanged(DedicatedHost host, DedicatedStatus status)
    {
        if (!ReferenceEquals(host, _dedicated)) return;
        var phaseChanged = status.Phase != _dedicatedStatus.Phase;
        var wasActive = _dedicatedStatus.Active;
        _dedicatedStatus = status;
        if (wasActive && !status.Active)
            Message = _lobbyLocalError ?? status.LastError?.Replace("\n", " · ", StringComparison.Ordinal) ?? S["lobby.stopped_toast"];
        if (phaseChanged) { Changed(string.Empty); return; }
        // A tick: only the status lines, so a field being edited is never re-read.
        foreach (var name in new[] { nameof(LobbyStateText), nameof(LobbyCode), nameof(ShowLobbyCode), nameof(LobbyPlayers), nameof(LobbyUptime), nameof(LobbyLastError), nameof(HasLobbyError) })
            Changed(name);
    }

    /// <summary>
    /// The dedicated world's own policy: no content session (the server skips content checks for it),
    /// only a definite update requirement or an observed online closure. A transient update check or an
    /// online status read in flight never closes a 24/7 lobby.
    /// </summary>
    private Task<WorldAccessDecision> DedicatedWorldAccessAsync(CancellationToken cancellation) =>
        Task.FromResult(DedicatedHostingDenied() is { } key ? new WorldAccessDecision(false, key) : WorldAccessDecision.Compatible);

    private string? DedicatedHostingDenied() =>
        _onlinePause is not null ? "lobby.online_paused"
        : _updateAwaitingCommit || _updateRecoveryBlocked || _launcherUpdatePhase is UpdatePhase.Required or UpdatePhase.Downloading
            or UpdatePhase.Verifying or UpdatePhase.Applying or UpdatePhase.Failed or UpdatePhase.RecoveryRequired ? "lobby.update_required"
        : null;

    /// <summary>Start and re-admission also wait for an in-game lobby to close (one relay port).</summary>
    private string? DedicatedHostingDeniedForStart() => DedicatedHostingDenied() ?? (PlayHostsWorld ? "lobby.overlay_hosting" : null);

    /// <summary>One launcher per state directory (ORCHESTRATOR §1.8): taken at startup, retried briefly for an update handoff.</summary>
    internal async Task<bool> AcquireInstanceLockAsync(int attempts = 5)
    {
        for (var attempt = 0; ; attempt++)
        {
            if (_instanceLock is not null) return true;
            try
            {
                using var self = System.Diagnostics.Process.GetCurrentProcess();
                var started = (ulong)new DateTimeOffset(self.StartTime.ToUniversalTime(), TimeSpan.Zero).ToUnixTimeSeconds();
                if (LauncherLock.TryAcquire(_stateDirectory, started, out var acquired, out var holder))
                {
                    _instanceLock = acquired;
                    return true;
                }
                if (attempt + 1 >= attempts)
                {
                    Log("launcher.lock: held by another launcher" + (holder is { } h ? " (pid " + h.Pid.ToString(CultureInfo.InvariantCulture) + ")" : string.Empty));
                    return false;
                }
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidOperationException or System.ComponentModel.Win32Exception)
            {
                Log("launcher.lock: unavailable (" + error.GetType().Name + ")");
                return false;
            }
            await Task.Delay(TimeSpan.FromSeconds(1));
        }
    }

    private void ReleaseInstanceLock()
    {
        _instanceLock?.Dispose();
        _instanceLock = null;
    }
}
