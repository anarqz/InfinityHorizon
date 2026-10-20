// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>Why a bundle is not a valid FH2 save (<c>fi_saves::bundle::BundleError</c>). Never a strike: almost always a revision read mid-write.</summary>
public enum BundleFault
{
    TooLarge,
    Truncated,
    UnknownFormat,
    InvalidFileCount,
    UnexpectedFile,
    InvalidFile,
    SizeOutOfRange,
    MissingFile,
    TrailingData,
    Unpack,
    CommitMismatch,
    NotGameDatabase,
    NotXml,
}

/// <summary>A bundle that does not pass <see cref="Fh2Bundle"/>'s checks; <see cref="File"/> names the file when one is at fault.</summary>
public sealed class BundleException(BundleFault fault, string? file, string message) : Exception(message)
{
    public BundleFault Fault { get; } = fault;

    public string? File { get; } = file;
}

/// <summary>One file of a save revision: <c>commit</c> or <c>data/&lt;name&gt;</c> (a name of <see cref="Fh2Bundle.SaveFiles"/>).</summary>
public sealed record SaveFile(string Name, byte[] Bytes);

/// <summary>One file of a bundle before unpacking: <c>Method</c> 0 stored, 1 XPRESS Huffman, 2 the hook's LZ.</summary>
public sealed record BundleEntry(string Name, uint RawSize, byte Method, ReadOnlyMemory<byte> Stored);

/// <summary>A bundle's parsed shape: the label (Azimuth's revision folder number) and its files in bundle order.</summary>
public sealed record BundleLayout(ulong Label, IReadOnlyList<BundleEntry> Entries);

/// <summary>A bundle that passed every check: files unpacked in <see cref="Fh2Bundle.SaveFiles"/> order, content digest (lower hex), summary.</summary>
public sealed record ValidBundle(ulong Label, IReadOnlyList<SaveFile> Files, string ContentSha256, SaveSummary Summary)
{
    /// <summary>The label as Azimuth names the folder (16 lowercase hex; folder names compare case-insensitively, CR-L9).</summary>
    public string LabelHex => Label.ToString("x16", CultureInfo.InvariantCulture);
}

/// <summary>
/// The FH2SAVE1 bundle, byte for byte <c>crates/fi-saves/src/bundle.rs</c> (CLOUD_SAVES.md §3,
/// SAVES_API.md §2): one Azimuth revision packed into one file.
/// <para>
/// Layout (little endian): <c>"FH2SAVE1"</c>, <c>u64</c> label, <c>u32</c> file count, then per file
/// <c>u16</c> name length, name, <c>u32</c> raw size, <c>u32</c> stored size, <c>u8</c> method, stored bytes.
/// <see cref="Pack"/> writes method 2 where smaller, else stored, deterministically, so packing the
/// same files twice gives the same bytes on both sides.
/// </para>
/// </summary>
public static class Fh2Bundle
{
    /// <summary>The bundle media type (<c>fi_protocol::headers::FH2SAVE1_MEDIA_TYPE</c>).</summary>
    public const string MediaType = "application/x-fh2save1";

    /// <summary>Largest bundle accepted: 384 KiB.</summary>
    public const int MaxBundle = 384 << 10;

    private const uint MaxFile = 8 << 20;
    private const long MaxUnpacked = 16 << 20;
    private const int LzMinMatch = 4;
    private const int LzHashBits = 16;

    public static ReadOnlySpan<byte> Magic => "FH2SAVE1"u8;

    /// <summary>The only names a bundle may hold, in canonical order.</summary>
    public static IReadOnlyList<string> SaveFiles { get; } =
    [
        "commit",
        "data/BinarySaveState",
        "data/PlayerDatabase",
        "data/ProfileData",
        "data/SaveState",
        "data/TransactionLogFile",
        "data/VersionFlags",
    ];

