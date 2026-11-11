// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>The kind of an FM5SAVE1 container, from its name (<c>fi_saves::fm5::Kind</c>).</summary>
public enum Fm5Kind
{
    /// <summary>The profile container (<c>User_&lt;XUID hex&gt;</c> on disk, <see cref="Fm5Bundle.User"/> in a bundle).</summary>
    User,

    /// <summary><c>BaseLivery_&lt;carId&gt;_&lt;yyyymmddhhmmss&gt;</c>.</summary>
    Livery,

    /// <summary><c>GarageThumbnail_&lt;carId&gt;_&lt;yyyymmddhhmmss&gt;</c>.</summary>
    Thumbnail,
}

/// <summary>
/// One container of an FM5 career, unpacked: <see cref="Fm5Bundle.User"/> or a sibling folder's name, its revision label (Azimuth's revision
/// folder name read as hex) and its files in its kind's order (<c>commit</c> first, then <c>data/&lt;name&gt;</c>).
/// </summary>
public sealed record Fm5Container(string Name, ulong Label, IReadOnlyList<SaveFile> Files)
{
    public Fm5Kind Kind => Name == Fm5Bundle.User ? Fm5Kind.User : Fm5Bundle.KindOfSibling(Name) ?? throw new BundleException(BundleFault.InvalidContainer, Name, "invalid save container " + Name);

    public byte[]? File(string name) => Files.FirstOrDefault(f => f.Name == name)?.Bytes;
}

/// <summary>
/// The FM5SAVE1 bundle, byte for byte <c>crates/fi-saves/src/fm5.rs</c> (CR-S105, SAVES_API.md §4.12; the format:
/// <c>deprecated/beta/notes/local-docs/re/fm5-save-format.md</c>): one Forza Motorsport 5 career, the profile's newest sealed revision and the
/// newest sealed revision of each sibling container its garage names.
/// <para>
/// Layout (little endian): <c>"FM5SAVE1"</c>, <c>u32</c> container count (1 to 401); per container <c>u16</c> name length, name (first
/// <c>User</c>, never the XUID; then sibling names strictly ascending by byte), <c>u64</c> label, <c>u8</c> file count (exactly its kind's; a profile without its TransactionLogFile omits that entry), then
/// per file in its kind's order <c>u16</c> name length, name, <c>u32</c> raw size, <c>u32</c> stored size, <c>u8</c> method (0 stored, 2 the
/// hook's LZ), stored bytes. <see cref="Pack"/> writes method 2 where smaller, else stored, deterministically, as <c>fm5::pack</c>.
/// </para>
/// </summary>
public static class Fm5Bundle
{
    /// <summary><c>fi_protocol::headers::FM5SAVE1_MEDIA_TYPE</c>.</summary>
    public const string MediaType = "application/x-fm5save1";

    /// <summary>The profile container's name inside a bundle.</summary>
    public const string User = "User";

    public const string LiveryPrefix = "BaseLivery_";
    public const string ThumbnailPrefix = "GarageThumbnail_";

    /// <summary>Most sibling containers in one bundle (200 cars).</summary>
    public const int MaxSiblings = 400;

    /// <summary>Most containers in one bundle.</summary>
    public const int MaxContainers = 1 + MaxSiblings;

    /// <summary>Largest bundle accepted: 48 MiB.</summary>
    public const int MaxBundle = 48 << 20;

    /// <summary>Most bytes unpacked in one bundle: 64 MiB.</summary>
    public const long MaxUnpacked = 64L << 20;

    public static ReadOnlySpan<byte> Magic => "FM5SAVE1"u8;

    private static ReadOnlySpan<byte> Png => [0x89, (byte)'P', (byte)'N', (byte)'G', (byte)'\r', (byte)'\n', 0x1a, (byte)'\n'];

    public static IReadOnlyList<string> FilesUser { get; } = ["commit", "data/PlayerDatabase", "data/ProfileData", "data/TransactionLogFile", "data/VersionFlags"];

