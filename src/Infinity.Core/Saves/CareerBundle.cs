// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.Saves;

/// <summary>
/// A game's cloud save bundle on this PC (catalog data, <see cref="SaveLayout.Bundle"/>; CR-S105): how a career on disk is packed and
/// digested, how a downloaded bundle is checked, and how it is installed. FH2: one revision as FH2SAVE1 (<see cref="Fh2CareerBundle"/>);
/// FM5: the profile's revision plus its garage's sibling containers as FM5SAVE1 (<see cref="Fm5CareerBundle"/>). The save flow
/// (<see cref="CloudSaves"/>, <see cref="SessionSaves"/>) is the same for both.
/// </summary>
public interface ICareerBundle
{
    /// <summary>The bundle's <c>Content-Type</c>.</summary>
    string MediaType { get; }

    /// <summary>The largest bundle of the format.</summary>
    int MaxBundle { get; }

    /// <summary>Reads (verified against each commit), packs and digests the career whose profile revision is <paramref name="revision"/>.</summary>
    PackedRevision Pack(LocalRevision revision);

    /// <summary>The content digest of the career whose profile revision is <paramref name="revision"/> (no packing).</summary>
    string ContentDigest(LocalRevision revision);

    /// <summary>Every check of a downloaded bundle; throws <see cref="BundleException"/>.</summary>
    ValidBundle Validate(ReadOnlyMemory<byte> data);

    /// <summary>
    /// Installs a validated bundle as <paramref name="xuid"/>'s newest revision (what it shadows archived as <paramref name="label"/>);
    /// throws <see cref="SaveException"/> (with <see cref="SaveException.Landed"/> when the profile's revision landed but its final check failed).
    /// </summary>
    InstalledRevision Install(string savesRoot, ulong xuid, ValidBundle bundle, string expectedContent, string stagingId, string label, Action<string>? afterMove);

    /// <summary>
    /// Every container folder of <c>player-1</c> that <see cref="Install"/> of <paramref name="bundle"/> onto <paramref name="xuid"/> may
    /// write, the profile's <c>User_&lt;hex&gt;</c> first (what a caller backs up before the install).
    /// </summary>
    IReadOnlyList<string> Containers(ValidBundle bundle, ulong xuid);
}

/// <summary>FH2SAVE1: the existing <see cref="CloudSaveFiles"/> and <see cref="Fh2Bundle"/> calls, unchanged.</summary>
public sealed class Fh2CareerBundle : ICareerBundle
{
    public static Fh2CareerBundle Instance { get; } = new();

    public string MediaType => Fh2Bundle.MediaType;

    public int MaxBundle => Fh2Bundle.MaxBundle;

    public PackedRevision Pack(LocalRevision revision) => CloudSaveFiles.Pack(revision);

    public string ContentDigest(LocalRevision revision) => Fh2Bundle.ContentDigest(CloudSaveFiles.ReadFiles(revision));

    public ValidBundle Validate(ReadOnlyMemory<byte> data) => Fh2Bundle.Validate(data);

    public InstalledRevision Install(string savesRoot, ulong xuid, ValidBundle bundle, string expectedContent, string stagingId, string label, Action<string>? afterMove)
    {
        ArgumentNullException.ThrowIfNull(bundle);
        return CloudSaveFiles.Install(savesRoot, xuid, bundle.Files, expectedContent, stagingId, label, afterMove);
    }

    /// <summary>FH2SAVE1 is the profile's folder only.</summary>
    public IReadOnlyList<string> Containers(ValidBundle bundle, ulong xuid) => [AzimuthSaves.FolderFor(xuid)];
}