    /// <summary>Files every save has.</summary>
    public static IReadOnlyList<string> Required { get; } = ["commit", "data/PlayerDatabase", "data/ProfileData", "data/SaveState"];

    /// <summary>(name, smallest, largest) unpacked size of each file (bundle.rs <c>FILE_SIZES</c>, CR-L6 minimums).</summary>
    public static IReadOnlyList<(string Name, uint Min, uint Max)> FileSizes { get; } =
    [
        ("commit", 256, 2048),
        ("data/BinarySaveState", 4, 256 << 10),
        ("data/PlayerDatabase", 6_291_464, 6_291_464),
        ("data/ProfileData", 20_488, 20_488),
        ("data/SaveState", 1 << 10, 256 << 10),
        ("data/TransactionLogFile", 0, 64 << 10),
        ("data/VersionFlags", 4, 4),
    ];

    /// <summary>Parses and checks a bundle's shape without unpacking (bundle.rs <c>parse_layout</c>).</summary>
    public static BundleLayout ParseLayout(ReadOnlyMemory<byte> data)
    {
        if (data.Length > MaxBundle)
        {
            throw new BundleException(BundleFault.TooLarge, null, "save too big");
        }
        var at = 0;
        ReadOnlyMemory<byte> Take(int n)
        {
            if (n < 0 || n > data.Length - at)
            {
                throw new BundleException(BundleFault.Truncated, null, "truncated bundle");
            }
            var slice = data.Slice(at, n);
            at += n;
            return slice;
        }
        if (!Take(8).Span.SequenceEqual(Magic))
        {
            throw new BundleException(BundleFault.UnknownFormat, null, "unknown save format");
        }
        var label = BinaryPrimitives.ReadUInt64LittleEndian(Take(8).Span);
        var count = BinaryPrimitives.ReadUInt32LittleEndian(Take(4).Span);
        if (count == 0 || count > SaveFiles.Count)
        {
            throw new BundleException(BundleFault.InvalidFileCount, null, "invalid file count");
        }
        var entries = new List<BundleEntry>((int)count);
        long unpacked = 0;
        for (var i = 0; i < count; i++)
        {
            var nameLength = BinaryPrimitives.ReadUInt16LittleEndian(Take(2).Span);
            var raw = Take(nameLength).ToArray();
            var shown = new string(Encoding.UTF8.GetString(raw).Take(40).ToArray());
            var name = SaveFiles.FirstOrDefault(n => Encoding.ASCII.GetBytes(n).AsSpan().SequenceEqual(raw))
                ?? throw new BundleException(BundleFault.UnexpectedFile, shown, "unexpected file in the save: " + shown);
            if (entries.Any(e => e.Name == name))
            {
                throw new BundleException(BundleFault.UnexpectedFile, shown, "unexpected file in the save: " + shown);
            }
            var rawSize = BinaryPrimitives.ReadUInt32LittleEndian(Take(4).Span);
            var storedSize = BinaryPrimitives.ReadUInt32LittleEndian(Take(4).Span);
            var method = Take(1).Span[0];
            // A packed file never takes more room than it unpacks to (the hook stores it as is then).
            if (rawSize > MaxFile || method > 2 || (method == 0 && storedSize != rawSize) || storedSize > rawSize)
            {
                throw new BundleException(BundleFault.InvalidFile, name, "invalid save file " + name);
            }
            var window = FileSizes.First(f => f.Name == name);
            if (rawSize < window.Min || rawSize > window.Max)
            {
                throw new BundleException(BundleFault.SizeOutOfRange, name, $"{name}: {rawSize} bytes is not a real save's size");
            }
            unpacked += rawSize;
            if (unpacked > MaxUnpacked)
            {
                throw new BundleException(BundleFault.TooLarge, null, "save too big");
            }
            entries.Add(new BundleEntry(name, rawSize, method, Take((int)storedSize)));
        }
        if (Required.FirstOrDefault(r => entries.All(e => e.Name != r)) is { } missing)
        {
            throw new BundleException(BundleFault.MissingFile, missing, "save without " + missing);
        }
        if (at != data.Length)
        {
            throw new BundleException(BundleFault.TrailingData, null, "trailing data in the bundle");
        }
        return new BundleLayout(label, entries);
    }

