// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;
using Infinity.Core.Api;
using Infinity.Core.Saves;

namespace Infinity.Core.Shell;

/// <summary>
/// The account profile a local profile became (or a cached account profile): stored as soon as the server answers.
/// <see cref="RenameNotice"/>: the server's OD-120 clash-rename notice, while its free rename is unused.
/// </summary>
public sealed record CloudLink(string AccountId, string ProfileId, string Xuid, string Gamertag, RenameNotice? RenameNotice = null);

/// <summary>
/// OD-118: a profile kept on this PC. Created without an account (the same screen as before, no cloud call); uploaded to the
/// account after sign-in with <see cref="UploadKey"/> as its <c>Idempotency-Key</c>; or a read-through copy of an account
/// profile (from GET /me) so it shows while signed out. <see cref="OfflineXuid"/> is its own offline identity (CR-L36 range),
/// the save folder <c>User_&lt;hex&gt;</c> the game uses when it plays offline.
/// </summary>
public sealed record LocalProfile(string Id, string Game, string Name, string Mode, string UploadKey, string CreatedAt, ulong OfflineXuid,
    CloudLink? Cloud = null, bool NotOnAccount = false)
{
    public bool Hardcore => Mode == "hardcore";

    /// <summary>Uploaded to (or cached from) <paramref name="accountId"/> and still listed by it: playable online under that account.</summary>
    public bool OnAccount(string? accountId) => accountId is not null && Cloud is { } cloud && cloud.AccountId == accountId && !NotOnAccount;

    /// <summary>Never uploaded: the update panel may upload it. A profile of another account is never uploaded under this one.</summary>
    public bool LocalOnly => Cloud is null;

    /// <summary>
    /// The identity an OFFLINE session plays (P1/P4): a Noob profile that is on the account continues the account XUID's save
    /// (cloned from its local save at upload); Hardcore, and every local-only profile, play its own local offline XUID.
    /// </summary>
    public ulong OfflinePlayXuid => !Hardcore && Cloud is { } cloud && !NotOnAccount
        && ulong.TryParse(cloud.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) ? xuid : OfflineXuid;

    /// <summary>The name shown (OD-120: plain, never a cached legacy <c>#NNNNN</c>).</summary>
    public string DisplayName => Cloud is { } cloud ? ProfileName.Plain(cloud.Gamertag) : Name;
}