/// <summary>
/// FM5SAVE1 on Azimuth's folders (CR-S105, SAVES_API.md §4.12 "What the launcher does"; fm5-save-format.md §1-§3):
/// <list type="bullet">
/// <item>Pack: the profile's newest sealed <c>User_&lt;hex&gt;</c> revision as <see cref="Fm5Bundle.User"/>, plus the newest sealed revision of
/// every <c>BaseLivery_</c>/<c>GarageThumbnail_</c> container its <c>Career_Garage</c> rows name (read from the very PlayerDatabase bytes packed),
/// ascending by name. Never <c>Replay_*</c>. Each container's commit must list exactly its kind's files.</item>
/// <item>Install: each sibling under its own name, skipped when its newest revision holds the same files, else as its <c>highest + 1</c>; then
/// the profile into the TARGET's own <c>User_&lt;hex&gt;\revisions\&lt;highest + 1&gt;</c> (never the source's XUID folder). Nothing else is
/// touched; what an install shadows is copied to the archive first; a failure before the profile lands moves this call's sibling revisions to
/// the archive again.</item>
/// </list>
/// </summary>
public sealed class Fm5CareerBundle(SaveLayout layout) : ICareerBundle
{
    private readonly SaveLayout _layout = layout ?? throw new ArgumentNullException(nameof(layout));

    public string MediaType => Fm5Bundle.MediaType;

    public int MaxBundle => Fm5Bundle.MaxBundle;

    /// <summary>
    /// One container's revision as its kind's files: the commit as stored, then each data file it lists in <paramref name="names"/> order. The
    /// commit must list exactly those files and each must match its line on the bytes read (never packed torn).
    /// </summary>
    public static IReadOnlyList<SaveFile> ReadContainer(LocalRevision revision, IReadOnlyList<string> names)
    {
        ArgumentNullException.ThrowIfNull(revision);
        ArgumentNullException.ThrowIfNull(names);
        var commit = File.ReadAllBytes(Path.Combine(revision.Path, "commit"));
        var entries = AzimuthSaves.ParseCommit(new System.Text.UTF8Encoding(false, true).GetString(commit))
            ?? throw new SaveException("the revision's commit file does not parse any more");
        // T-1110-FM5TLF: a profile revision without its TransactionLogFile reads as the profile's other files (the entry omitted)
        if (names.Contains(Fm5Bundle.OptionalUserFile, StringComparer.Ordinal) && entries.All(e => e.Name != Fm5Bundle.OptionalUserFile[5..]))
            names = Fm5Bundle.FilesUserWithoutLog;
        var want = names.Skip(1).Select(n => n[5..]).ToList();
        if (entries.FirstOrDefault(e => !want.Contains(e.Name, StringComparer.Ordinal)) is { } unknown) throw new SaveException(unknown.Name + " is not a save file of this container");
        if (want.FirstOrDefault(n => entries.All(e => e.Name != n)) is { } missing) throw new SaveException("the revision has no " + missing);
        var files = new List<SaveFile> { new("commit", commit) };
        foreach (var name in names.Skip(1))
        {
            var entry = entries.First(e => e.Name == name[5..]);
            var bytes = File.ReadAllBytes(Path.Combine(revision.Path, "data", entry.Name));
            if (Fh2Bundle.CommitLine(name, bytes) != string.Create(CultureInfo.InvariantCulture, $"{entry.Sha256} {entry.Size} {entry.Name}"))
                throw new SaveException(entry.Name + " does not match its commit line (the revision is not sealed)");
            files.Add(new SaveFile(name, bytes));
        }
        return files;
    }

    /// <summary><c>…\player-1</c> of a profile revision (<c>…\player-1\User_x\revisions\&lt;16 hex&gt;</c>).</summary>
    private static string PlayerDirOf(LocalRevision revision) =>
        Path.GetDirectoryName(Path.GetDirectoryName(Path.GetDirectoryName(Path.TrimEndingDirectorySeparator(revision.Path))))
            ?? throw new SaveException("the revision is not inside a player folder");