    /// <summary>
    /// Owner 2026-11-10 (T-1110-FM5TLF, "add whatever they send us; FM5 is still experimental"): FM5 writes profile revisions without a
    /// TransactionLogFile (<c>blobs 3</c>: PlayerDatabase, ProfileData, VersionFlags). It is the only optional file: a profile container
    /// holds <see cref="FilesUser"/>, or <see cref="FilesUserWithoutLog"/> (the entry omitted, never invented; file count 4); every other
    /// file stays required, in the same order, with the same size windows and hashes.
    /// </summary>
    public const string OptionalUserFile = "data/TransactionLogFile";

    /// <summary><see cref="FilesUser"/> without <see cref="OptionalUserFile"/>.</summary>
    public static IReadOnlyList<string> FilesUserWithoutLog { get; } = ["commit", "data/PlayerDatabase", "data/ProfileData", "data/VersionFlags"];

    /// <summary>The files a container of <paramref name="kind"/> with <paramref name="count"/> files holds, in bundle order; null when no shape has that count.</summary>
    public static IReadOnlyList<string>? FilesOf(Fm5Kind kind, int count) =>
        FilesOf(kind) is var all && all.Count == count ? all
        : kind == Fm5Kind.User && count == FilesUserWithoutLog.Count ? FilesUserWithoutLog : null;
    public static IReadOnlyList<string> FilesLivery { get; } = ["commit", "data/C_livery", "data/header"];
    public static IReadOnlyList<string> FilesThumbnail { get; } = ["commit", "data/bigThumb.png", "data/smallThumb.png"];

    /// <summary>(name, smallest, largest) unpacked size of each file (<c>fm5::FILE_SIZES</c>).</summary>
    public static IReadOnlyList<(string Name, uint Min, uint Max)> FileSizes { get; } =
    [
        ("commit", 64, 2048),
        ("data/PlayerDatabase", 1 << 20, 8 << 20),
        ("data/ProfileData", 4 << 10, 64 << 10),
        ("data/TransactionLogFile", 8, 64 << 10),
        ("data/VersionFlags", 4, 4),
        ("data/C_livery", 1, 256 << 10),
        ("data/header", 1, 4 << 10),
        ("data/bigThumb.png", 1 << 10, 512 << 10),
        ("data/smallThumb.png", 256, 128 << 10),
    ];

    /// <summary>A container kind's files, in bundle order.</summary>
    public static IReadOnlyList<string> FilesOf(Fm5Kind kind) => kind switch
    {
        Fm5Kind.User => FilesUser,
        Fm5Kind.Livery => FilesLivery,
        _ => FilesThumbnail,
    };

    /// <summary>A sibling container's kind (<c>&lt;prefix&gt;&lt;carId 1-10 digits&gt;_&lt;14 digits&gt;</c>), null for any other name (the profile's, a replay's).</summary>
    public static Fm5Kind? KindOfSibling(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        Fm5Kind kind;
        string rest;
        if (name.StartsWith(LiveryPrefix, StringComparison.Ordinal)) (kind, rest) = (Fm5Kind.Livery, name[LiveryPrefix.Length..]);
        else if (name.StartsWith(ThumbnailPrefix, StringComparison.Ordinal)) (kind, rest) = (Fm5Kind.Thumbnail, name[ThumbnailPrefix.Length..]);
        else return null;
        var split = rest.IndexOf('_', StringComparison.Ordinal);
        if (split < 0) return null;
        var (car, stamp) = (rest[..split], rest[(split + 1)..]);
        static bool Digits(string s) => s.All(char.IsAsciiDigit);
        return car.Length is >= 1 and <= 10 && Digits(car) && stamp.Length == 14 && Digits(stamp) ? kind : null;
    }

    private sealed record Entry(string Name, uint RawSize, byte Method, ReadOnlyMemory<byte> Stored);

    private sealed record Layout(string Name, Fm5Kind Kind, ulong Label, IReadOnlyList<Entry> Entries);

    private static string Shown(ReadOnlySpan<byte> raw) => new(Encoding.UTF8.GetString(raw).Take(40).ToArray());

