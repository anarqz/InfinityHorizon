// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Net.Http;
using Infinity.Core;
using Infinity.Core.Overlay;
using Infinity.Core.Presence;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// CR-L41.2, DISCORD_PRESENCE.md §4: a lobby link (<c>infinity-horizon://join/&lt;worldId&gt;</c>) arrives from a protocol launch or from a
/// second launch handed over through <c>launcher.lock</c>: the Join button of a Discord card or of the play dashboard (PLAY_DASHBOARD.md
/// §5.1). Owner 2026-11-08: the "Join from link" field (paste a link on PLAY) is gone; the deep link stays because those two buttons open it.
/// The player already chose to join there (and the browser asked before opening the launcher), so the world waits here and joins as soon as
/// the game is loaded (the overlay's own <c>POST /mm/v1/join {worldId}</c> through the session's worlds bridge, with the game ticket); every
/// step is said in the status line. A link waits at most <see cref="PendingJoinLifetime"/>.
/// </summary>
public sealed partial class ShellViewModel
{
    private string? _pendingJoin;
    private DateTimeOffset _pendingJoinAt;
    private bool _joining;
    private LauncherHandoff? _handoff;

    /// <summary>How long a received link waits for the game to load before it is dropped (said in the status line).</summary>
    internal static readonly TimeSpan PendingJoinLifetime = TimeSpan.FromMinutes(15);

    /// <summary>The window comes to the front (a link arrived from a second launch).</summary>
    public event Action? ActivateRequested;

    /// <summary>The presence tick: a waiting link joins once the session's game reports ready.</summary>
    private void TryPendingJoin()
    {
        if (_pendingJoin is null || _joining) return;
        if (DateTimeOffset.UtcNow - _pendingJoinAt > PendingJoinLifetime)
        {
            _pendingJoin = null;
            Message = S["join.expired"];
            Log("link: expired before the game loaded");
            return;
        }
        if (!GameStarted || _playWorlds is not { } worlds || worlds.GameReadiness.CurrentGeneration is null) return;
        _ = JoinNowAsync(worlds, _pendingJoin);
    }

    private async Task JoinNowAsync(Infinity.Core.Matchmaking.WorldsBridge worlds, string worldId)
    {
        if (_joining) return;
        _joining = true;
        try
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(45));
            var result = await worlds.JoinAsync(new UiFrame.JoinWorld(worldId), bound.Token);
            if (result.Result == UiJoin.Reserved)
            {
                _pendingJoin = null;
                Message = S["join.reserved"];
                OverlayToast(1, S["join.reserved_title"], S["worlds.reserved"]);
                Log("link: seat reserved");
            }
            else if (worlds.GameReadiness.CurrentGeneration is null)
            {
                Message = S["join.waiting_game"]; // the game left free roam meanwhile: kept for the next ready sample
            }
            else
            {
                _pendingJoin = null;
                Message = result.Message; // a mode refusal says so: the player picks the other profile on PLAY
                Log("link: join refused (" + result.Result + (worlds.LastJoinRefusal is { } why ? ", " + why : string.Empty) + ")");
            }
        }
        catch (Exception error) when (error is OperationCanceledException or HttpRequestException or IOException or TimeoutException)
        {
            _pendingJoin = null;
            Message = S["worlds.join_failed"];
            Log("link: join failed (" + error.GetType().Name + ")");
        }
        finally
        {
            _joining = false;
        }
    }

    /// <summary>A link from the command line or from a second launch: it waits for the game to load, and the status line says what happens.</summary>
    internal void ReceiveJoinLink(string worldId)
    {
        if (!JoinLink.IsWorldId(worldId)) return;
        _pendingJoin = worldId.ToLowerInvariant();
        _pendingJoinAt = DateTimeOffset.UtcNow;
        if (InShell) OpenHome();
        Message = S[!SignedIn ? "join.sign_in_first" : Playing ? "join.waiting_game" : "join.press_play"];
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
}
