// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Matchmaking;
using Infinity.Core.Overlay;

namespace Infinity.Core.Voice;

/// <summary>
/// The launcher's voice chat for the profile in play (OD-154, VOICE_CHAT.md §4, §11): the lobby's switch, this
/// player's settings and mutes, and who talks, turned into the overlay's <see cref="UiFrame.VoiceState"/>.
/// <list type="bullet">
/// <item>The lobby carries its voice mode (CR-S67, CR-L46: off, open or proximity). This player reads it from the world row
/// (<see cref="WorldSaid"/>, display only; OD-208: players host no lobby, so nobody here sets it).</item>
/// <item>A mute or unmute of a player is reported to the server (CR-S68, <see cref="MuteReported"/>); "mute everyone" is not
/// a judgement of anybody and is never reported.</item>
/// <item>A mute is this profile's: kept in <see cref="VoiceStore"/> and applied again when that player is in a later
/// lobby. "Mute everyone" lasts for the session only; "unmute everyone" also lifts the current players' kept mutes.</item>
/// <item>Voice on/off and the mic mode are settings of the profile; voice starts on for Hardcore, off for Noob.</item>
/// </list>
/// No audio and no network here: the relay (VC-1) reads <see cref="MutedBy"/> and <see cref="LobbyVoice"/>, and the
/// talking state arrives through <see cref="SetTalking"/>.
/// </summary>
public sealed class VoiceRoom
{
    private readonly object _lock = new();
    private readonly VoiceStore? _store;
    private readonly string _profile;
    private readonly TimeProvider _clock;
    private readonly bool _defaultEnabled;
    private VoiceSettings _settings;
    private Dictionary<string, VoiceMute> _mutes;
    private readonly Dictionary<string, PlayerVolume> _volumes; // §15: per player XUID, kept with the settings
    private readonly HashSet<string> _sessionMutes = [];
    private readonly HashSet<string> _talking = [];
    private string? _world;
    private string? _lobbyMode; // VoiceModes; null while unknown
    private DateTimeOffset? _blockedUntil; // CR-S67/S68: this player's own voice block, from the roster or the voice_blocked notice

    public VoiceRoom(VoiceStore? store, string mode, string? profileXuid, TimeProvider? clock = null)
    {
        _store = store;
        _profile = profileXuid ?? string.Empty;
        _clock = clock ?? TimeProvider.System;
        _defaultEnabled = mode == WorldModes.Hardcore;
        var known = store is not null && _profile.Length != 0;
        _settings = known ? store!.Settings(_profile) : new(null, UiMicMode.Open);
        _mutes = known ? new(store!.Mutes(_profile)) : [];
        _volumes = known ? new(store!.Volumes(_profile)) : [];
    }

    /// <summary>Voice is on in this profile's settings (the mode's default until the player changes it).</summary>
    public bool Enabled { get { lock (_lock) return _settings.Enabled ?? _defaultEnabled; } }

    /// <summary>The lobby's voice is on (its mode is not off): null while unknown (a joiner before the world or the relay says it).</summary>
    public bool? LobbyVoice { get { lock (_lock) return _lobbyMode is null ? null : _lobbyMode != Matchmaking.VoiceModes.Off; } }

    /// <summary>The lobby's voice mode (<see cref="Matchmaking.VoiceModes"/>): null while unknown.</summary>
    public string? LobbyMode { get { lock (_lock) return _lobbyMode; } }

    /// <summary>
    /// CR-S68: this player muted (true) or unmuted (false) <c>xuid</c> in the world <c>worldId</c>; the launcher reports it to the
    /// server (<c>POST /launcher/v1/voice/mutes</c>). Called under this room's lock: it must only queue.
    /// </summary>
    public Action<string, bool, string>? MuteReported { get; init; }