    /// <summary>
    /// The career as containers, bundle order: the profile, then each sibling its garage names that is sealed on this PC (ascending by
    /// byte); <paramref name="missing"/> the named ones that are not. More than <see cref="Fm5Bundle.MaxSiblings"/> throws.
    /// </summary>
    public IReadOnlyList<Fm5Container> ReadCareer(LocalRevision revision, out IReadOnlyList<string> missing)
    {
        ArgumentNullException.ThrowIfNull(revision);
        var user = ReadContainer(revision, Fm5Bundle.FilesUser);
        var references = _layout.References is { } refs ? References(user.First(f => f.Name == "data/" + refs.File).Bytes) : new HashSet<string>();
        var names = references.Where(n => Fm5Bundle.KindOfSibling(n) is not null).Order(StringComparer.Ordinal).ToList();
        if (names.Count > Fm5Bundle.MaxSiblings) throw new SaveException($"the career names {names.Count} liveries and thumbnails, over the {Fm5Bundle.MaxSiblings} limit");
        var player = PlayerDirOf(revision);
        var containers = new List<Fm5Container> { new(Fm5Bundle.User, revision.Number, user) };
        var absent = new List<string>();
        foreach (var name in names)
        {
            if (AzimuthSaves.NewestSealed(Path.Combine(player, name)) is not { } newest) { absent.Add(name); continue; }
            containers.Add(new Fm5Container(name, newest.Number, ReadContainer(newest, Fm5Bundle.FilesOf(Fm5Bundle.KindOfSibling(name)!.Value))));
        }
        missing = absent;
        return containers;
    }

    // the names a database holds depend on its bytes only: read once per database (the cards' chips digest the career on every render)
    private readonly System.Collections.Concurrent.ConcurrentDictionary<string, IReadOnlySet<string>> _references = new(StringComparer.Ordinal);

    private IReadOnlySet<string> References(byte[] database)
    {
        var key = Convert.ToHexStringLower(System.Security.Cryptography.SHA256.HashData(database));
        if (_references.TryGetValue(key, out var known)) return known;
        var names = SaveReferences.ReferencedSiblings(database, _layout);
        if (_references.Count >= 32) _references.Clear();
        _references[key] = names;
        return names;
    }

    public PackedRevision Pack(LocalRevision revision)
    {
        var containers = ReadCareer(revision, out var missing);
        var bundle = Fm5Bundle.Pack(containers);
        if (bundle.Length > Fm5Bundle.MaxBundle) throw new SaveException($"the career packs to {bundle.Length} bytes, over the {Fm5Bundle.MaxBundle} limit");
        // the server's own checks, here first: a career it would refuse (a size outside its windows, a commit that lists another file) is never sent
        ValidBundle valid;
        try { valid = Fm5Bundle.Validate(bundle); }
        catch (BundleException e) { throw new SaveException("the career is not a valid FM5 save: " + e.Message); }
        return new PackedRevision(revision.Number, containers[0].Files, bundle, valid.ContentSha256) { Containers = containers, MissingSiblings = missing };
    }

    public string ContentDigest(LocalRevision revision) => Fm5Bundle.ContentDigest(ReadCareer(revision, out _));

    public ValidBundle Validate(ReadOnlyMemory<byte> data) => Fm5Bundle.Validate(data);

