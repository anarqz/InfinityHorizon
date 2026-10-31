// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// OD-154: the overlay's voice chat (protocol 3) against the world this profile is in. The lobby's voice mode (CR-S67, CR-L46: off,
// open, proximity) rides on the world the host opened (and changes while it is open); the mutes are this profile's
// (Voice/VoiceRoom.cs) and are reported to the server (CR-S68); the server's voice blocks reach the host's relay from the grants and
// from the roster (CR-S67), and this player through the roster's own row and the voice_blocked notice.
public sealed partial class WorldsBridge
{
    /// <summary>The profile's voice chat; null leaves the overlay's voice frames unanswered (an older shell).</summary>
    public Voice.VoiceRoom? Voice { get; init; }

    /// <summary>
    /// CR-S68: <c>GET /core/v1/me/notices</c> with the launcher's token (null: none). Read when the roster says this player's own voice
    /// block started or changed, for the <c>voice_blocked</c> notice's end; its toast is localized by kind (the server's text is
    /// English only). Without it, the roster's own row alone says it.
    /// </summary>
    public Func<CancellationToken, Task<IReadOnlyList<AccountNotice>>>? AccountNotices { get; init; }

    // CR-S68 (under _voiceNotice): this player's own block as the roster said it last, the end it was told about, and the notice read
    private readonly object _voiceNotice = new();
    private DateTimeOffset? _selfBlockSeen, _voiceBlockToldUntil;
    private bool _noticeWanted;
    private DateTimeOffset _noticesNext;
    private int _noticesReading;
    // CR-S67: the hosted world's voice mode as this launcher last told the server (the PUT, then a heartbeat that carried a change)
    private string? _hostedVoice;

    /// <summary>The voice state for the world of the last "My world" answer, or null without <see cref="Voice"/>.</summary>
    public UiFrame.VoiceState? VoiceFrame()
    {
        if (Voice is not { } voice) return null;
        var mine = Volatile.Read(ref _mineSnapshot);
        voice.Observe(mine.WorldId, mine.Role);
        PushVoiceRules(voice);
        return voice.Frame(mine.Members);
    }

    // The host's mode and lobby mutes reach its own relay, which enforces them (RelayEngine.Voice.cs).
    private void PushVoiceRules(Voice.VoiceRoom voice)
    {
        IHostRelay? relay;
        lock (_hostState) relay = _relay;
        if (relay is IntegratedWorldRelay integrated && voice.RelayRules() is { } rules) integrated.SetVoice(rules.Mode, rules.MutedForLobby);
    }

    /// <summary>An overlay's voice command, then the state to show (also when the command was not the player's to make).</summary>
    public UiFrame.VoiceState? VoiceAct(UiFrame.VoiceCommand command)
    {
        ArgumentNullException.ThrowIfNull(command);
        if (Voice is not { } voice) return null;
        var mine = Volatile.Read(ref _mineSnapshot);
        voice.Observe(mine.WorldId, mine.Role);
        if (!voice.Apply(command, mine.Members)) _log?.Invoke($"voice: op {command.Op} refused (not this player's to make)");
        PushVoiceRules(voice);
        return voice.Frame(mine.Members);
    }

    /// <summary>VOICE_CHAT.md §15: the microphone or output the player picked, then the state to show.</summary>
    public UiFrame.VoiceState? VoiceDevice(UiFrame.VoiceDevice device)
    {
        ArgumentNullException.ThrowIfNull(device);
        if (Voice is not { } voice) return null;
        voice.SetDevice(device);
        var mine = Volatile.Read(ref _mineSnapshot);
        voice.Observe(mine.WorldId, mine.Role);
        return voice.Frame(mine.Members);
    }

    /// <summary>CR-S67: the voice mode the hosted world's row should carry now, when it differs from what the server was last told.</summary>
    private string? VoiceChange()
    {
        if (Voice?.HostMode() is not { } mode) return null;
        return mode == Volatile.Read(ref _hostedVoice) ? null : mode;
    }

    /// <summary>
    /// A "My world" read: the world row's voice mode for a joiner (display only), the roster's voice blocks for the host's relay
    /// (CR-S67: the host's own block reaches it only this way), and this player's own block.
    /// </summary>
    private void ObserveVoice(string worldId, byte role, WorldView world, WorldMembers? roster)
    {
        if (Voice is { } voice)
        {
            voice.Observe(worldId, role);
            if (role != UiWorldRole.Hosting) voice.WorldSaid(worldId, world.Voice);
        }
        if (roster is null) return;
        if (role == UiWorldRole.Hosting)
        {
            IHostRelay? relay;
            lock (_hostState) relay = _relay;
            if (relay is IntegratedWorldRelay integrated) integrated.SetRosterVoiceBlocks(roster.Items);
        }
        var self = roster.Items.FirstOrDefault(m => m.Friend == "self" || _ownXuid is not null && m.Xuid == _ownXuid);
        if (self is null) return;
        DateTimeOffset? until = self.VoiceBlockedUntil is { } instant && DateTimeOffset.TryParse(instant, CultureInfo.InvariantCulture,
            DateTimeStyles.AssumeUniversal, out var at) && at > Now ? at : null;
        Voice?.SetBlockedUntil(until);
        bool read;
        lock (_voiceNotice)
        {
            if (until != _selfBlockSeen)
            {
                _selfBlockSeen = until;
                _noticeWanted = until is not null;
                if (until is null) _voiceBlockToldUntil = null; // lifted: a later block is told again
            }
            read = _noticeWanted && Now >= _noticesNext;
        }
        if (read) _ = ReadNoticesAsync(until!.Value);
    }

    /// <summary>
    /// CR-S68: the account's <c>voice_blocked</c> notice, for its end (else the roster's), then one toast per block, localized by kind
    /// (never who muted or why).
    /// </summary>
    private async Task ReadNoticesAsync(DateTimeOffset rosterUntil)
    {
        if (Interlocked.Exchange(ref _noticesReading, 1) != 0) return;
        try
        {
            var until = rosterUntil;
            if (AccountNotices is { } read)
            {
                using var bounded = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token);
                bounded.CancelAfter(TimeSpan.FromSeconds(10));
                var notices = await read(bounded.Token).ConfigureAwait(false);
                if (notices.FirstOrDefault(n => n?.Kind == AccountNoticeKinds.VoiceBlocked)?.Until is { } instant
                    && DateTimeOffset.TryParse(instant, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var at) && at > Now)
                    until = at;
            }
            lock (_voiceNotice)
            {
                _noticeWanted = false;
                if (_selfBlockSeen is null || _voiceBlockToldUntil == until) return;
                _voiceBlockToldUntil = until;
            }
            Voice?.SetBlockedUntil(until);
            _freeRoamToasts.Enqueue(VoiceBlockedToast(until));
            _log("voice: this account's voice is blocked (voice_blocked)");
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or TimeoutException or OperationCanceledException
            or System.Text.Json.JsonException)
        {
            lock (_voiceNotice) _noticesNext = Now + TimeSpan.FromMinutes(1);
        }
        finally { Volatile.Write(ref _noticesReading, 0); }
    }

    /// <summary>The <c>voice_blocked</c> notice in this player's language, with its end in local time.</summary>
    internal UiFrame.Toast VoiceBlockedToast(DateTimeOffset until) =>
        new(UiLevel.Warn, 2, 150, _text("voice.blocked")
            .Replace("{until}", until.ToLocalTime().ToString("g", CultureInfo.CurrentCulture), StringComparison.Ordinal));
}
