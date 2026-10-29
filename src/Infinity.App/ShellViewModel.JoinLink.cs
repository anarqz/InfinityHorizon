// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Net.Http;
using Infinity.Core;
using Infinity.Core.Overlay;
using Infinity.Core.Presence;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// CR-L41.2, DISCORD_PRESENCE.md §4: "Join from link". A link (or world id) comes from the field, from a protocol launch
/// (<c>infinity-horizon://join/&lt;worldId&gt;</c>) or from a second launch handed over through <c>launcher.lock</c>; it only fills
/// the field, the player presses Join. The join is the overlay's own (<c>POST /mm/v1/join {worldId}</c> through the session's
/// worlds bridge, with the game ticket): it needs the game loaded (the server's checks and the bridge's readiness rule), so
/// outside a loaded game the world waits here, joins as soon as the game is ready, and the player is told to choose
/// FREE ROAM ONLINE. A mode refusal offers a profile of the other mode.
/// </summary>
public sealed partial class ShellViewModel
{
    private string _joinLinkDraft = string.Empty;
    private string? _joinLinkStatus;
    private string? _pendingJoin;
    private string? _joinModeOffer;
    private string? _joinModeWorld;
    private bool _joining;
    private LauncherHandoff? _handoff;

    /// <summary>The window comes to the front (a link arrived from a second launch).</summary>
    public event Action? ActivateRequested;

    public string JoinLinkDraft
    {
        get => _joinLinkDraft;
        set
        {
            _joinLinkDraft = value ?? string.Empty;
            Changed();
            Changed(nameof(CanJoinFromLink));
            Changed(nameof(JoinLinkInvalid));
        }
    }

    public bool ShowJoinFromLink => SignedIn && !OfflineMode;

    public bool JoinLinkInvalid => _joinLinkDraft.Trim().Length > 0 && JoinLink.WorldId(_joinLinkDraft) is null;

    public bool CanJoinFromLink => SignedIn && !_joining && !DedicatedActive && JoinLink.WorldId(_joinLinkDraft) is not null;

    public string? JoinLinkStatus
    {
        get => _joinLinkStatus;
        private set { _joinLinkStatus = value; Changed(); Changed(nameof(HasJoinLinkStatus)); }
    }

    public bool HasJoinLinkStatus => !string.IsNullOrEmpty(_joinLinkStatus);

    public bool HasPendingJoin => _pendingJoin is not null;

    /// <summary>A mode refusal and a profile of that mode on the account: offer switching to it.</summary>
    public bool ShowJoinModeSwitch => _joinModeOffer is not null && JoinModeProfile is not null;

    public bool CanSwitchJoinMode => ShowJoinModeSwitch && !Playing && !_busy && !DedicatedActive;

    public string JoinModeSwitchLabel => S[_joinModeOffer == "hardcore" ? "join.switch_hardcore" : "join.switch_noob"];

    private Infinity.Core.Api.Profile? JoinModeProfile => _joinModeOffer is { } mode
        ? _session?.Me?.Profiles.FirstOrDefault(p => p.Game == _gate.Game && p.Mode == mode && p.Status == "active") : null;

    public async Task JoinFromLinkAsync()
    {
        if (JoinLink.WorldId(_joinLinkDraft) is not { } worldId) { JoinLinkStatus = S["join.invalid"]; return; }
        if (!CanJoinFromLink) return;
        _joinModeOffer = _joinModeWorld = null;
        Log("link: join asked");
        if (_playWorlds is { } worlds && GameStarted && worlds.GameReadiness.CurrentGeneration is not null)
        {
            await JoinNowAsync(worlds, worldId);
            return;
        }
        _pendingJoin = worldId;
        JoinLinkStatus = S[Playing ? "join.waiting_game" : "join.press_play"];
        ChangedJoin();
    }

    public void CancelPendingJoin()
    {
        _pendingJoin = null;
        JoinLinkStatus = null;
        ChangedJoin();
    }

    /// <summary>The presence tick: a waiting link joins once the session's game reports ready.</summary>
    private void TryPendingJoin()
    {
        if (_pendingJoin is not { } worldId || _joining || !GameStarted || _playWorlds is not { } worlds || worlds.GameReadiness.CurrentGeneration is null) return;
        _ = JoinNowAsync(worlds, worldId);
    }

