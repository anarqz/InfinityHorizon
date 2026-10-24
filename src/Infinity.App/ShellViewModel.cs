// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.IO;
using System.Net.Http;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Text.Json;
using Infinity.Core;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// The shell's bindings and its async commands. Decisions stay in Infinity.Core
/// (<see cref="ShellGate"/>, <see cref="AccountSession"/>); this class turns them into text and
/// runs them off the UI thread's blocking path (every call is awaited).
/// </summary>
public sealed partial class ShellViewModel : INotifyPropertyChanged, IDisposable
{
    private static readonly HttpClient Http = new(new HttpClientHandler { AllowAutoRedirect = false }) { Timeout = TimeSpan.FromSeconds(30) };
    private static readonly Lazy<HttpClient> ProductionMatchmakingHttp=new(Infinity.Core.Matchmaking.MatchmakingTransport.Create);

    private readonly ServerConfig _config;
    private readonly string _stateDirectory;
    private readonly Infinity.Core.Diagnostics.BoundedRunLog _runLog;
    private ShellGate _gate = ShellGate.SignedOut;
    private AccountSession? _session;
    private PendingAccount? _pending;
    private CancellationTokenSource? _signIn;
    private string? _message;
    private bool _busy;
    private string _newName = string.Empty;
    private bool _loginOpen;
    private bool _onBugReport;