/// <summary>
/// <c>local-profiles.json</c> in the launcher's state folder. At most <see cref="MaxProfiles"/> per game are created on this
/// launcher; account profiles cached from another PC may show beside them. Written whole through a temporary file.
/// </summary>
public sealed class LocalProfileStore(string stateDirectory)
{
    public const string FileName = "local-profiles.json";
    public const int MaxProfiles = 3;
    /// <summary>CR-L36 (ACCOUNTS.md): local offline profiles, never issued or accepted by the server.</summary>
    public const ulong FirstOfflineXuid = 0x9000000000001, LastOfflineXuid = 0x90000000000FF;

    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web) { WriteIndented = true, DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull };
    private sealed record Document(int SchemaVersion, List<LocalProfile> Profiles);
    private string Path_ => Path.Combine(stateDirectory, FileName);

    /// <summary>The profiles, newest last; a missing or unreadable file is none (never a crash: it only lists profiles).</summary>
    public IReadOnlyList<LocalProfile> Read()
    {
        try { return Load(); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { return []; }
    }

    /// <summary>
    /// The profiles for a change: an unreadable or foreign file throws <see cref="IOException"/> instead of reading as empty, so a change
    /// never overwrites profiles (and their offline XUIDs) it could not read.
    /// </summary>
    private List<LocalProfile> Load()
    {
        if (!File.Exists(Path_)) return [];
        if (new FileInfo(Path_).Length > 256 * 1024) throw new IOException("local-profiles.json is too large");
        try
        {
            var document = JsonSerializer.Deserialize<Document>(File.ReadAllBytes(Path_), Json);
            return document is { SchemaVersion: 1, Profiles: { } list } ? list.Where(Valid).ToList() : throw new IOException("local-profiles.json is not this launcher's");
        }
        catch (JsonException error) { throw new IOException("local-profiles.json is damaged", error); }
    }

    private static bool Valid(LocalProfile? p) => p is { Id.Length: > 0, Game.Length: > 0, Name.Length: > 0, UploadKey.Length: > 0 }
        && p.Mode is "hardcore" or "noob" && p.OfflineXuid is >= FirstOfflineXuid and <= LastOfflineXuid;

    private void Write(IEnumerable<LocalProfile> profiles)
    {
        Directory.CreateDirectory(stateDirectory);
        var bytes = JsonSerializer.SerializeToUtf8Bytes(new Document(1, profiles.ToList()), Json);
        var staging = Path_ + "." + Guid.NewGuid().ToString("N") + ".tmp";
        File.WriteAllBytes(staging, bytes);
        File.Move(staging, Path_, overwrite: true);
    }

    /// <summary>
    /// A new local profile (OD-118: no cloud call). Its offline XUID is the lowest one in the CR-L36 range that no profile uses and
    /// whose save folder does not exist (<paramref name="saveFolderExists"/>, never reusing a folder another identity left behind).
    /// </summary>
    public LocalProfile Create(string game, string name, string mode, Func<string, bool> saveFolderExists, DateTimeOffset now)
    {
        ArgumentNullException.ThrowIfNull(saveFolderExists);
        if (!ProfileName.IsValid(name)) throw new LocalProfileException(ErrorCodes.AccountsInvalidName);
        if (mode is not ("hardcore" or "noob")) throw new ArgumentException("unknown mode", nameof(mode));
        var all = Load();
        if (all.Count(p => p.Game == game && p.LocalOnly) + all.Count(p => p.Game == game && !p.LocalOnly && !p.NotOnAccount) >= MaxProfiles)
            throw new LocalProfileException(ErrorCodes.AccountsProfileLimit);
        if (all.Any(p => p.Game == game && string.Equals(p.Name, name, StringComparison.OrdinalIgnoreCase)))
            throw new LocalProfileException(ErrorCodes.AccountsNameUnavailable);
        var profile = new LocalProfile(Guid.NewGuid().ToString(), game, name, mode, Guid.NewGuid().ToString(),
            now.UtcDateTime.ToString("O", CultureInfo.InvariantCulture), FreeOfflineXuid(all, saveFolderExists));
        all.Add(profile);
        Write(all);
        return profile;
    }

    private static ulong FreeOfflineXuid(IReadOnlyCollection<LocalProfile> all, Func<string, bool> saveFolderExists)
    {
        for (var xuid = FirstOfflineXuid; xuid <= LastOfflineXuid; xuid++)
            if (all.All(p => p.OfflineXuid != xuid) && !saveFolderExists(AzimuthSaves.FolderFor(xuid))) return xuid;
        throw new LocalProfileException(ErrorCodes.AccountsProfileLimit);
    }

    /// <summary>
    /// Renames or changes the mode of a profile that was never uploaded. A new upload key is minted: the server replays a key's
    /// first outcome, and the same key with another body is refused (REQUEST.IDEMPOTENCY_KEY_REUSED).
    /// </summary>
    public LocalProfile Edit(string id, string name, string mode)
    {
        if (!ProfileName.IsValid(name)) throw new LocalProfileException(ErrorCodes.AccountsInvalidName);
        var all = Load();
        var index = all.FindIndex(p => p.Id == id);
        if (index < 0 || !all[index].LocalOnly) throw new InvalidOperationException("only a local profile that was never uploaded can be edited");
        if (all.Any(p => p.Id != id && p.Game == all[index].Game && string.Equals(p.Name, name, StringComparison.OrdinalIgnoreCase)))
            throw new LocalProfileException(ErrorCodes.AccountsNameUnavailable);
        var edited = all[index] with { Name = name, Mode = mode, UploadKey = all[index].Name == name && all[index].Mode == mode ? all[index].UploadKey : Guid.NewGuid().ToString() };
        all[index] = edited;
        Write(all);
        return edited;
    }

    /// <summary>Removes a profile from this launcher's list; its save folder is never touched.</summary>
    public void Remove(string id) => Write(Read().Where(p => p.Id != id));

    /// <summary>Stores the server's answer for an uploaded profile at once (its key's replay record is not kept forever).</summary>
    public LocalProfile Link(string id, CloudLink cloud)
    {
        var all = Load();
        var index = all.FindIndex(p => p.Id == id);
        if (index < 0) throw new InvalidOperationException("unknown local profile");
        all[index] = all[index] with { Cloud = cloud, NotOnAccount = false };
        Write(all);
        return all[index];
    }

    /// <summary>
    /// On every sign-in (P3): the account's profiles from GET /me. Cached ones it still lists are refreshed, their name included
    /// (OD-120: the server's plain name, also the name they play offline); ones it no longer lists (deleted) are marked not on this
    /// account, never removed with their save; account profiles made on another PC are added as cached entries with their own
    /// offline XUID. Profiles of another account stay as they are, never this account's.
    /// </summary>
    public IReadOnlyList<LocalProfile> RefreshFromAccount(Me me, string game, Func<string, bool> saveFolderExists)
    {
        ArgumentNullException.ThrowIfNull(me);
        var all = Load();
        var listed = me.Profiles.Where(p => p.Game == game).ToList();
        for (var i = 0; i < all.Count; i++)
        {
            // Only this game's profiles are compared with this game's list (another game's are not "not on this account").
            if (all[i].Game != game || all[i].Cloud is not { } cloud || cloud.AccountId != me.AccountId) continue;
            all[i] = listed.FirstOrDefault(p => p.ProfileId == cloud.ProfileId) is { } current ? Refreshed(all[i], cloud, current) : all[i] with { NotOnAccount = true };
        }
        foreach (var profile in listed.Where(p => all.All(l => l.Cloud?.ProfileId != p.ProfileId)))
        {
            var name = PlayName(profile);
            var link = new CloudLink(me.AccountId, profile.ProfileId, profile.Xuid, profile.Gamertag, profile.RenameNotice);
            // An upload whose answer was lost: the local profile it came from is linked, not shadowed by a second cached copy.
            var origin = all.FindIndex(l => l.LocalOnly && l.Game == game && l.Mode == profile.Mode && string.Equals(l.Name, name, StringComparison.OrdinalIgnoreCase));
            if (origin >= 0) { all[origin] = all[origin] with { Cloud = link, NotOnAccount = false }; continue; }
            all.Add(new LocalProfile(Guid.NewGuid().ToString(), game, name, profile.Mode, Guid.NewGuid().ToString(), profile.CreatedAt,
                FreeOfflineXuid(all, saveFolderExists), link));
        }
        Write(all);
        return all;
    }

    /// <summary>
    /// One account profile as the server answered it now (a free rename's 200 <c>Profile</c>): its cached copy takes the new name and
    /// notice at once, without waiting for the next GET /me. Nothing happens when this launcher holds no copy of it.
    /// </summary>
    public LocalProfile? ApplyAccountProfile(string accountId, Profile current)
    {
        ArgumentNullException.ThrowIfNull(current);
        var all = Load();
        var index = all.FindIndex(p => p.Cloud is { } cloud && cloud.AccountId == accountId && cloud.ProfileId == current.ProfileId);
        if (index < 0) return null;
        all[index] = Refreshed(all[index], all[index].Cloud!, current);
        Write(all);
        return all[index];
    }

    private static LocalProfile Refreshed(LocalProfile local, CloudLink cloud, Profile current) => local with
    {
        Cloud = cloud with { Xuid = current.Xuid, Gamertag = current.Gamertag, RenameNotice = current.RenameNotice },
        Name = PlayName(current), Mode = current.Mode, NotOnAccount = false,
    };

    /// <summary>
    /// The name an account profile plays under offline (FI_OFFLINE_GAMERTAG): its plain name (a legacy <c>Name#12345</c> without the
    /// number; the handle is never a profile name), kept to the profile-name rule the hook checks (1-20 letters or digits, a letter first).
    /// </summary>
    internal static string PlayName(Profile profile) => ProfileName.Playable(profile.Gamertag);
}

/// <summary>A local refusal with the account plane's own code, so the launcher shows the same text as for the server's.</summary>
public sealed class LocalProfileException(string code) : Exception(code)
{
    public string Code { get; } = code;
}