    /// <summary>The server took this profile's mute of <paramref name="xuid"/>: its history entry is marked sent.</summary>
    public void MuteSent(string xuid)
    {
        lock (_lock)
        {
            if (!_mutes.TryGetValue(xuid, out var mute) || mute.Sent) return;
            _mutes[xuid] = mute with { Sent = true };
            if (_store is not null && _profile.Length != 0) _store.SetMute(_profile, xuid, _mutes[xuid]);
        }
    }

    /// <summary>CR-S67/S68: this player's own voice block (null: none), from the roster's own row or the <c>voice_blocked</c> notice.</summary>
    public void SetBlockedUntil(DateTimeOffset? until)
    {
        lock (_lock) _blockedUntil = until;
    }

    /// <summary>This player's voice is blocked now (CR-S67/S68).</summary>
    public bool Blocked { get { lock (_lock) return _blockedUntil is { } until && until > _clock.GetUtcNow(); } }

    /// <summary>Whether this profile hears <paramref name="xuid"/> (the relay's per-listener filter, VC-1).</summary>
    public bool MutedBy(string xuid)
    {
        lock (_lock) return _mutes.ContainsKey(xuid) || _sessionMutes.Contains(xuid);
    }

    /// <summary>
    /// The world this profile is in, from each "My world" answer (<see cref="UiWorldRole"/>): a new world resets the lobby's state.
    /// </summary>
    public void Observe(string? worldId, byte role)
    {
        lock (_lock)
        {
            if (role == UiWorldRole.None || string.IsNullOrEmpty(worldId))
            {
                Reset(null);
                return;
            }
            if (_world == worldId) return;
            Reset(worldId);
        }
    }

    /// <summary>
    /// The world row this player is seated in says the lobby's voice mode (CR-S67, <c>WorldView.voice</c>): shown only; ignored for
    /// another world than this profile's.
    /// </summary>
    public void WorldSaid(string worldId, string mode)
    {
        lock (_lock)
            if (_world == worldId && Matchmaking.VoiceModes.All.Contains(mode)) _lobbyMode = mode;
    }

    /// <summary>Who talks now (VC-1/VC-3 feed it); <paramref name="xuids"/> replaces the set.</summary>
    public void SetTalking(IEnumerable<string> xuids)
    {
        lock (_lock)
        {
            _talking.Clear();
            _talking.UnionWith(xuids);
        }
    }

    /// <summary>Applies an overlay's command; false when it is not this player's to make (the lobby switch and lobby mutes are nobody's, OD-208).</summary>
    public bool Apply(UiFrame.VoiceCommand command, IReadOnlyList<MemberRow> roster)
    {
        ArgumentNullException.ThrowIfNull(command);
        ArgumentNullException.ThrowIfNull(roster);
        lock (_lock)
        {
            switch (command.Op)
            {

                case UiVoiceOp.Mute:
                    if (command.Xuid == _profile) return false;
                    Mute(command.Xuid, roster.FirstOrDefault(m => m.Xuid == command.Xuid)?.Gamertag ?? string.Empty, command.Value == 1);
                    return true;
                case UiVoiceOp.MuteEveryone:
                    foreach (var member in Others(roster))
                    {
                        if (command.Value == 1) _sessionMutes.Add(member.Xuid);
                        else Mute(member.Xuid, member.Gamertag, false);
                    }
                    return true;
                case UiVoiceOp.Mic:
                    Save(_settings with { Mic = command.Value });
                    return true;
                case UiVoiceOp.Enabled:
                    Save(_settings with { Enabled = command.Value == 1 });
                    return true;
                case UiVoiceOp.VoiceVolume: // §15: the frame already checked 1..100
                    Save(_settings with { VoiceVolume = command.Value });
                    return true;
                case UiVoiceOp.MicVolume:
                    Save(_settings with { MicVolume = command.Value });
                    return true;
                case UiVoiceOp.PlayerVolume:
                    if (command.Xuid == _profile) return false;
                    var volume = command.Value >= UiFrame.MaxVolume ? null : new PlayerVolume(command.Value, _clock.GetUtcNow().UtcDateTime);
                    if (volume is null) _volumes.Remove(command.Xuid); else _volumes[command.Xuid] = volume;
                    if (_store is not null && _profile.Length != 0) _store.SetVolume(_profile, command.Xuid, volume);
                    return true;
                default:
                    return false;
            }
        }
    }