    public ShellViewModel(Strings strings, ServerConfig config, string stateDirectory)
    {
        S = strings;
        _config = config;
        config.Validate();
        _stateDirectory = config.StateDirectory(stateDirectory);
        _resuming = SessionStore.Exists(_stateDirectory);
        _runLog=new(_stateDirectory);
        Version = Assembly.GetExecutingAssembly().GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion
            ?? Assembly.GetExecutingAssembly().GetName().Version?.ToString(3) ?? "0.0.0";
        ServerLabel = S.Format("shell.live", ("server", config.Server.Authority));
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    public Strings S { get; }

    public string ServerLabel { get; }

    public string Version { get; }

    public string VersionLabel => "v" + Version + " · BETA";

    public int SidebarWidth => InShell ? 220 : 0;

    public bool CanChangeIdentity => InShell && !Playing && !_busy && !LauncherUpdateBusy && !DedicatedActive;

    public bool ShowWelcome => !InShell && _pending is null && !_loginOpen && !_onMods;

    /// <summary>The kept session is being checked: the welcome screen says so instead of offering sign-in.</summary>
    public bool Resuming => _resuming;
    private bool _resuming;

    public bool ShowHero => ShowWelcome || ShowHome;

    public bool ShowBugReport => InShell && _onBugReport && !ShowPickSave;

    /// <summary>
    /// OD-118 "Play offline": the launcher without an account. The one game this launcher serves is chosen for the player (no server
    /// list without an account), then the local profiles are offered; every area opens, online ones say they are offline.
    /// </summary>
    public void PlayOffline()
    {
        if (_busy || InShell) return;
        _loginOpen = false; _pending = null; Message = null;
        Games.Clear();
        Games.Add(OfflineGame);
        Gate = _gate.PlayOffline().ChooseGame(OfflineGame.Id);
        FillProfiles();
    }

    /// <summary>The game an offline launcher plays (the server's list needs an account): Forza Horizon 2.</summary>
    internal static GameInfo OfflineGame { get; } = new("fh2", "Forza Horizon 2", "", "", true, null);

    public void OpenSignIn()
    {
        // OD-118: from the offline path the sign-in screen replaces it; cancelling returns to the welcome screen (Play offline again).
        if (OfflineMode && !Playing && !_busy) { Gate = _gate.SignOut(); Profiles.Clear(); Games.Clear(); }
        _loginOpen = true;
        Message = null;
        Changed(string.Empty);
    }

    public void OpenWelcome()
    {
        if (_busy) return;
        CancelVerification();
        _loginOpen = false;
        Message = null;
        Changed(string.Empty);
    }

    public void OpenBugReport()
    {
        if (!InShell) return;
        LeaveAccount();
        _onMods = _onSetup = false;
        _onBugReport = true;
        Changed(string.Empty);
    }

    /// <summary><c>infinity-launcher/&lt;version&gt; (windows; deck=0)</c> (headers.rs <c>FI_CLIENT</c>).</summary>
    public string ClientHeader => $"infinity-launcher/{Version} (windows; deck=0)";

    public ShellGate Gate
    {
        get => _gate;
        private set
        {
            _gate = value;
            Changed(string.Empty);
        }
    }

    public string? Message
    {
        get => _message;
        set
        {
            _message = value;
            _messageForConfiguration = false;
            Changed();
            Changed(nameof(DisplayMessage));
        }
    }

    public bool Busy
    {
        get => _busy;
        private set
        {
            _busy = value;
            Changed(string.Empty);
        }
    }

    public string NewName
    {
        get => _newName;
        set
        {
            _newName = value;
            Changed();
        }
    }

    public ObservableCollection<GameInfo> Games { get; } = [];

    public ObservableCollection<Profile> Profiles { get; } = [];

    /// <summary>The profile picker's rows: the profile and what its mode means now (OD-52).</summary>
    public ObservableCollection<ProfileRow> ProfileRows { get; } = [];

    /// <summary>An account is signed in: online features (sessions, integrity, lobbies, the account page).</summary>
    public bool SignedIn => _gate.SignedIn;

    /// <summary>OD-118: past the welcome screen, signed in or on the offline path: every launcher area opens.</summary>
    public bool InShell => _gate.Step != ShellStep.SignIn;

    /// <summary>OD-118: the launcher used without an account (the welcome screen's Play offline).</summary>
    public bool OfflineMode => _gate.Offline;

    public bool Ready => _gate.Step == ShellStep.Ready;

    public bool ShowSignIn => _gate.Step == ShellStep.SignIn && _pending is null && _loginOpen;

    public bool ShowNeedsAccount => _pending is not null;

    public bool ShowPickGame => _gate.Step == ShellStep.PickGame && !_onMods && !_onSetup && !_onBugReport && !_onAccount && !ShowCrashReport;

    public bool ShowPickProfile => _gate.Step == ShellStep.PickProfile && !ShowPickSave && !_onMods && !_onSetup && !_onBugReport && !_onAccount && !ShowCrashReport;

    public bool ShowHome => Ready && !ShowPickSave && !_onMods && !_onSetup && !_onBugReport && !_onAccount && !ShowCrashReport;

    public bool Idle => !_busy;

    /// <summary>
    /// The top bar: signed in, or the server's state before sign-in (checked at start, so
    /// "OFFLINE" means the server did not answer, never just "not signed in").
    /// </summary>
    public string Connection => OnlineServicePaused ? S["online.paused_label"] : _session is not null ? S["shell.connected"]
        : _serverReachable switch { true => S["shell.reachable"], false => S["shell.offline"], null => S["shell.checking"] };

    /// <summary>The server answers (or a session is open): the top bar's status dot.</summary>
    public bool Online => OnlineServiceAllowsPlay && (_session is not null || _serverReachable == true);

    private bool? _serverReachable;

    /// <summary>At start: <c>/launcher/v1/info</c> once, logged either way (the shell never calls the server silently).</summary>
    public Task ProbeServerAsync() => CheckLauncherUpdatesAsync();

    public string Email => _gate.AccountEmail ?? string.Empty;

    public string GameLabel => _gate.Game is { } id ? Games.FirstOrDefault(g => g.Id == id)?.Title ?? id.ToUpperInvariant() : S["shell.pick_game"];

    public string ProfileLabel => _gate.ProfileTag ?? S["shell.no_profile"];

    public string NeedsAccountText => _pending?.Email is { } email
        ? S["signin.needs_account"] + " " + S.Format("signin.form_email", ("email", email))
        : S["signin.needs_account"];

    public string ProfilesText => Profiles.Count == 0 ? S["profiles.none_yet"] : S.Format("profiles.from_server", ("game", GameLabel));

    public void CancelSignIn()
    {
        _signIn?.Cancel();
        _nativeFlow.Clear();
        Message = S["signin.cancelled"];
        Changed(string.Empty);
    }

    /// <summary>The first profile of a new account (<c>needs_account</c>).</summary>
    public async Task CreateAccountAsync()
    {
        if (!OnlineServiceAllowsPlay) { OpenOnlineStatus(); Message = OnlineServiceMessage; return; }
        if (!_registrationEnabled) { RegistrationDisabled(); return; }
        if (_pending is null || !CheckName())
        {
            return;
        }
        await Run(async () =>
        {
            var session = await _pending.CreateAccountAsync("fh2", NewName);
            _pending = null;
            NewName = string.Empty;
            try { await EnterAsync(session); }
            catch { session.Dispose(); throw; }
            if (session.Me?.Profiles.FirstOrDefault(p => p.Game == "fh2") is { } first)
            {
                OfferSaves(first);
            }
        });
    }

    public void ChooseGame(GameInfo game)
    {
        ArgumentNullException.ThrowIfNull(game);
        Gate = _gate.ChooseGame(game.Id);
        FillProfiles();
    }

    public void ChooseProfile(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        Gate = _gate.ChooseProfile(profile.Gamertag);
        Message = null;
        _ = RefreshIntegrityAutomaticallyAsync(); // OD-113: a Supporter + Hardcore profile is checked in the background
    }

    /// <summary>Re-opens the game picker (top bar "change").</summary>
    public void ChangeGame()
    {
        if (Playing || _busy || DedicatedActive) return;
        OpenHome();
        if (_session is not null)
        {
            Gate = _gate with { Game = null, ProfileTag = null };
        }
    }

    /// <summary>Re-opens the profile picker (top bar "change").</summary>
    public void ChangeProfile()
    {
        if (Playing || _busy || DedicatedActive) return;
        OpenHome();
        if (_gate.Game is not null)
        {
            Gate = _gate with { ProfileTag = null };
            FillProfiles();
            _ = RefreshIntegrityAutomaticallyAsync(); // OD-113: clears a previous profile's integrity result
        }
    }

    public async Task CreateProfileAsync()
    {
        if (_session is null || _gate.Game is not { } game || !CheckName())
        {
            return;
        }
        await Run(async () =>
        {
            Message = S.Format("profiles.creating", ("name", NewName));
            var hardcore = NewHardcore;
            var profile = await _session.CreateProfileAsync(game, NewName, hardcore ? "hardcore" : "noob");
            await _session.RefreshMeAsync();
            FillProfiles();
            Message = S.Format(hardcore ? "modes.created_hardcore" : "profiles.created", ("name", profile.Gamertag));
            NewName = string.Empty;
            if (!hardcore)
            {
                OfferSaves(profile); // a Hardcore career starts empty and the game seeds it (OD-60), never from a local save
            }
        });
    }

    public void SignOut()
    {
        // A dedicated lobby is stopped first (its gate leave needs this session's tokens): CanChangeIdentity hides sign-out meanwhile.
        if (Playing || _busy || DedicatedActive) return;
        _nativeFlow.Clear();
        LeaveAccount();
        _onMods = _onSetup = _onBugReport = _loginOpen = false;
        _playStatus = null;
        _session?.Dispose();
        _session = null;
        SessionStore.Delete(_stateDirectory);
        ClearIntegrity();
        ClearCrashScreen();
        _pending = null;
        _saveTarget = null;
        SaveChoices.Clear();
        Games.Clear();
        Profiles.Clear();
        Gate = _gate.SignOut();
        Message = S["account.signed_out_toast"];
    }

    private async Task EnterAsync(AccountSession session, CancellationToken cancellation = default)
    {
        var epoch = _onlinePauseEpoch;
        cancellation.ThrowIfCancellationRequested();
        if (!OnlineServiceAllowsPlay) throw new OperationCanceledException("Online services paused.", cancellation);
        // Stay signed in: every rotated refresh token is saved before the next request can use it.
        session.Tokens.Changed += refresh => KeepSession(session, refresh);
        var me = await session.RefreshMeAsync(cancellation);
        KeepSession(session, session.Tokens.RefreshToken);
        cancellation.ThrowIfCancellationRequested();
        if (!OnlineServiceAllowsPlay || epoch != _onlinePauseEpoch) throw new OperationCanceledException("Online services paused.", cancellation);
        _nativeFlow.Clear();
        _session = session;
        LoadPendingCrash();
        if (_install is not null && !_catalogLoaded) _ = LoadModCatalogQuietlyAsync(); // OD-45: updates checked when the launcher opens
        _loginOpen = false;
        LeaveAccount();
        _onMods = _onSetup = _onBugReport = false;
        Games.Clear();
        foreach (var game in session.Info.Games ?? [])
        {
            Games.Add(game);
        }
        var gate = _gate.SignIn(session.Email ?? (me.Profiles.Count > 0 ? me.Profiles[0].Gamertag : me.AccountId[..8]));
        // One game served: choose it for the player, as the owner's flow only asks when there is a choice.
        if (Games.Count == 1)
        {
            gate = gate.ChooseGame(Games[0].Id);
        }
        Gate = gate;
        FillProfiles();
        if (session.Deletion is not null)
        {
            _onAccount = true;
            DeletionEmail = session.Email ?? string.Empty;
            Message = DeletionPendingText;
        }
        else Message = S["signin.done_toast"];
        await RefreshIntegrityAutomaticallyAsync();
    }

    private void KeepSession(AccountSession session, Secret refresh)
    {
        try
        {
            SessionStore.Save(_stateDirectory, session.Tokens.AccountId, session.Email, refresh);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.ComponentModel.Win32Exception)
        {
            Log("auth: session not kept: " + e.GetType().Name);
        }
    }

    /// <summary>Signs back in with the session kept from the last run; any refusal falls back to the sign-in screen.</summary>
    public async Task ResumeSessionAsync()
    {
        if (_session is not null || _busy || !OnlineServiceAllowsPlay || SessionStore.Load(_stateDirectory) is not { } saved)
        {
            _resuming = false;
            Changed(nameof(Resuming));
            return;
        }
        Busy = true;
        AccountSession? session = null;
        try
        {
            using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(30));
            var (info, api) = await AccountSession.ConnectAsync(Http, _config, ClientHeader, cancellation.Token);
            session = AccountSession.Resume(api, info, DeviceIdentity.Load(_stateDirectory, "windows"), saved);
            await EnterAsync(session, cancellation.Token);
            Log("auth: session resumed");
            session = null;
        }
        catch (ApiException e)
        {
            Log($"auth: saved session refused: {e.Code}");
            if (e.Code is ErrorCodes.AuthRefreshInvalid or ErrorCodes.AuthRefreshReused or ErrorCodes.AccountsBanned) SessionStore.Delete(_stateDirectory);
        }
        catch (Exception e) when (e is HttpRequestException or IOException or JsonException or OperationCanceledException or InvalidOperationException)
        {
            Log($"auth: saved session not resumed: {e.GetType().Name}");
        }
        finally
        {
            session?.Dispose();
            _resuming = false;
            Busy = false;
            Changed(nameof(Resuming));
        }
    }