    /// <summary>Parses and checks the bundle's shape without unpacking (<c>fm5::parse_layout</c>).</summary>
    private static List<Layout> ParseLayout(ReadOnlyMemory<byte> data)
    {
        if (data.Length > MaxBundle) throw new BundleException(BundleFault.TooLarge, null, "save too big");
        var at = 0;
        ReadOnlyMemory<byte> Take(int n)
        {
            if (n < 0 || n > data.Length - at) throw new BundleException(BundleFault.Truncated, null, "truncated bundle");
            var slice = data.Slice(at, n);
            at += n;
            return slice;
        }
        if (!Take(8).Span.SequenceEqual(Magic)) throw new BundleException(BundleFault.UnknownFormat, null, "unknown save format");
        var count = BinaryPrimitives.ReadUInt32LittleEndian(Take(4).Span);
        if (count == 0 || count > MaxContainers)
            throw new BundleException(BundleFault.InvalidContainer, null, string.Create(CultureInfo.InvariantCulture, $"invalid save container {count} containers"));
        var containers = new List<Layout>((int)Math.Min(count, 64));
        long unpacked = 0;
        for (var i = 0; i < count; i++)
        {
            var raw = Take(BinaryPrimitives.ReadUInt16LittleEndian(Take(2).Span)).Span;
            string name;
            try { name = new UTF8Encoding(false, true).GetString(raw); }
            catch (DecoderFallbackException) { throw new BundleException(BundleFault.InvalidContainer, Shown(raw), "invalid save container " + Shown(raw)); }
            Fm5Kind kind;
            if (i == 0)
            {
                kind = name == User ? Fm5Kind.User : throw new BundleException(BundleFault.InvalidContainer, Shown(raw), "invalid save container " + Shown(raw));
            }
            else
            {
                kind = KindOfSibling(name) ?? throw new BundleException(BundleFault.InvalidContainer, Shown(raw), "invalid save container " + Shown(raw));
                // strictly ascending by byte: canonical, and no container twice (the profile is not compared)
                if (containers.Count > 1 && string.CompareOrdinal(containers[^1].Name, name) >= 0)
                    throw new BundleException(BundleFault.InvalidContainer, Shown(raw), "invalid save container " + Shown(raw));
            }
            var label = BinaryPrimitives.ReadUInt64LittleEndian(Take(8).Span);
            // its kind's files; a profile may omit only its TransactionLogFile (T-1110-FM5TLF)
            var files = FilesOf(kind, Take(1).Span[0]) ?? throw new BundleException(BundleFault.InvalidFileCount, null, "invalid file count");
            var entries = new List<Entry>(files.Count);
            foreach (var want in files)
            {
                var rawName = Take(BinaryPrimitives.ReadUInt16LittleEndian(Take(2).Span)).Span;
                if (!rawName.SequenceEqual(Encoding.ASCII.GetBytes(want)))
                    throw new BundleException(BundleFault.UnexpectedFile, Shown(rawName), "unexpected file in the save: " + Shown(rawName));
                var rawSize = BinaryPrimitives.ReadUInt32LittleEndian(Take(4).Span);
                var storedSize = BinaryPrimitives.ReadUInt32LittleEndian(Take(4).Span);
                var method = Take(1).Span[0];
                if (method is not (0 or 2) || (method == 0 && storedSize != rawSize) || storedSize > rawSize)
                    throw new BundleException(BundleFault.InvalidFile, want, "invalid save file " + want);
                var window = FileSizes.First(f => f.Name == want);
                if (rawSize < window.Min || rawSize > window.Max)
                    throw new BundleException(BundleFault.SizeOutOfRange, want, $"{want}: {rawSize} bytes is not a real save's size");
                unpacked += rawSize;
                if (unpacked > MaxUnpacked) throw new BundleException(BundleFault.TooLarge, null, "save too big");
                entries.Add(new Entry(want, rawSize, method, Take(checked((int)storedSize))));
            }
            containers.Add(new Layout(name, kind, label, entries));
        }
        if (at != data.Length) throw new BundleException(BundleFault.TrailingData, null, "trailing data in the bundle");
        return containers;
    }

    private static byte[] Decode(Entry entry)
    {
        var size = (int)entry.RawSize;
        var bytes = entry.Method switch
        {
            0 => entry.Stored.ToArray(),
            2 => Fh2Bundle.LzDecode(entry.Stored.Span, size),
            _ => null,
        };
        return bytes is not null && bytes.Length == size ? bytes : throw new BundleException(BundleFault.Unpack, entry.Name, entry.Name + " does not unpack");
    }

