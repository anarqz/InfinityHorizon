// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>What a "Who can join" change answers (OD-210, W14): one notice, and the control's new state (null when it did not change).</summary>
public sealed record JoinModesAnswer(UiFrame.Toast Notice, UiFrame.LobbyJoinModes? Frame);

// OD-210 (CR-S101, MATCHMAKING_API §2.11, W14): official lobbies are mixed (Noob and Hardcore); a private official lobby's current
// leader picks who may join from now on: Everyone (the default, `any`), Hardcore only or Noob only. The overlay's Private Lobby pane
// shows the control from WorldView.modes (0x28 LobbyJoinModes, after every MyWorld) and lets only the leader change it (the invite
// rule, PrivateLobbyPolicy.MayInvite / lobby_policy.h); the change (0x29 JoinModesSet) is PUT /mm/v1/worlds/{worldId}/modes {modes}.
// It applies to new joins only: players already in the lobby stay, and kicking stays the leader's job. Placement is the server's:
// the launcher's quick join and private create never name a mode. FM5 has no Hardcore (the server answers 400 `modes`), so its
// lobbies offer Everyone and Noob only.
public sealed partial class WorldsBridge
{
    // the private world whose join modes the overlay shows: its id, WorldView.modes (an older server: its mode) and its game
    private sealed record JoinModesWorld(string Id, string? Modes, string Game);
    private JoinModesWorld? _joinModesWorld;
    private int _joinModesBusy;

    private void ObserveJoinModes(WorldView world) =>
        Volatile.Write(ref _joinModesWorld, new JoinModesWorld(world.Id, world.Modes ?? world.Mode, world.Game));

    /// <summary>
    /// The private lobby's "Who can join" as last read (no request), for the overlay after every <see cref="UiFrame.MyWorld"/>;
    /// <see cref="UiFrame.LobbyJoinModes.None"/> outside a private official lobby (or before its world was read).
    /// </summary>
    public UiFrame.LobbyJoinModes JoinModesFrame()
    {
        var mine = Volatile.Read(ref _mineSnapshot);
        if (!InPrivateMine(mine) || Volatile.Read(ref _joinModesWorld) is not { } world || world.Id != mine.WorldId
            || System.Text.Encoding.UTF8.GetByteCount(world.Id) > UiFrame.MaxPaneId)
            return UiFrame.LobbyJoinModes.None;
        return new UiFrame.LobbyJoinModes(world.Id, UiJoinModes.Of(world.Modes), UiJoinModes.OfferedOf(world.Game));
    }

    /// <summary>
    /// The leader chose who can join (<paramref name="choice"/>, <see cref="UiJoinModes"/>): one <c>PUT …/modes</c> (its one transport
    /// retry sends the same body: the route is idempotent) and one notice marked Always. Nothing is sent outside a private lobby this
    /// player leads, nor a choice its game does not offer; a press while one is answered is dropped (null).
    /// </summary>
    public async Task<JoinModesAnswer?> SetJoinModesAsync(byte choice, CancellationToken cancellation)
    {
        if (Interlocked.Exchange(ref _joinModesBusy, 1) != 0) return null;
        try { return await SetJoinModesCoreAsync(choice, cancellation).ConfigureAwait(false); }
        finally { Volatile.Write(ref _joinModesBusy, 0); }
    }

    private async Task<JoinModesAnswer> SetJoinModesCoreAsync(byte choice, CancellationToken cancellation)
    {
        var mine = Volatile.Read(ref _mineSnapshot);
        if (!InPrivateMine(mine)) return Refusal(PrivateLobbyNotices.JoinModesPrivateOnly, "outside a private lobby");
        if (!PrivateLobbyPolicy.MayInvite(mine.Leader, _ownXuid))
            return Refusal(PrivateLobbyNotices.JoinModesNotLeader, mine.Leader.Length == 0 ? "the leader is not known" : "another player leads");
        var current = JoinModesFrame();
        if (current.WorldId.Length != 0 && !UiJoinModes.Offers(current.Offered, choice))
            return Refusal(PrivateLobbyNotices.JoinModesUnavailable, $"choice {choice} is not offered in this lobby's game");
        var modes = UiJoinModes.ToServer(choice);
        MmResult<WorldView>? answer = null;
        string reason;
        try
        {
            try
            {
                answer = await _mm.SetWorldModesAsync(mine.WorldId, modes, cancellation).ConfigureAwait(false);
            }
            catch (HttpRequestException) when (!cancellation.IsCancellationRequested)
            {
                answer = await _mm.SetWorldModesAsync(mine.WorldId, modes, cancellation).ConfigureAwait(false);
            }
            reason = answer.Ok ? PrivateLobbyNotices.JoinModesSet : PrivateLobbyNotices.JoinModesRefusal(answer.Error!);
        }
        catch (NotSupportedException)
        {
            reason = PrivateLobbyNotices.JoinModesFailed;
        }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested)
        {
            _log($"worlds: who can join: failed ({error.GetType().Name})");
            reason = PrivateLobbyNotices.JoinModesFailed;
        }
        if (reason != PrivateLobbyNotices.JoinModesSet)
            return Refusal(reason, answer?.Error is { } refused ? $"HTTP {(int)refused.Status} {refused.Code}" : "no answer");
        var world = answer!.Value is { } view && view.Id == mine.WorldId ? view : null;
        Volatile.Write(ref _joinModesWorld, new JoinModesWorld(mine.WorldId, world?.Modes ?? modes,
            world?.Game ?? Volatile.Read(ref _joinModesWorld)?.Game ?? InviteGame));
        if (world is not null) ObserveNoticeWorld(world); // OD-160's notice reads the world's modes too
        _log($"worlds: who can join: {mine.WorldId} admits {modes} from now on (players already in it stay)");
        var who = _text(PrivateLobbyNotices.JoinModesChoiceKeys[UiJoinModes.Of(world?.Modes ?? modes)]);
        var notice = PrivateLobbyNotices.Done(PrivateLobbyNotices.Text(_text, PrivateLobbyNotices.JoinModesSet).Replace("{who}", who, StringComparison.Ordinal));
        return new(notice, JoinModesFrame());
    }

    private JoinModesAnswer Refusal(string reason, string why)
    {
        _log($"worlds: who can join: not changed ({reason}: {why})");
        return new(PrivateLobbyNotices.Toast(_text, reason), null);
    }
}