    private async Task JoinNowAsync(Infinity.Core.Matchmaking.WorldsBridge worlds, string worldId)
    {
        if (_joining) return;
        _joining = true;
        ChangedJoin();
        try
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(45));
            var result = await worlds.JoinAsync(new UiFrame.JoinWorld(worldId, string.Empty), bound.Token);
            if (result.Result == UiJoin.Reserved)
            {
                _pendingJoin = null;
                JoinLinkStatus = S["join.reserved"];
                OverlayToast(1, S["join.reserved_title"], S["worlds.reserved"]);
                Log("link: seat reserved");
            }
            else if (result.Result == UiJoin.NotAllowed && worlds.LastJoinRefusal == "mode")
            {
                _pendingJoin = null;
                _joinModeWorld = worldId;
                _joinModeOffer = ProfileHardcore ? "noob" : "hardcore";
                JoinLinkStatus = result.Message;
                Log("link: refused for the profile's mode");
            }
            else if (worlds.GameReadiness.CurrentGeneration is null)
            {
                JoinLinkStatus = S["join.waiting_game"]; // the game left free roam meanwhile: kept for the next ready sample
            }
            else
            {
                _pendingJoin = null;
                JoinLinkStatus = result.Message;
                Log("link: join refused (" + result.Result + ")");
            }
        }
        catch (Exception error) when (error is OperationCanceledException or HttpRequestException or IOException or TimeoutException)
        {
            _pendingJoin = null;
            JoinLinkStatus = S["worlds.join_failed"];
            Log("link: join failed (" + error.GetType().Name + ")");
        }
        finally
        {
            _joining = false;
            ChangedJoin();
        }
    }

    /// <summary>Chooses the account's profile of the lobby's mode; the link waits for that profile's Play.</summary>
    public void SwitchJoinMode()
    {
        if (!CanSwitchJoinMode || JoinModeProfile is not { } profile) return;
        var world = _joinModeWorld;
        _joinModeOffer = _joinModeWorld = null;
        ChooseProfile(profile);
        if (world is not null)
        {
            _pendingJoin = world;
            JoinLinkStatus = S["join.press_play"];
        }
        ChangedJoin();
    }

    /// <summary>A link from the command line or from a second launch: fills the field and shows it, nothing joins by itself.</summary>
    internal void ReceiveJoinLink(string worldId)
    {
        if (!JoinLink.IsWorldId(worldId)) return;
        JoinLinkDraft = worldId;
        if (InShell) OpenHome();
        JoinLinkStatus = S[SignedIn ? "join.received" : "join.sign_in_first"];
        Log("link: received");
        ActivateRequested?.Invoke();
    }

    /// <summary>The lock holder listens for links from second launches (CR-L41.2).</summary>
    private void StartLinkListener()
    {
        if (_handoff is not null || !OperatingSystem.IsWindows()) return;
        try { _handoff = LauncherHandoff.Listen((uint)Environment.ProcessId, id => _ui.Post(_ => ReceiveJoinLink(id), null), Log); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { Log("link: hand-off unavailable (" + error.GetType().Name + ")"); }
    }

    /// <summary>
    /// CR-L41.2: <c>infinity-horizon://</c> for this user, written only by a release build (development builds never touch the
    /// registry), idempotently; a moved copy re-points it to itself.
    /// </summary>
    internal void RegisterJoinLinks()
    {
        if (!BuildProfile.ProductionOnly || !OperatingSystem.IsWindows() || Environment.ProcessPath is not { } exe) return;
        try
        {
            if (ProtocolRegistration.Ensure(new ProtocolRegistration.CurrentUser(), exe) > 0) Log("link: infinity-horizon:// registered for this user");
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or System.Security.SecurityException or ArgumentException)
        {
            Log("link: registration not written (" + error.GetType().Name + ")");
        }
    }

    private void ChangedJoin()
    {
        Changed(nameof(CanJoinFromLink));
        Changed(nameof(HasPendingJoin));
        Changed(nameof(ShowJoinModeSwitch));
        Changed(nameof(CanSwitchJoinMode));
        Changed(nameof(JoinModeSwitchLabel));
    }
}