    /// <summary>
    /// The <c>commit</c> of one container lists exactly its data files (<c>fm5::commit_lists</c>): Azimuth's header, <c>blobs N</c> with N its
    /// data files, one <c>&lt;sha256&gt; &lt;size&gt; &lt;name&gt;</c> line each (any order, <c>\n</c> or <c>\r\n</c>). The first file at fault, else null.
    /// </summary>
    public static string? CommitMismatch(ReadOnlySpan<byte> commit, IReadOnlyList<SaveFile> dataFiles)
    {
        ArgumentNullException.ThrowIfNull(dataFiles);
        var lines = Encoding.UTF8.GetString(commit).Split('\n').Select(l => l.TrimEnd('\r')).Where(l => l.Length > 0).ToList();
        foreach (var file in dataFiles)
        {
            var line = Fh2Bundle.CommitLine(file.Name, file.Bytes);
            if (!lines.Skip(2).Contains(line, StringComparer.Ordinal)) return file.Name;
        }
        var blobs = string.Create(CultureInfo.InvariantCulture, $"blobs {dataFiles.Count}");
        return lines.Count != 2 + dataFiles.Count || lines[0] != AzimuthSaves.CommitHeader || lines[1] != blobs ? "commit" : null;
    }

    private static bool IsGameDatabase(ReadOnlySpan<byte> db)
    {
        long inner = db.Length >= 8 ? BinaryPrimitives.ReadUInt32LittleEndian(db[4..8]) : 0;
        return db.Length >= 24 && db[..4].SequenceEqual("ssmc"u8) && inner + 8 <= db.Length && db[8..24].SequenceEqual("SQLite format 3\0"u8);
    }

    private static bool IsProfileData(ReadOnlySpan<byte> body)
    {
        long inner = body.Length >= 8 ? BinaryPrimitives.ReadUInt32LittleEndian(body[4..8]) : long.MaxValue;
        var s = ProfileSummary(body);
        return body.Length >= 4 && body[..4].SequenceEqual("ssmc"u8) && inner + 8 <= body.Length && s is { Level: not null, Xp: not null, Credits: not null };
    }

    /// <summary><c>Main/Credits</c>, <c>Main/XP</c>, <c>Main/Level</c> of an FM5 ProfileData, each by path (<c>fm5::profile_summary</c>); cars unknown.</summary>
    public static SaveSummary ProfileSummary(ReadOnlySpan<byte> profile) =>
        new(ProfilePathInt(profile, "Main/Credits"), ProfilePathInt(profile, "Main/XP"), ProfilePathInt(profile, "Main/Level"), null);