    /// <summary>Unpacks every file, in <see cref="SaveFiles"/> order.</summary>
    public static IReadOnlyList<SaveFile> Unpack(BundleLayout layout)
    {
        ArgumentNullException.ThrowIfNull(layout);
        var files = new List<SaveFile>(layout.Entries.Count);
        foreach (var name in SaveFiles)
        {
            if (layout.Entries.FirstOrDefault(e => e.Name == name) is { } entry)
            {
                files.Add(new SaveFile(name, Decode(entry)));
            }
        }
        return files;
    }

    private static byte[] Decode(BundleEntry entry)
    {
        var size = (int)entry.RawSize;
        var bytes = entry.Method switch
        {
            0 => entry.Stored.ToArray(),
            1 => XpressHuffmanDecode(entry.Stored.Span, size),
            2 => LzDecode(entry.Stored.Span, size),
            _ => null,
        };
        return bytes is not null && bytes.Length == size
            ? bytes
            : throw new BundleException(BundleFault.Unpack, entry.Name, entry.Name + " does not unpack");
    }

    private static byte[]? File(IReadOnlyList<SaveFile> files, string name) => files.FirstOrDefault(f => f.Name == name)?.Bytes;

    /// <summary>The line a data file has in <c>commit</c>: <c>"&lt;sha256&gt; &lt;size&gt; &lt;short name&gt;"</c>.</summary>
    public static string CommitLine(string name, ReadOnlySpan<byte> body)
    {
        ArgumentNullException.ThrowIfNull(name);
        var shortName = name.StartsWith("data/", StringComparison.Ordinal) ? name[5..] : name;
        return string.Create(CultureInfo.InvariantCulture, $"{Convert.ToHexStringLower(SHA256.HashData(body))} {body.Length} {shortName}");
    }

    /// <summary>Unpacked files against <c>commit</c> and the game's formats (bundle.rs <c>check_contents</c>).</summary>
    public static void CheckContents(IReadOnlyList<SaveFile> files)
    {
        ArgumentNullException.ThrowIfNull(files);
        var commitBytes = File(files, "commit") ?? throw new BundleException(BundleFault.MissingFile, "commit", "save without commit");
        var lines = Encoding.UTF8.GetString(commitBytes).Split('\n').Select(l => l.TrimEnd('\r')).ToHashSet(StringComparer.Ordinal);
        foreach (var file in files.Where(f => f.Name != "commit"))
        {
            if (!lines.Contains(CommitLine(file.Name, file.Bytes)))
            {
                throw new BundleException(BundleFault.CommitMismatch, file.Name, file.Name + " does not match the save's commit");
            }
        }
        if (!IsGameDatabase(File(files, "data/PlayerDatabase") ?? []))
        {
            throw new BundleException(BundleFault.NotGameDatabase, "data/PlayerDatabase", "PlayerDatabase is not the game's database");
        }
        if (!IsXml(File(files, "data/SaveState") ?? []))
        {
            throw new BundleException(BundleFault.NotXml, "data/SaveState", "SaveState is not XML");
        }
    }

    /// <summary>The game's <c>ssmc</c> envelope (tag, u32 inner length) around a SQLite file.</summary>
    private static bool IsGameDatabase(ReadOnlySpan<byte> db)
    {
        long inner = db.Length >= 8 ? BinaryPrimitives.ReadUInt32LittleEndian(db[4..8]) : 0;
        return db.Length >= 24 && db[..4].SequenceEqual("ssmc"u8) && inner + 8 <= db.Length && db[8..24].SequenceEqual("SQLite format 3\0"u8);
    }

