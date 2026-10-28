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
        _runLog=new(_stateDirectory);
        // OD-118: every Infinity client behind one gate; nothing is sent until the player clicks SIGN IN (or resumes an online run).
        InfinityGate = CreateInfinityGate(new HttpClientHandler { AllowAutoRedirect = false });
        Http = new(InfinityGate, disposeHandler: false) { Timeout = TimeSpan.FromSeconds(30) };
        ProductionMatchmakingHttp = new(() => Infinity.Core.Matchmaking.MatchmakingTransport.Create(transport => CreateMatchmakingGate(transport)));
        _resuming = StartsOnline;
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

    public bool ShowBugReport => SignedIn && _onBugReport && !ShowPickSave;

    /// <summary>OD-118: an online-only area (the account page, bug reports) opened on the offline path says so, centered.</summary>
    public bool ShowOfflinePane => OfflineMode && (_onAccount || _onBugReport || _onOfflineStatus) && !ShowPickSave;

    /// <summary>
    /// OD-118 "Play offline": the launcher without an account. The one game this launcher serves is chosen for the player (no server
    /// list without an account), then the local profiles are offered; every area opens, online ones say they are offline.
    /// </summary>
    public void PlayOffline()
    {
        if (_busy || InShell) return;
        _loginOpen = false; _pending = null; Message = null;
        DisengageOnline(); // OD-118: from here until SIGN IN, not a single request to Infinity
        RememberLaunchMode(LaunchModeStore.Offline);
        Games.Clear();
        Games.Add(OfflineGame);
        Gate = _gate.PlayOffline().ChooseGame(OfflineGame.Id);
        FillProfiles();
        EnsureModCatalog(); // T1f: offline, Play needs the (GitHub) catalog's requirements; never wait for Mods to be opened
        // OD-118 T1c: the profile last chosen offline is chosen again (top bar no longer says "no profile" after a restart).
        if (LaunchModeStore.ReadLastProfile(_stateDirectory) is { } last
            && Profiles.FirstOrDefault(p => _rowLocals.TryGetValue(p.ProfileId, out var local) && local.Id == last) is { } row)
            ChooseProfile(row);
    }

    /// <summary>The game an offline launcher plays (the server's list needs an account): Forza Horizon 2.</summary>
    internal static GameInfo OfflineGame { get; } = new("fh2", "Forza Horizon 2", "", "", true, null);

    public void OpenSignIn()
    {
        // OD-118: from the offline path the sign-in screen replaces it; cancelling returns to the welcome screen (Play offline again).
        if (OfflineMode && !Playing && !_busy) { Gate = _gate.SignOut(); Profiles.Clear(); Games.Clear(); }
        _loginOpen = true; _onNews = false;
        Message = null;
        Changed(string.Empty);
        // OD-118 (G1): the player's SIGN IN is the first moment Infinity is contacted: the service status, then the release floor.
        if (!_onlineEngaged)
        {
            EngageOnline();
            _ = EngagedChecksAsync();
        }
    }

    private async Task EngagedChecksAsync()
    {
        // A SIGN IN after sign-out or Play offline may fall inside the previous answer's retry-after (or a read still in
        // flight): the first read waits for it instead of being dropped, so the status and the floor are always read. A
        // later sign-out or SIGN IN starts a new engagement; this one's waiter then stops without sending anything.
        var engagement = _onlineEngagement;
        bool Current() => _onlineEngaged && engagement == _onlineEngagement;
        try
        {
            for (var waited = false; Current();)
            {
                var wait = _onlineChecking ? TimeSpan.FromMilliseconds(100) : OnlineRetryRemaining;
                if (wait <= TimeSpan.Zero) break;
                if (!waited) { waited = true; _onlineWaiting = true; Message = S["online.checking"]; Changed(string.Empty); } // the form says why it waits
                // Task.Delay takes 1 ms to 1 day: a negative remainder means "now", a longer server cooldown is re-checked
                await Task.Delay(wait < TimeSpan.FromMilliseconds(1) ? TimeSpan.FromMilliseconds(1) : wait > TimeSpan.FromDays(1) ? TimeSpan.FromDays(1) : wait,
                    _onlineClock, _onlineStop.Token);
            }
        }
        catch (OperationCanceledException) { return; }
        finally
        {
            if (engagement == _onlineEngagement && _onlineWaiting)
            {
                _onlineWaiting = false;
                if (Message == S["online.checking"]) Message = null;
                Changed(string.Empty);
            }
        }
        if (!Current()) return;
        await CheckOnlineStatusAsync(manual: false);
        // the startup GitHub-only update probe may still be running: the floor read waits for it instead of being skipped
        try
        {
            while (Current() && OnlineServiceAllowsPlay && _updateChecking)
                await Task.Delay(TimeSpan.FromMilliseconds(100), _onlineClock, _onlineStop.Token);
        }
        catch (OperationCanceledException) { return; }
        if (!Current()) return;
        // OD-118 T1c: strictly in order; the release floor (/launcher/v1/info) only once the service answered open.
        if (_onlineEngaged && OnlineServiceAllowsPlay) await CheckLauncherUpdatesAsync();
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
        _onMods = _onSetup = _onNews = false;
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
            Changed(nameof(ShowAzimuthNameWarning));
            Changed(nameof(AzimuthNameWarning));
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

    public bool ShowPickGame => _gate.Step == ShellStep.PickGame && !_onMods && !_onSetup && !_onBugReport && !_onAccount && !_onNews && !ShowCrashReport;

    public bool ShowPickProfile => _gate.Step == ShellStep.PickProfile && !ShowPickSave && !_onMods && !_onSetup && !_onBugReport && !_onAccount && !_onNews && !ShowCrashReport;

    public bool ShowHome => Ready && !ShowPickSave && !_onMods && !_onSetup && !_onBugReport && !_onAccount && !_onNews && !ShowCrashReport;

    public bool Idle => !_busy;

    /// <summary>
    /// The top bar: signed in, or the server's state before sign-in (checked at start, so
    /// "OFFLINE" means the server did not answer, never just "not signed in").
    /// </summary>
    // OD-118 T1c: offline (before SIGN IN, or Play offline) nothing is checked, so the pill says so, never "checking".
    public string Connection => !_onlineEngaged && _session is null ? S["shell.offline"]
        : OnlineServicePaused ? S["online.paused_label"] : _session is not null ? S["shell.connected"]
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

    public string ProfilesText => Profiles.Count == 0 ? S["profiles.none_yet"]
        : S.Format(OfflineMode ? "offline.profiles_local" : "profiles.from_server", ("game", GameLabel));

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
        if (_firstLocal is not null)
        {
            await CreateAccountFromLocalAsync(_pending, _firstLocal);
            return;
        }
        // Owner 2026-10-27: the new account's first profile starts from the Azimuth career too, as a Noob profile made on the
        // profile screen: named as Azimuth's player it adopts that career, otherwise the picker offers this PC's Azimuth saves.
        var adoptsAzimuth = ShowAzimuthNameWarning;
        Profile? created = null;
        await Run(async () =>
        {
            var session = await _pending.CreateAccountAsync("fh2", NewName);
            _pending = null;
            NewName = string.Empty;
            try { await EnterAsync(session); }
            catch { session.Dispose(); throw; }
            created = session.Me?.Profiles.FirstOrDefault(p => p.Game == "fh2");
        });
        if (created is { Mode: not "hardcore" } first) // B2: a Hardcore career starts empty
        {
            if (adoptsAzimuth && await AdoptAzimuthCareerAsync(first)) return;
            OfferSaves(first);
        }
    }

    /// <summary>The local profile a new account starts with (OD-120: POST /accounts registers it), null when none is on this PC.</summary>
    private LocalProfile? _firstLocal;

    /// <summary>
    /// OD-120: a new account whose first profile is this PC's own (made signed out): POST /accounts with its name under its upload
    /// key, then linked like any registration, and its offline save continues online (P1b). A name taken meanwhile renames the local
    /// profile first (the player typed another name), so the profile and its save are never left behind.
    /// </summary>
    private Task CreateAccountFromLocalAsync(PendingAccount pending, LocalProfile first) => Run(async () =>
    {
        // The stored copy decides: another window may have changed or removed it since the screen opened.
        if (LocalProfiles.Read().FirstOrDefault(p => p.Id == first.Id && p.LocalOnly) is not { } local) { _firstLocal = null; Message = S["offline.error.other"]; return; }
        if (!string.Equals(NewName.Trim(), local.Name, StringComparison.Ordinal))
        {
            try { local = _firstLocal = LocalProfiles.Edit(local.Id, NewName.Trim(), local.Mode); }
            catch (LocalProfileException refused) { Message = ProfileErrorText(refused.Code); return; }
        }
        AccountSession session; Profile profile;
        try { (session, profile) = await pending.CreateAccountAsync(new NewProfile(local.Game, local.Name, local.Mode), local.UploadKey); }
        catch (ApiException refused) when (refused.Code == ErrorCodes.AccountsNameUnavailable) { Message = S["offline.error.name_unavailable"]; return; }
        var linked = LocalProfiles.Link(local.Id, new CloudLink(session.Tokens.AccountId, profile.ProfileId, profile.Xuid, profile.Gamertag, profile.RenameNotice));
        _pending = null; _firstLocal = null;
        NewName = string.Empty;
        var firstLine = S.Format("upsert.registered", ("names", linked.DisplayName)) + SaveNote(linked);
        try { await EnterAsync(session); }
        catch { session.Dispose(); throw; }
        // The sign-in's own registrations (the other local profiles) follow this one's line.
        Message = Message is { } after && after != S["signin.done_toast"] ? firstLine + " " + after : firstLine;
    });

    public void ChooseGame(GameInfo game)
    {
        ArgumentNullException.ThrowIfNull(game);
        Gate = _gate.ChooseGame(game.Id);
        FillProfiles();
    }

    public void ChooseProfile(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        _chosenLocal = _rowLocals.GetValueOrDefault(profile.ProfileId);
        if (OfflineMode && _chosenLocal is { } chosen) RememberLastLocalProfile(chosen.Id);
        Gate = _gate.ChooseProfile(ProfileName.Plain(profile.Gamertag), profile.ProfileId);
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
            Gate = _gate with { Game = null, ProfileTag = null, ProfileKey = null };
        }
    }

    /// <summary>Re-opens the profile picker (top bar "change").</summary>
    public void ChangeProfile()
    {
        if (Playing || _busy || DedicatedActive) return;
        OpenHome();
        if (_gate.Game is not null)
        {
            Gate = _gate with { ProfileTag = null, ProfileKey = null };
            FillProfiles();
            _ = RefreshIntegrityAutomaticallyAsync(); // OD-113: clears a previous profile's integrity result
        }
    }

    public async Task CreateProfileAsync()
    {
        // OD-118: created on this PC (no cloud call); signed in, uploaded at once by the idempotent upsert.
        if (_gate.Game is not { } game || !CheckName())
        {
            return;
        }
        await CreateLocalProfileAsync(game);
    }

    public void SignOut()
    {
        // A dedicated lobby is stopped first (its gate leave needs this session's tokens): CanChangeIdentity hides sign-out meanwhile.
        if (Playing || _busy || DedicatedActive) return;
        _nativeFlow.Clear();
        LeaveAccount(); _onNews = false;
        _onMods = _onSetup = _onBugReport = _loginOpen = false;
        _playStatus = null;
        ClearDiscord(); // CR-S63
        ClearShellStatus(); // CR-L40
        _session?.Dispose();
        _session = null;
        SessionStore.Delete(_stateDirectory);
        DisengageOnline(); // OD-118: signed out is offline again until the next SIGN IN
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
        RememberLaunchMode(LaunchModeStore.Online); // OD-118 (G2): the next start resumes this session
        cancellation.ThrowIfCancellationRequested();
        if (!OnlineServiceAllowsPlay || epoch != _onlinePauseEpoch) throw new OperationCanceledException("Online services paused.", cancellation);
        _nativeFlow.Clear();
        _session = session;
        LoadPendingCrash();
        if (_install is not null && !_catalogLoaded) _ = LoadModCatalogQuietlyAsync(); // OD-45: updates checked when the launcher opens
        _loginOpen = false;
        LeaveAccount();
        _onMods = _onSetup = _onBugReport = _onNews = false;
        Games.Clear();
        foreach (var game in session.Info.Games ?? [])
        {
            Games.Add(game);
        }
        var gate = _gate.SignIn(session.Email ?? (me.Profiles.Count > 0 ? ProfileName.Plain(me.Profiles[0].Gamertag) : me.AccountId[..8]));
        // One game served: choose it for the player, as the owner's flow only asks when there is a choice.
        if (Games.Count == 1)
        {
            gate = gate.ChooseGame(Games[0].Id);
        }
        Gate = gate;
        FillProfiles();
        if (session.Deletion is not null)
        {
            _onAccount = true; _onNews = false;
            DeletionEmail = session.Email ?? string.Empty;
            Message = DeletionPendingText;
        }
        else Message = S["signin.done_toast"];
        _ = RefreshDiscordAsync(); // CR-S63: the account's Discord handle on its profile rows (after sign-in only, OD-118)
        RestartShellStatus(now: true); // CR-L40: the connected server's figures in the sidebar, then every 30 s
        await RefreshIntegrityAutomaticallyAsync();
        await OfferProfileUpsertAsync(); // OD-118: this PC's profiles that could go online, and the account cache refreshed (P3)
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
        catch (TrustChangedException change)
        {
            ObserveTrustChange(change);
        }
        catch (Exception e) when (e is HttpRequestException or IOException or JsonException or OperationCanceledException or InvalidOperationException
            or InvalidDataException)
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
        _rowLocals.Clear();
        // OD-118: offline the local profiles; signed in the account's plus the local ones not on it (offline-only until uploaded).
        var game = _gate.Game ?? "";
        var rows = OfflineMode ? OfflineRows(game)
            : (_session?.Me?.Profiles.Where(p => p.Game == game).Select(p => (Row: p, Local: (LocalProfile?)null)) ?? [])
                .Concat(_session?.Me is { } me ? LocalOnlyRows(game, me.AccountId) : []);
        foreach (var (profile, local) in rows)
        {
            Profiles.Add(profile);
            ProfileRows.Add(new ProfileRow(profile, local is null ? ModeLine(profile) : ModeLine(profile) + " · " + LocalLine(local), OnAccount: local is null && !OfflineMode));
            if (local is not null) _rowLocals[profile.ProfileId] = local;
        }
        Changed(nameof(CanChooseHardcore));
        Changed(nameof(NewHardcore));
        Changed(nameof(ProfilesText));
        ChangedSeedForm();
        ChangedNoobSeedForm();
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
            Message = ErrorText(e.Code); // OD122-L7: "already in use" or the name rule, never the raw code
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

    private string Describe(Exception e) => e switch
    {
        ApiException { Code: ErrorCodes.AccountsBanned } banned => BannedText(banned.Details), // CR-S52: the review of a seeded line
        ApiException api => ErrorText(api.Code),
        HttpRequestException => S["native.unreachable"],
        _ => S["native.failed"],
    };

    /// <summary>
    /// OD122-L7: a server error code in the player's language (<c>error.&lt;CODE&gt;</c>, every code of fi-protocol's catalog); a code this
    /// build does not know reads "Unexpected error (CODE)".
    /// </summary>
    internal string ErrorText(string code) => KnownErrorText(code) ?? S.Format("error.unexpected", ("code", code));

    /// <summary>The translated text of a server error code, or null for a code this build has no text for.</summary>
    internal string? KnownErrorText(string code) => S["error." + code] is var text && text != "error." + code ? text : null;

    private void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));

    public void Dispose()
    {
        _newsClient?.Dispose();
        if (_pickerHttp.IsValueCreated) _pickerHttp.Value.Dispose();
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
        _discordAvatarHttp?.Dispose();
        _session?.Dispose();
        ReleaseInstanceLock();
    }
}