    /// <summary>
    /// ProfileData's integer at <paramref name="path"/> (<c>Main/Level</c>), the property tree walked from its header (<c>bundle::profile_path_int</c>):
    /// <c>"ssmc"</c>, u32 payload length, u32 top-level count; per property u32 name length, the ASCII name, <c>" \0\0\0\0"</c>, u32 type, the
    /// value (type 0x0f a group of u32 children). Null when the tree does not parse, or the path is absent or not an integer.
    /// </summary>
    public static long? ProfilePathInt(ReadOnlySpan<byte> profile, string path)
    {
        ArgumentNullException.ThrowIfNull(path);
        if (profile.Length < 12 || !profile[..4].SequenceEqual("ssmc"u8)) return null;
        uint? U32(ReadOnlySpan<byte> p, long at) => at >= 0 && at + 4 <= p.Length ? BinaryPrimitives.ReadUInt32LittleEndian(p[(int)at..]) : null;
        var end = Math.Min((long)U32(profile, 4)!.Value + 8, profile.Length);
        var want = path.Split('/');
        long at = 12;
        var left = new List<uint> { U32(profile, 8)!.Value };
        var trail = new List<string>();
        var steps = 0;
        while (left.Count > 0)
        {
            if (++steps > 10_000 || at >= end) return null;
            if (left[^1] == 0) { left.RemoveAt(left.Count - 1); if (trail.Count > 0) trail.RemoveAt(trail.Count - 1); continue; }
            left[^1]--;
            if (U32(profile, at) is not { } len || len == 0 || len > 128 || at + 4 + len > profile.Length) return null;
            string name;
            try { name = new UTF8Encoding(false, true).GetString(profile.Slice((int)at + 4, (int)len)); }
            catch (DecoderFallbackException) { return null; }
            at += 4 + len;
            if (at + 9 > profile.Length || !profile.Slice((int)at, 5).SequenceEqual(" \0\0\0\0"u8)) return null;
            var kind = U32(profile, at + 5)!.Value;
            at += 9;
            var here = trail.Count + 1 == want.Length && want[^1] == name && trail.Zip(want).All(p => p.First == p.Second);
            switch (kind)
            {
                case 0 or 1:
                    if (here) return at < profile.Length ? profile[(int)at] : null;
                    at += 1;
                    break;
                case 3 or 7:
                    if (here) return U32(profile, at) is { } v ? kind == 3 ? (long)(int)v : (long)v : null;
                    at += 4;
                    break;
                case 9:
                    at += 4;
                    break;
                case 4:
                    if (here) return at + 8 <= profile.Length ? BinaryPrimitives.ReadInt64LittleEndian(profile[(int)at..]) : null;
                    at += 8;
                    break;
                case 0x0c:
                    if (U32(profile, at) is not { } chars) return null;
                    at += 4 + 2L * chars;
                    break;
                case 0x0f:
                    if (U32(profile, at) is not { } children) return null;
                    at += 4;
                    if (left.Count > 8) return null;
                    left.Add(children);
                    trail.Add(name);
                    break;
                default:
                    return null;
            }
        }
        return null;
    }

    private static void DigestLine(Stream text, string container, string file, ReadOnlySpan<byte> sha)
    {
        text.Write(Encoding.UTF8.GetBytes(container));
        text.WriteByte(0);
        text.Write(Encoding.ASCII.GetBytes(file));
        text.WriteByte(0);
        text.Write(sha);
    }

    /// <summary>
    /// The content digest (<c>fm5::content_digest</c>): SHA-256 over every data file of every container in bundle order as
    /// <c>container ‖ 0x00 ‖ file ‖ 0x00 ‖ SHA-256(file)</c>; commits and labels left out. Lower hex.
    /// </summary>
    public static string ContentDigest(IReadOnlyList<Fm5Container> containers)
    {
        ArgumentNullException.ThrowIfNull(containers);
        using var text = new MemoryStream();
        foreach (var c in containers)
            foreach (var f in c.Files.Where(f => f.Name != "commit"))
                DigestLine(text, c.Name, f.Name, SHA256.HashData(f.Bytes));
        return Convert.ToHexStringLower(SHA256.HashData(text.ToArray()));
    }

    /// <summary>The summary: ProfileData's numbers, and <c>cars</c> the number of livery containers (<c>fm5::summary</c>).</summary>
    public static SaveSummary Summary(IReadOnlyList<Fm5Container> containers)
    {
        ArgumentNullException.ThrowIfNull(containers);
        var profile = (containers.Count > 0 ? containers[0].File("data/ProfileData") : null) ?? [];
        return ProfileSummary(profile) with { Cars = containers.Count(c => c.Name.StartsWith(LiveryPrefix, StringComparison.Ordinal)) };
    }

