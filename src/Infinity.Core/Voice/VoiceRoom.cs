// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Matchmaking;
using Infinity.Core.Overlay;

namespace Infinity.Core.Voice;

/// <summary>
/// The launcher's voice chat for the profile in play (OD-154, VOICE_CHAT.md §4, §11): the lobby's switch, this
/// player's settings and mutes, and who talks, turned into the overlay's <see cref="UiFrame.VoiceState"/>.
/// <list type="bullet">
/// <item>The lobby carries its voice chat switch: the host sets it when creating the lobby (on by default) and may
/// change it at any time while the lobby is open. Until the server's world row carries it (CR-S67) a joiner learns it
/// from the host's relay (VC-1); before that it reads "set by the host".</item>
/// <item>A mute is this profile's: kept in <see cref="VoiceStore"/> and applied again when that player is in a later
/// lobby. "Mute everyone" lasts for the session only; "unmute everyone" also lifts the current players' kept mutes.</item>
/// <item>The host may mute a player for the whole lobby, for the lobby's life.</item>
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
    private readonly HashSet<string> _lobbyMutes = [];
    private readonly HashSet<string> _talking = [];
    private string? _world;
    private bool _host;
    private bool? _lobbyVoice;
    private bool? _created; // the switch of the lobby being created, applied once its world id is known

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

    /// <summary>The lobby's switch: null while unknown (a joiner before the host's relay says it).</summary>
    public bool? LobbyVoice { get { lock (_lock) return _lobbyVoice; } }

    /// <summary>Whether this profile hears <paramref name="xuid"/> (the relay's per-listener filter, VC-1).</summary>
    public bool MutedBy(string xuid)
    {
        lock (_lock) return _mutes.ContainsKey(xuid) || _sessionMutes.Contains(xuid) || _lobbyMutes.Contains(xuid);
    }

    /// <summary>What the host's relay enforces (OD-154): null when this profile does not host the current world.</summary>
    public (bool LobbyOn, IReadOnlyCollection<ulong> MutedForLobby)? RelayRules()
    {
        lock (_lock)
        {
            if (!_host || _world is null) return null;
            var muted = _lobbyMutes.Select(x => ulong.TryParse(x, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var n) ? n : 0)
                .Where(n => n != 0).ToArray();
            return (_lobbyVoice ?? true, muted);
        }
    }

    /// <summary>The overlay asked to create a lobby with this switch.</summary>
    public void Creating(bool voice)
    {
        lock (_lock) _created = voice;
    }

    /// <summary>
    /// The world this profile is in, from each "My world" answer (<see cref="UiWorldRole"/>): a new world resets the
    /// lobby's state; the host's own world takes the switch it was created with (on when unknown, e.g. after a restart).
    /// </summary>
    public void Observe(string? worldId, byte role)
    {
        lock (_lock)
        {
            if (role == UiWorldRole.None || string.IsNullOrEmpty(worldId))
            {
                Reset(null, false);
                return;
            }
            var host = role == UiWorldRole.Hosting;
            if (_world == worldId && _host == host) return;
            Reset(worldId, host);
            if (host)
            {
                _lobbyVoice = _created ?? true;
                _created = null;
            }
        }
    }

    /// <summary>The host's relay told a joiner the lobby's switch (VC-1); ignored by the host, who owns it.</summary>
    public void LobbySaid(bool on)
    {
        lock (_lock)
            if (!_host && _world is not null) _lobbyVoice = on;
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

    /// <summary>Applies an overlay's command; false when it is not this player's to make (a joiner's lobby switch).</summary>
    public bool Apply(UiFrame.VoiceCommand command, IReadOnlyList<MemberRow> roster)
    {
        ArgumentNullException.ThrowIfNull(command);
        ArgumentNullException.ThrowIfNull(roster);
        lock (_lock)
        {
            switch (command.Op)
            {
                case UiVoiceOp.LobbyVoice:
                    if (!_host || _world is null) return false;
                    _lobbyVoice = command.Value == 1;
                    return true;
                case UiVoiceOp.MuteForLobby:
                    if (!_host || _world is null || command.Xuid == _profile) return false;
                    if (command.Value == 1) _lobbyMutes.Add(command.Xuid); else _lobbyMutes.Remove(command.Xuid);
                    return true;
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
            if (_lobbyVoice is { } on)
            {
                flags |= UiVoice.LobbyKnown;
                if (on) flags |= UiVoice.LobbyOn;
            }
            if (_host && _world is not null) flags |= UiVoice.MayChange;
            if (_settings.Enabled ?? _defaultEnabled) flags |= UiVoice.Enabled;
            var members = Others(roster).Take(UiFrame.MaxVoiceMembers).Select(m =>
            {
                byte state = 0;
                if (_mutes.ContainsKey(m.Xuid) || _sessionMutes.Contains(m.Xuid)) state |= UiVoiceMember.MutedByMe;
                if (_talking.Contains(m.Xuid)) state |= UiVoiceMember.Talking;
                if (_lobbyMutes.Contains(m.Xuid)) state |= UiVoiceMember.MutedForLobby;
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
        if (on) _mutes[xuid] = new VoiceMute(gamertag, _clock.GetUtcNow().UtcDateTime, Sent: false);
        else _mutes.Remove(xuid);
        if (_store is not null && _profile.Length != 0) _store.SetMute(_profile, xuid, on ? _mutes[xuid] : null);
    }

    private void Save(VoiceSettings settings)
    {
        _settings = settings;
        if (_store is not null && _profile.Length != 0) _store.SaveSettings(_profile, settings);
    }

    private void Reset(string? world, bool host)
    {
        _world = world;
        _host = host;
        _lobbyVoice = null;
        _lobbyMutes.Clear();
        _sessionMutes.Clear();
        _talking.Clear();
    }
}