    /// <summary>The first byte past ASCII whitespace and a UTF-8 BOM is <c>&lt;</c> (Rust's <c>is_ascii_whitespace</c>: no vertical tab).</summary>
    private static bool IsXml(ReadOnlySpan<byte> state)
    {
        foreach (var b in state)
        {
            if (b is (byte)' ' or (byte)'\t' or (byte)'\n' or 0x0c or (byte)'\r' or 0xef or 0xbb or 0xbf)
            {
                continue;
            }
            return b == (byte)'<';
        }
        return false;
    }

    /// <summary>
    /// The content digest (SAVES_API.md §2): SHA-256 over each data file present, in canonical
    /// order, as <c>name ‖ 0x00 ‖ sha256(file)</c>. Ignores <c>commit</c> and the label. Lower hex.
    /// </summary>
    public static string ContentDigest(IReadOnlyList<SaveFile> files)
    {
        ArgumentNullException.ThrowIfNull(files);
        using var text = new MemoryStream();
        foreach (var name in SaveFiles.Skip(1))
        {
            if (File(files, name) is { } bytes)
            {
                text.Write(Encoding.ASCII.GetBytes(name));
                text.WriteByte(0);
                text.Write(SHA256.HashData(bytes));
            }
        }
        return Convert.ToHexStringLower(SHA256.HashData(text.ToArray()));
    }

    /// <summary>The progress numbers of ProfileData (<c>cars</c> needs the database and stays null, as bundle.rs).</summary>
    public static SaveSummary Summary(IReadOnlyList<SaveFile> files)
    {
        ArgumentNullException.ThrowIfNull(files);
        var profile = File(files, "data/ProfileData") ?? [];
        long? Int(string name) => AzimuthSaves.ProfileInt(profile, name);
        return new SaveSummary(Int("Credits"), Int("XP"), Int("Level"), null);
    }

    /// <summary>Shape, unpacking and content checks, then digest and summary (bundle.rs <c>validate</c>).</summary>
    public static ValidBundle Validate(ReadOnlyMemory<byte> data)
    {
        var layout = ParseLayout(data);
        var files = Unpack(layout);
        CheckContents(files);
        return new ValidBundle(layout.Label, files, ContentDigest(files), Summary(files));
    }

    /// <summary>Packs files into a bundle (bundle.rs <c>pack</c>): method 2 where smaller, else stored, in the order given.</summary>
    public static byte[] Pack(ulong label, IReadOnlyList<SaveFile> files)
    {
        ArgumentNullException.ThrowIfNull(files);
        using var output = new MemoryStream();
        Span<byte> number = stackalloc byte[8];
        output.Write(Magic);
        BinaryPrimitives.WriteUInt64LittleEndian(number, label);
        output.Write(number);
        BinaryPrimitives.WriteUInt32LittleEndian(number, (uint)files.Count);
        output.Write(number[..4]);
        foreach (var file in files)
        {
            var name = Encoding.ASCII.GetBytes(file.Name);
            BinaryPrimitives.WriteUInt16LittleEndian(number, (ushort)name.Length);
            output.Write(number[..2]);
            output.Write(name);
            BinaryPrimitives.WriteUInt32LittleEndian(number, (uint)file.Bytes.Length);
            output.Write(number[..4]);
            var packed = file.Bytes.Length == 0 ? [] : LzEncode(file.Bytes);
            var (method, body) = packed.Length > 0 && packed.Length < file.Bytes.Length ? ((byte)2, packed) : ((byte)0, file.Bytes);
            BinaryPrimitives.WriteUInt32LittleEndian(number, (uint)body.Length);
            output.Write(number[..4]);
            output.WriteByte(method);
            output.Write(body);
        }
        return output.ToArray();
    }

    // ---------------------------------------------------------------- the hook's LZ (method 2)

