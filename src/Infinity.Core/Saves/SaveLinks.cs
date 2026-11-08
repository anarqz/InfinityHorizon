// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Saves;

/// <summary>Which Azimuth profile an Infinity profile was cloned from (OD-41).</summary>
public sealed record SaveLink(string Source, ulong SourceRevision, ulong Revision, DateTime ClonedAtUtc);

/// <summary>
/// <c>save-links.json</c> in the launcher's state folder: XUID (decimal string) → <see cref="SaveLink"/>.
/// Written whole through a temporary file; a damaged file reads as empty (it only labels the UI). CR-S98: a XUID names a career per game,
/// so another game's link is keyed <c>&lt;game&gt;:&lt;xuid&gt;</c>; FH2's keep the bare XUID every earlier launcher wrote.
/// </summary>
public sealed class SaveLinks(string stateDirectory, string game = SaveSyncStore.DefaultGame)
{
    public const string FileName = "save-links.json";

    private string Key(string xuid) => game == SaveSyncStore.DefaultGame ? xuid : game + ":" + xuid;

    private static readonly JsonSerializerOptions Indented = new() { WriteIndented = true };

    private string FilePath => Path.Combine(stateDirectory, FileName);

    public IReadOnlyDictionary<string, SaveLink> Read()
    {
        try
        {
            return JsonSerializer.Deserialize<Dictionary<string, SaveLink>>(File.ReadAllText(FilePath)) ?? [];
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return new Dictionary<string, SaveLink>();
        }
    }

    public SaveLink? For(string xuid) => Read().TryGetValue(Key(xuid), out var link) ? link : null;

    public void Set(string xuid, SaveLink link)
    {
        var all = new Dictionary<string, SaveLink>(Read()) { [Key(xuid)] = link };
        Directory.CreateDirectory(stateDirectory);
        var temp = FilePath + ".tmp";
        File.WriteAllText(temp, JsonSerializer.Serialize(all, Indented));
        File.Move(temp, FilePath, overwrite: true);
    }
}