    public InstalledRevision Install(string savesRoot, ulong xuid, ValidBundle bundle, string expectedContent, string stagingId, string label, Action<string>? afterMove)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(bundle);
        var containers = bundle.Containers ?? throw new SaveException("the downloaded save is not an FM5 career");
        if (containers.Count == 0 || containers[0].Name != Fm5Bundle.User) throw new SaveException("the downloaded career has no profile");
        if (!string.Equals(Fm5Bundle.ContentDigest(containers), expectedContent, StringComparison.OrdinalIgnoreCase))
            throw new SaveException("the downloaded save does not match the cloud head's digest");
        var player = Path.Combine(savesRoot, _layout.PlayerDir);
        var archived = new List<string>();
        var installed = new List<(string Name, string Path, ulong Number)>();
        var skipped = new List<string>();
        try
        {
            foreach (var c in containers.Skip(1))
            {
                var kind = Fm5Bundle.KindOfSibling(c.Name) ?? throw new SaveException("invalid save container " + c.Name);
                var dir = Path.Combine(player, c.Name);
                var mine = Fm5Bundle.ContainerDigest(c.Name, c.Files);
                if (Same(dir, c.Name, kind, mine)) { skipped.Add(c.Name); continue; }
                var done = CloudSaveFiles.InstallContainer(savesRoot, dir, c.Name, c.Files, mine, files => Fm5Bundle.ContainerDigest(c.Name, files),
                    r => ReadContainer(r, Fm5Bundle.FilesOf(kind)), stagingId, label, null);
                installed.Add((c.Name, done.Path, done.Number));
                archived.AddRange(done.Archived);
            }
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
        {
            // a sibling that landed but did not read back goes to the archive with the others
            if (e is SaveException { Landed: true, LandedPath: { } landed } && Path.GetFileName(Path.GetDirectoryName(Path.GetDirectoryName(landed))) is { } name)
                installed.Add((name, landed, ulong.TryParse(Path.GetFileName(landed), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out var n) ? n : 0));
            RollBack(savesRoot, installed, label);
            if (e is SaveException { Landed: true } s) throw new SaveException(s.Message);
            throw e as SaveException ?? new SaveException(e.Message);
        }
        InstalledRevision profile;
        try
        {
            profile = CloudSaveFiles.InstallContainer(savesRoot, _layout.ProfileDir(savesRoot, xuid), _layout.UserFolder(xuid), containers[0].Files,
                Fm5Bundle.ContainerDigest(Fm5Bundle.User, containers[0].Files), files => Fm5Bundle.ContainerDigest(Fm5Bundle.User, files),
                r => ReadContainer(r, Fm5Bundle.FilesUser), stagingId, label, afterMove);
        }
        catch (Exception e) when (e is SaveException { Landed: false } or IOException or UnauthorizedAccessException)
        {
            // the profile did not land: the liveries and thumbnails this call added go back to the archive (the PC is as it was)
            RollBack(savesRoot, installed, label);
            throw e as SaveException ?? new SaveException(e.Message);
        }
        return new InstalledRevision(profile.Number, profile.Path, [.. archived, .. profile.Archived])
        {
            Siblings = [.. installed.Select(i => i.Name)],
            SkippedSiblings = skipped,
        };
    }

    /// <summary>The profile's <c>User_&lt;hex&gt;</c>, then each sibling the bundle carries (bundle order).</summary>
    public IReadOnlyList<string> Containers(ValidBundle bundle, ulong xuid)
    {
        ArgumentNullException.ThrowIfNull(bundle);
        return [_layout.UserFolder(xuid), .. (bundle.Containers ?? []).Skip(1).Select(c => c.Name)];
    }

    /// <summary>The container's newest sealed revision holds exactly these files (a container that does not read as its kind is not the same).</summary>
    private static bool Same(string dir, string name, Fm5Kind kind, string digest)
    {
        try
        {
            return AzimuthSaves.NewestSealed(dir) is { } newest && Fm5Bundle.ContainerDigest(name, ReadContainer(newest, Fm5Bundle.FilesOf(kind))) == digest;
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            return false;
        }
    }

    /// <summary>Moves this call's sibling revisions to the archive (<c>rolled-back-&lt;label&gt;</c>); never deletes. Best effort.</summary>
    private static void RollBack(string savesRoot, IEnumerable<(string Name, string Path, ulong Number)> installed, string label)
    {
        foreach (var (name, path, number) in installed)
        {
            try
            {
                var slot = AzimuthSaves.ArchiveSlot(savesRoot, name, "rolled-back-" + label, number);
                Directory.CreateDirectory(Path.GetDirectoryName(slot)!);
                Directory.Move(path, slot);
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
            }
        }
    }
}