    private void FillProfiles()
    {
        Profiles.Clear();
        ProfileRows.Clear();
        foreach (var profile in _session?.Me?.Profiles.Where(p => p.Game == _gate.Game) ?? [])
        {
            Profiles.Add(profile);
            ProfileRows.Add(new ProfileRow(profile, ModeLine(profile)));
        }
        Changed(nameof(CanChooseHardcore));
        Changed(nameof(NewHardcore));
        Changed(nameof(ProfilesText));
    }

    private bool CheckName()
    {
        if (ProfileName.IsValid(NewName))
        {
            return true;
        }
        Message = S["profiles.name_rule"];
        return false;
    }

    private async Task Run(Func<Task> work)
    {
        if (_busy)
        {
            return;
        }
        Busy = true;
        try
        {
            await work();
        }
        catch (ApiException e) when (e.Code is ErrorCodes.AccountsNameUnavailable or ErrorCodes.AccountsInvalidName)
        {
            Message = S["profiles.name_rule"] + " (" + e.Code + ")";
        }
        catch (ApiException e) when (Infinity.Core.Online.OnlinePause.From(e) is { } pause)
        {
            ObserveOnlinePause(pause);
            ExtendOnlineCooldown(e.RetryAfter);
        }
        catch (ApiException e) when (e.Code == ErrorCodes.AccountsRegistrationDisabled)
        {
            RegistrationDisabled();
        }
        catch (OperationCanceledException)
        {
            Message = OnlineServiceAllowsPlay ? S["signin.cancelled"] : OnlineServiceMessage;
        }
        catch (Exception e) when (e is ApiException or HttpRequestException or IOException)
        {
            Message = Describe(e);
        }
        finally
        {
            if (_onlinePause is not null) Message = OnlineServiceMessage;
            Busy = false;
        }
    }

    private static string Describe(Exception e) => e is ApiException api ? api.Code : e.Message;

    private void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));

    public void Dispose()
    {
        // OD-109: the dedicated lobby's DELETE and gate leave start first and run beside the play
        // session's own cleanup; both finish (bounded) before the session's token manager is disposed.
        var dedicated = _dedicated is { Active: true } host ? Task.Run(host.StopAsync) : Task.CompletedTask;
        _integrityLifetime.Cancel();
        DisposeOnlineStatus();
        _updateStop.Cancel();
        _updater?.Dispose();
        ClearIntegrity();
        ClearCrashScreen();
        StopPlay();
        // the supervisor leaves the gate on cancel: let it finish before its token manager goes (SAURON)
        try
        {
            _playTask?.Wait(TimeSpan.FromSeconds(12));
        }
        catch (AggregateException)
        {
            // it ended with an error: nothing more to wait for
        }
        try
        {
            dedicated.Wait(TimeSpan.FromSeconds(12));
        }
        catch (AggregateException)
        {
            // the stop is bounded and logged; the world lapses 45 s after its last announce
        }
        _signIn?.Cancel();
        _nativeFlow.Dispose();
        LeaveAccount();
        _session?.Dispose();
        ReleaseInstanceLock();
    }
}