    private static void PutVarint(MemoryStream output, int value)
    {
        var v = (uint)value;
        while (true)
        {
            var b = (byte)(v & 127);
            v >>= 7;
            output.WriteByte((byte)(b | (v != 0 ? 128 : 0)));
            if (v == 0)
            {
                return;
            }
        }
    }

    private static uint Read32(ReadOnlySpan<byte> p, int i) => BinaryPrimitives.ReadUInt32LittleEndian(p[i..]);

    private static int LzHash(ReadOnlySpan<byte> p, int i) => (int)(unchecked(Read32(p, i) * 2_654_435_761u) >> (32 - LzHashBits));

    /// <summary>
    /// <c>hook/adapters/azimuth_codec.c</c> <c>codec_lz_encode</c> (bundle.rs <c>lz_encode</c>): varint
    /// literal count, the literals, then varint length - 4 and varint offset - 1; no empty run after a
    /// final match. Deterministic.
    /// </summary>
    public static byte[] LzEncode(ReadOnlySpan<byte> input)
    {
        var size = input.Length;
        using var output = new MemoryStream(size / 8 + 16);
        var table = new uint[1 << LzHashBits];
        int anchor = 0, i = 0;
        static void Literals(MemoryStream output, ReadOnlySpan<byte> input, int from, int to)
        {
            PutVarint(output, to - from);
            output.Write(input[from..to]);
        }
        while (i + LzMinMatch <= size)
        {
            var h = LzHash(input, i);
            var candidate = (int)table[h];
            table[h] = (uint)(i + 1);
            if (candidate == 0 || Read32(input, candidate - 1) != Read32(input, i))
            {
                i++;
                continue;
            }
            var from = candidate - 1;
            var length = LzMinMatch;
            while (i + length < size && input[from + length] == input[i + length])
            {
                length++;
            }
            Literals(output, input, anchor, i);
            PutVarint(output, length - LzMinMatch);
            PutVarint(output, i - from - 1);
            var end = i + length;
            for (var p = i + 1; p + LzMinMatch <= size && p < end && p < i + 16; p++)
            {
                table[LzHash(input, p)] = (uint)(p + 1);
            }
            if (end - 1 > i + 16 && end - 1 + LzMinMatch <= size)
            {
                table[LzHash(input, end - 1)] = (uint)end;
            }
            i = end;
            anchor = end;
        }
        if (anchor < size)
        {
            Literals(output, input, anchor, size);
        }
        return output.ToArray();
    }

    private static long? Varint(ReadOnlySpan<byte> input, ref int at)
    {
        long v = 0;
        for (var shift = 0; shift < 35; shift += 7)
        {
            if (at >= input.Length)
            {
                return null;
            }
            var b = input[at++];
            v |= (long)(b & 127) << shift;
            if ((b & 128) == 0)
            {
                return v;
            }
        }
        return null;
    }

    /// <summary>Decodes method 2; null unless the input decodes to exactly <paramref name="size"/> bytes with nothing left.</summary>
    public static byte[]? LzDecode(ReadOnlySpan<byte> input, int size)
    {
        var at = 0;
        var output = new byte[size];
        var length0 = 0;
        while (length0 < size)
        {
            if (Varint(input, ref at) is not { } n || n > size - length0 || n > input.Length - at)
            {
                return null;
            }
            input.Slice(at, (int)n).CopyTo(output.AsSpan(length0));
            at += (int)n;
            length0 += (int)n;
            if (length0 == size)
            {
                break;
            }
            if (Varint(input, ref at) is not { } l || Varint(input, ref at) is not { } o)
            {
                return null;
            }
            var length = l + 4;
            var offset = o + 1;
            if (offset > length0 || length > size - length0)
            {
                return null;
            }
            for (var k = 0; k < length; k++)
            {
                output[length0] = output[length0 - offset];
                length0++;
            }
        }
        return at == input.Length ? output : null;
    }

