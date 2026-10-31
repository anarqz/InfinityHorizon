// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Voice;

/// <summary>A player this profile muted (VOICE_CHAT.md §4): kept on the PC and applied again in every later lobby.</summary>
public sealed record VoiceMute(string GamertagAtMute, DateTime MutedAtUtc, bool Sent);

/// <summary>
/// One profile's voice settings: <c>Enabled</c> null follows the mode (Hardcore on, Noob off); <c>Mic</c>
/// <see cref="Overlay.UiMicMode"/>. VOICE_CHAT.md §15: the voice chat and microphone volumes (1..100, 100 the default;
/// anything else reads as 100) and the chosen microphone and output by endpoint id (null: the system default), with
/// the name to say which one went missing.
/// </summary>
public sealed record VoiceSettings(bool? Enabled, byte Mic, byte VoiceVolume = Overlay.UiFrame.MaxVolume, byte MicVolume = Overlay.UiFrame.MaxVolume,
    string? MicId = null, string? MicName = null, string? OutputId = null, string? OutputName = null);

/// <summary>§15: the volume a profile hears another player at (1..99; 100 is not stored), and when it was set.</summary>
public sealed record PlayerVolume(byte Volume, DateTime SetAtUtc);

/// <summary>
/// <c>voice.json</c> in the launcher's state folder (OD-154): per profile XUID its <see cref="VoiceSettings"/>, its
/// mute history (target XUID → <see cref="VoiceMute"/>) and the volumes it set for other players (target XUID →
/// <see cref="PlayerVolume"/>, §15; no gamertag). Local only, never sent anywhere but as mute events (VC-5).
/// Written whole through a temporary file; a damaged file reads as empty (nobody muted, the mode's defaults).
/// </summary>
public sealed class VoiceStore(string stateDirectory)
{
    public const string FileName = "voice.json";

    /// <summary>The history keeps the newest mutes when it grows past this (a profile meets many players).</summary>
    public const int MaxMutes = 1000;

    /// <summary>§15: the newest per-player volumes kept, the same bound as the mutes.</summary>
    public const int MaxVolumes = 1000;

    private static readonly JsonSerializerOptions Indented = new() { WriteIndented = true };
    private readonly object _lock = new();

    private sealed record Profile(VoiceSettings? Settings, Dictionary<string, VoiceMute>? Mutes, Dictionary<string, PlayerVolume>? Volumes = null);

    private string FilePath => Path.Combine(stateDirectory, FileName);

    public VoiceSettings Settings(string profileXuid) => Read().TryGetValue(profileXuid, out var p) && p.Settings is { } s ? Valid(s) : new(null, 0);

    /// <summary>§15: the per-player volumes of a profile (only the valid ones: 1..99).</summary>
    public IReadOnlyDictionary<string, PlayerVolume> Volumes(string profileXuid) =>
        Read().TryGetValue(profileXuid, out var p) && p.Volumes is { } v
            ? v.Where(e => e.Value is { Volume: >= Overlay.UiFrame.MinVolume and < Overlay.UiFrame.MaxVolume }).ToDictionary(e => e.Key, e => e.Value)
            : new Dictionary<string, PlayerVolume>();

    /// <summary>§15: sets the volume a profile hears a player at; 100 (or null) forgets it.</summary>
    public void SetVolume(string profileXuid, string targetXuid, PlayerVolume? volume) => Change(profileXuid, p =>
    {
        var volumes = new Dictionary<string, PlayerVolume>(p.Volumes ?? []);
        if (volume is null || volume.Volume >= Overlay.UiFrame.MaxVolume) volumes.Remove(targetXuid);
        else volumes[targetXuid] = volume;
        if (volumes.Count > MaxVolumes)
            volumes = volumes.OrderByDescending(v => v.Value.SetAtUtc).Take(MaxVolumes).ToDictionary(v => v.Key, v => v.Value);
        return p with { Volumes = volumes };
    });

    // A hand-edited or damaged value reads as its default (§15): volumes 1..100, device ids and names within the pipe's
    // 256 bytes, and a name only with an id.
    private static VoiceSettings Valid(VoiceSettings s)
    {
        static byte Volume(byte v) => v is >= Overlay.UiFrame.MinVolume and <= Overlay.UiFrame.MaxVolume ? v : Overlay.UiFrame.MaxVolume;
        static (string?, string?) Device(string? id, string? name) =>
            string.IsNullOrEmpty(id) || System.Text.Encoding.UTF8.GetByteCount(id) > Overlay.UiFrame.MaxDeviceText ? (null, null)
            : (id, name is null ? null : CutUtf8(name, Overlay.UiFrame.MaxDeviceText));
        var (micId, micName) = Device(s.MicId, s.MicName);
        var (outId, outName) = Device(s.OutputId, s.OutputName);
        return s with { VoiceVolume = Volume(s.VoiceVolume), MicVolume = Volume(s.MicVolume), MicId = micId, MicName = micName, OutputId = outId, OutputName = outName };
    }

    private static string CutUtf8(string text, int maxBytes)
    {
        if (System.Text.Encoding.UTF8.GetByteCount(text) <= maxBytes) return text;
        var cut = text;
        while (cut.Length > 0 && System.Text.Encoding.UTF8.GetByteCount(cut) > maxBytes)
            cut = char.IsLowSurrogate(cut[^1]) && cut.Length > 1 ? cut[..^2] : cut[..^1];
        return cut;
    }

    public IReadOnlyDictionary<string, VoiceMute> Mutes(string profileXuid) =>
        Read().TryGetValue(profileXuid, out var p) && p.Mutes is { } m ? m : new Dictionary<string, VoiceMute>();

    public void SaveSettings(string profileXuid, VoiceSettings settings) => Change(profileXuid, p => p with { Settings = settings });

    /// <summary>Adds (or refreshes the gamertag of) a mute, or removes it when <paramref name="mute"/> is null.</summary>
    public void SetMute(string profileXuid, string targetXuid, VoiceMute? mute) => Change(profileXuid, p =>
    {
        var mutes = new Dictionary<string, VoiceMute>(p.Mutes ?? []);
        if (mute is null) mutes.Remove(targetXuid);
        else mutes[targetXuid] = mute;
        if (mutes.Count > MaxMutes)
            mutes = mutes.OrderByDescending(m => m.Value.MutedAtUtc).Take(MaxMutes).ToDictionary(m => m.Key, m => m.Value);
        return p with { Mutes = mutes };
    });

    private void Change(string profileXuid, Func<Profile, Profile> change)
    {
        lock (_lock)
        {
            var all = new Dictionary<string, Profile>(Read());
            all[profileXuid] = change(all.TryGetValue(profileXuid, out var p) ? p : new(null, null, null));
            Directory.CreateDirectory(stateDirectory);
            var temp = FilePath + ".tmp";
            File.WriteAllText(temp, JsonSerializer.Serialize(all, Indented));
            File.Move(temp, FilePath, overwrite: true);
        }
    }

    private Dictionary<string, Profile> Read()
    {
        try
        {
            return JsonSerializer.Deserialize<Dictionary<string, Profile>>(File.ReadAllText(FilePath)) ?? [];
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return [];
        }
    }
}