    /// <summary>
    /// Shape, unpacking and content checks, then digest and summary (<c>fm5::validate</c>): an unpack failure anywhere wins over a content
    /// failure, and the first content failure in bundle order is the one reported. <see cref="ValidBundle.Files"/> are the profile's.
    /// </summary>
    public static ValidBundle Validate(ReadOnlyMemory<byte> data)
    {
        var layout = ParseLayout(data);
        BundleException? failure = null;
        var kept = new List<Fm5Container>(layout.Count);
        foreach (var c in layout)
        {
            var commit = Decode(c.Entries[0]);
            var files = new List<SaveFile>(c.Entries.Count) { new("commit", commit) };
            foreach (var e in c.Entries.Skip(1))
            {
                var body = Decode(e);
                BundleException? content = e.Name switch
                {
                    "data/PlayerDatabase" when !IsGameDatabase(body) => new(BundleFault.NotGameDatabase, e.Name, "PlayerDatabase is not the game's database"),
                    "data/ProfileData" when !IsProfileData(body) => new(BundleFault.NotProfileData, e.Name, "ProfileData is not the game's profile"),
                    "data/bigThumb.png" or "data/smallThumb.png" when !body.AsSpan().StartsWith(Png) => new(BundleFault.NotThumbnail, e.Name, e.Name + " is not a PNG"),
                    _ => null,
                };
                files.Add(new SaveFile(e.Name, body));
                failure ??= content;
            }
            if (failure is null && CommitMismatch(commit, files.Skip(1).ToList()) is { } wrong)
                failure = new BundleException(BundleFault.CommitMismatch, wrong, wrong + " does not match the save's commit");
            kept.Add(new Fm5Container(c.Name, c.Label, files));
        }
        if (failure is not null) throw failure;
        return new ValidBundle(kept[0].Label, kept[0].Files, ContentDigest(kept), Summary(kept)) { Containers = kept };
    }

    /// <summary>
    /// Packs containers (<c>fm5::pack</c>): method 2 where smaller, else stored. Pass the profile first, then the siblings ascending by name,
    /// each container's files in its kind's order, for a bundle <see cref="Validate"/> accepts.
    /// </summary>
    public static byte[] Pack(IReadOnlyList<Fm5Container> containers)
    {
        ArgumentNullException.ThrowIfNull(containers);
        using var output = new MemoryStream();
        Span<byte> number = stackalloc byte[8];
        output.Write(Magic);
        BinaryPrimitives.WriteUInt32LittleEndian(number, (uint)containers.Count);
        output.Write(number[..4]);
        foreach (var c in containers)
        {
            var name = Encoding.UTF8.GetBytes(c.Name);
            BinaryPrimitives.WriteUInt16LittleEndian(number, (ushort)name.Length);
            output.Write(number[..2]);
            output.Write(name);
            BinaryPrimitives.WriteUInt64LittleEndian(number, c.Label);
            output.Write(number);
            output.WriteByte((byte)c.Files.Count);
            foreach (var file in c.Files)
            {
                var fileName = Encoding.ASCII.GetBytes(file.Name);
                BinaryPrimitives.WriteUInt16LittleEndian(number, (ushort)fileName.Length);
                output.Write(number[..2]);
                output.Write(fileName);
                BinaryPrimitives.WriteUInt32LittleEndian(number, (uint)file.Bytes.Length);
                output.Write(number[..4]);
                var packed = file.Bytes.Length == 0 ? [] : Fh2Bundle.LzEncode(file.Bytes);
                var (method, body) = packed.Length > 0 && packed.Length < file.Bytes.Length ? ((byte)2, packed) : ((byte)0, file.Bytes);
                BinaryPrimitives.WriteUInt32LittleEndian(number, (uint)body.Length);
                output.Write(number[..4]);
                output.WriteByte(method);
                output.Write(body);
            }
        }
        return output.ToArray();
    }

    /// <summary>Azimuth's <c>commit</c> for a container's data files (<c>fm5::commit_for</c>): the header, <c>blobs N</c>, one line each.</summary>
    public static byte[] CommitFor(IReadOnlyList<SaveFile> dataFiles)
    {
        ArgumentNullException.ThrowIfNull(dataFiles);
        var data = dataFiles.Where(f => f.Name != "commit").ToList();
        var text = new StringBuilder(string.Create(CultureInfo.InvariantCulture, $"{AzimuthSaves.CommitHeader}\nblobs {data.Count}\n"));
        foreach (var f in data) text.Append(Fh2Bundle.CommitLine(f.Name, f.Bytes)).Append('\n');
        return Encoding.ASCII.GetBytes(text.ToString());
    }

    /// <summary>One container's own digest (its data files by name, as <see cref="ContentDigest"/> lines): whether two revisions hold the same files.</summary>
    public static string ContainerDigest(string container, IReadOnlyList<SaveFile> files)
    {
        ArgumentNullException.ThrowIfNull(files);
        return ContentDigest([new Fm5Container(container, 0, files)]);
    }
}