    /// <summary>§15: the microphone or output the player picked (an empty id: the system default), kept per profile.</summary>
    public void SetDevice(UiFrame.VoiceDevice device)
    {
        ArgumentNullException.ThrowIfNull(device);
        string? id = device.Id.Length == 0 ? null : device.Id, name = id is null || device.Name.Length == 0 ? null : device.Name;
        lock (_lock)
            Save(device.Flow == UiVoiceFlow.Microphone ? _settings with { MicId = id, MicName = name } : _settings with { OutputId = id, OutputName = name });
    }

    /// <summary>What the overlay shows for <paramref name="roster"/> (this player left out, at most 16).</summary>
    public UiFrame.VoiceState Frame(IReadOnlyList<MemberRow> roster)
    {
        ArgumentNullException.ThrowIfNull(roster);
        lock (_lock)
        {
            byte flags = 0;
            if (_lobbyMode is { } mode)
            {
                flags |= UiVoice.LobbyKnown;
                if (mode != Matchmaking.VoiceModes.Off) flags |= UiVoice.LobbyOn;
                if (mode == Matchmaking.VoiceModes.Proximity) flags |= UiVoice.Proximity;
            }
            if (_blockedUntil is { } until && until > _clock.GetUtcNow()) flags |= UiVoice.Blocked;
            if (_settings.Enabled ?? _defaultEnabled) flags |= UiVoice.Enabled;
            var members = Others(roster).Take(UiFrame.MaxVoiceMembers).Select(m =>
            {
                byte state = 0;
                if (_mutes.ContainsKey(m.Xuid) || _sessionMutes.Contains(m.Xuid)) state |= UiVoiceMember.MutedByMe;
                if (_talking.Contains(m.Xuid)) state |= UiVoiceMember.Talking;
                return new VoiceMember(m.Xuid, m.Gamertag, state, _volumes.TryGetValue(m.Xuid, out var v) ? v.Volume : UiFrame.MaxVolume);
            }).ToList();
            var s = _settings;
            return new UiFrame.VoiceState(flags, s.Mic, members, s.VoiceVolume, s.MicVolume, s.MicId ?? "", s.MicId is null ? "" : s.MicName ?? "",
                s.OutputId ?? "", s.OutputId is null ? "" : s.OutputName ?? "");
        }
    }

    private IEnumerable<MemberRow> Others(IReadOnlyList<MemberRow> roster) =>
        roster.Where(m => m.Friend != UiFriend.Self && m.Xuid != _profile && m.Xuid.Length is > 0 and <= 20 && m.Xuid.All(char.IsAsciiDigit));

    private void Mute(string xuid, string gamertag, bool on)
    {
        _sessionMutes.Remove(xuid);
        // CR-S68: a change of this profile's kept mute is reported in the world it happened in (none outside a world)
        if (_mutes.ContainsKey(xuid) != on && _world is { } world) MuteReported?.Invoke(xuid, on, world);
        if (on) _mutes[xuid] = new VoiceMute(gamertag, _clock.GetUtcNow().UtcDateTime, Sent: false);
        else _mutes.Remove(xuid);
        if (_store is not null && _profile.Length != 0) _store.SetMute(_profile, xuid, on ? _mutes[xuid] : null);
    }

    private void Save(VoiceSettings settings)
    {
        _settings = settings;
        if (_store is not null && _profile.Length != 0) _store.SaveSettings(_profile, settings);
    }

    private void Reset(string? world)
    {
        _world = world;
        _lobbyMode = null;
        _sessionMutes.Clear();
        _talking.Clear();
    }
}
