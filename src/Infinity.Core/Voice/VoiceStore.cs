// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Voice;

/// <summary>A player this profile muted (VOICE_CHAT.md §4): kept on the PC and applied again in every later lobby.</summary>
public sealed record VoiceMute(string GamertagAtMute, DateTime MutedAtUtc, bool Sent);

/// <summary>One profile's voice settings: <c>Enabled</c> null follows the mode (Hardcore on, Noob off); <c>Mic</c> <see cref="Overlay.UiMicMode"/>.</summary>
public sealed record VoiceSettings(bool? Enabled, byte Mic);

/// <summary>
/// <c>voice.json</c> in the launcher's state folder (OD-147): per profile XUID its <see cref="VoiceSettings"/> and its
/// mute history (target XUID → <see cref="VoiceMute"/>). Local only, never sent anywhere but as mute events (VC-5).
/// Written whole through a temporary file; a damaged file reads as empty (nobody muted, the mode's defaults).
/// </summary>
public sealed class VoiceStore(string stateDirectory)
{
    public const string FileName = "voice.json";

    /// <summary>The history keeps the newest mutes when it grows past this (a profile meets many players).</summary>
    public const int MaxMutes = 1000;

    private static readonly JsonSerializerOptions Indented = new() { WriteIndented = true };
    private readonly object _lock = new();

    private sealed record Profile(VoiceSettings? Settings, Dictionary<string, VoiceMute>? Mutes);

    private string FilePath => Path.Combine(stateDirectory, FileName);

    public VoiceSettings Settings(string profileXuid) => Read().TryGetValue(profileXuid, out var p) && p.Settings is { } s ? s : new(null, 0);

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
            all[profileXuid] = change(all.TryGetValue(profileXuid, out var p) ? p : new(null, null));
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
