// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// OD-154: the overlay's voice chat (protocol 3) against the world this profile is in. The lobby's switch rides on the
// world the host opened (and changes while it is open); the mutes are this profile's (Voice/VoiceRoom.cs).
public sealed partial class WorldsBridge
{
    /// <summary>The profile's voice chat; null leaves the overlay's voice frames unanswered (an older shell).</summary>
    public Voice.VoiceRoom? Voice { get; init; }

    /// <summary>The voice state for the world of the last "My world" answer, or null without <see cref="Voice"/>.</summary>
    public UiFrame.VoiceState? VoiceFrame()
    {
        if (Voice is not { } voice) return null;
        var mine = Volatile.Read(ref _mineSnapshot);
        voice.Observe(mine.WorldId, mine.Role);
        PushVoiceRules(voice);
        return voice.Frame(mine.Members);
    }

    // The host's switch and lobby mutes reach its own relay, which enforces them (RelayEngine.Voice.cs).
    private void PushVoiceRules(Voice.VoiceRoom voice)
    {
        IHostRelay? relay;
        lock (_hostState) relay = _relay;
        if (relay is IntegratedWorldRelay integrated && voice.RelayRules() is { } rules) integrated.SetVoice(rules.LobbyOn, rules.MutedForLobby);
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
}