    /// <summary>
    /// XPRESS Huffman (MS-XCA 2.2.4), method 1 of hooks before method 2 (bundle.rs
    /// <c>xpress_huffman_decode</c>): blocks of 64 KiB of output, each starting with 256 bytes of 4-bit
    /// code lengths for 512 symbols. Decode only.
    /// </summary>
    public static byte[]? XpressHuffmanDecode(ReadOnlySpan<byte> input, int size)
    {
        static uint Word(ReadOnlySpan<byte> data, ref int at)
        {
            uint v = (at < data.Length ? data[at] : 0u) | (uint)(at + 1 < data.Length ? data[at + 1] : 0) << 8;
            at += 2;
            return v;
        }
        static void Take(ReadOnlySpan<byte> data, int count, ref uint bits, ref int extra, ref int at)
        {
            bits <<= count;
            extra -= count;
            if (extra < 0)
            {
                bits |= Word(data, ref at) << -extra;
                extra += 16;
            }
        }
        var output = new List<byte>(size);
        int @base = 0, blockEnd = 0;
        var symbols = new ushort[1 << 15];
        var lengths = new byte[1 << 15];
        while (output.Count < size)
        {
            if (@base > input.Length - 256)
            {
                return null;
            }
            var table = input.Slice(@base, 256);
            Array.Clear(symbols);
            Array.Clear(lengths);
            var code = 0;
            for (byte length = 1; length <= 15; length++)
            {
                for (var symbol = 0; symbol < 512; symbol++)
                {
                    var l = symbol % 2 == 0 ? table[symbol / 2] & 15 : table[symbol / 2] >> 4;
                    if (l == length)
                    {
                        var first = (long)code << (15 - length);
                        var n = 1 << (15 - length);
                        if (first + n > symbols.Length)
                        {
                            return null;
                        }
                        Array.Fill(symbols, (ushort)symbol, (int)first, n);
                        Array.Fill(lengths, length, (int)first, n);
                        code++;
                    }
                }
                code <<= 1;
            }
            var at = @base + 256;
            var high = Word(input, ref at);
            var bits = high << 16 | Word(input, ref at);
            var extra = 16;
            blockEnd += 65_536;
            var end = Math.Min(blockEnd, size);
            while (output.Count < end)
            {
                var index = (int)(bits >> 17);
                var (symbol, codeLength) = (symbols[index], lengths[index]);
                if (codeLength == 0)
                {
                    return null;
                }
                Take(input, codeLength, ref bits, ref extra, ref at);
                if (symbol < 256)
                {
                    output.Add((byte)symbol);
                    continue;
                }
                var match = symbol - 256;
                long length = match & 15;
                var offsetBits = match >> 4;
                if (length == 15)
                {
                    if (at >= input.Length)
                    {
                        return null;
                    }
                    length = input[at] + 15;
                    at++;
                    if (length == 270)
                    {
                        if (at > input.Length - 2)
                        {
                            return null;
                        }
                        length = BinaryPrimitives.ReadUInt16LittleEndian(input[at..]);
                        at += 2;
                        if (length == 0)
                        {
                            if (at > input.Length - 4)
                            {
                                return null;
                            }
                            length = BinaryPrimitives.ReadUInt32LittleEndian(input[at..]);
                            at += 4;
                        }
                    }
                }
                length += 3;
                long offset = offsetBits > 0 ? (bits >> (32 - offsetBits)) | (1u << offsetBits) : 1;
                if (offsetBits > 0)
                {
                    Take(input, offsetBits, ref bits, ref extra, ref at);
                }
                if (offset > output.Count || length > Math.Max(0, size - output.Count) + 65_536L)
                {
                    return null;
                }
                for (var k = 0; k < length; k++)
                {
                    output.Add(output[output.Count - (int)offset]);
                }
            }
            @base = at;
        }
        return [.. output.Take(size)];
    }
}
