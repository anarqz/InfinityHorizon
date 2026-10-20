// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;

namespace Infinity.Core.Diagnostics;

/// <summary>Only bounded, non-path crash metadata. This does not establish crash causality.</summary>
public sealed record CrashDumpInfo(uint ProcessId, DateTimeOffset TimestampUtc,
    uint? ExceptionCode, string? FaultModule, ulong? FaultModuleRva);

public enum MinidumpReadFailure { None, TooLarge, UnsupportedFormat, Malformed, MissingProcessIdentity }

/// <summary>A structural reader, not a debugger. It never interprets process memory or exposes dump strings.</summary>
public static class MinidumpInspector
{
    public const int MaxDumpBytes = 16 * 1024 * 1024;
    public const int MaxUploadDumpBytes = 4 * 1024 * 1024;
    public const int MaxStreams = 128;
    private const int MaxRecords = 4096;

    /// <summary>Eligibility for the server's restricted attachment format; this does not promise memory redaction.</summary>
    public static bool IsMinimizedUpload(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length > MaxUploadDumpBytes || !TryRead(bytes, out _, out _)) return false;
        if ((U64(bytes, 24) & ~0x1080UL) != 0) return false;
        var count = U32(bytes, 8);
        var directory = (int)U32(bytes, 12);
        for (var i = 0; i < count; i++)
            if (U32(bytes, directory + (i * 12)) == 9) return false;
        return true;
    }

    public static bool TryRead(ReadOnlySpan<byte> bytes, out CrashDumpInfo? info, out MinidumpReadFailure failure)
    {
        info = null;
        failure = MinidumpReadFailure.Malformed;
        if (bytes.Length > MaxDumpBytes) { failure = MinidumpReadFailure.TooLarge; return false; }
        if (bytes.Length < 32) return false;
        if (U32(bytes, 0) != 0x504d444d || (U32(bytes, 4) & 0xffff) != 0xa793)
        { failure = MinidumpReadFailure.UnsupportedFormat; return false; }
        var count = U32(bytes, 8);
        var directory = U32(bytes, 12);
        if (count == 0 || count > MaxStreams || directory < 32 || !Range(bytes, directory, count * 12UL)) return false;
        var types = new HashSet<uint>();
        uint? processId = null;
        uint? exceptionCode = null;
        ulong? faultAddress = null;
        uint moduleRva = 0, moduleSize = 0;
        for (var i = 0; i < count; i++)
        {
            var entry = (int)directory + (i * 12);
            var type = U32(bytes, entry);
            var size = U32(bytes, entry + 4);
            var rva = U32(bytes, entry + 8);
            if (!types.Add(type) || !Location(bytes, size, rva)) return false;
            if (size != 0 && (rva < 32 || Overlaps(rva, size, directory, count * 12UL))) return false;
            var stream = bytes.Slice((int)rva, (int)size);
            switch (type)
            {
                case 0: // Unused stream.
                    if (size != 0) return false;
                    break;
                case 3:
                case 8:
                    if (!Threads(bytes, stream, type == 8)) return false;
                    break;
                case 4:
                    if (!Modules(bytes, stream)) return false;
                    moduleRva = rva; moduleSize = size;
                    break;
                case 5:
                    if (!Memory(bytes, stream)) return false;
                    break;
                case 6:
                    if (size < 168 || U32(stream, 32) > 15 || !Descriptor(bytes, stream, 160)) return false;
                    exceptionCode = U32(stream, 8); faultAddress = U64(stream, 24);
                    break;
                case 7:
                    if (size < 56 || !OptionalString(bytes, U32(stream, 24))) return false;
                    break;
                case 9:
                    if (!Memory64(bytes, stream)) return false;
                    break;
                case 10:
                case 11: // Comment streams are never decoded or exposed.
                    if (type == 11 && size % 2 != 0) return false;
                    break;
                case 14:
                    if (!UnloadedModules(bytes, stream)) return false;
                    break;
                case 15:
                    if (size < 24 || U32(stream, 0) < 24 || U32(stream, 0) > size) return false;
                    if ((U32(stream, 4) & 1) != 0) processId = U32(stream, 8);
                    break;
                case 16:
                    if (size < 16 || !Table(stream, U32(stream, 0), U32(stream, 4), U64(stream, 8), 16, 48)) return false;
                    break;
                case 17:
                    if (size < 12 || !Table(stream, U32(stream, 0), U32(stream, 4), U32(stream, 8), 12, 64)) return false;
                    break;
                case 21:
                case 22: // System memory / process VM counters contain no RVAs.
                    if (size < 4) return false;
                    break;
                case 24:
                    if (!ThreadNames(bytes, stream)) return false;
                    break;
                default:
                    // Do not attach streams whose nested pointer/size layout this reader cannot validate.
                    failure = MinidumpReadFailure.UnsupportedFormat; return false;
            }
        }
        if (processId is null or 0) { failure = MinidumpReadFailure.MissingProcessIdentity; return false; }
        string? module = null;
        ulong? relativeAddress = null;
        if (faultAddress is { } address && moduleSize != 0)
        {
            var modules = bytes.Slice((int)moduleRva, (int)moduleSize);
            var moduleCount = U32(modules, 0);
            for (var i = 0; i < moduleCount; i++)
            {
                var offset = 4 + (i * 108);
                var start = U64(modules, offset);
                var length = U32(modules, offset + 8);
                if (address < start || address - start >= length) continue;
                if (relativeAddress is not null) return false; // Ambiguous overlapping module ranges.
                relativeAddress = address - start;
                module = SafeBasename(bytes, U32(modules, offset + 20));
            }
        }
        info = new(processId.Value, DateTimeOffset.FromUnixTimeSeconds(U32(bytes, 20)), exceptionCode, module, relativeAddress);
        failure = MinidumpReadFailure.None;
        return true;
    }

    private static bool Threads(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> stream, bool extended)
    {
        var entrySize = extended ? 64 : 48;
        if (stream.Length < 4 || !Table(stream, 4, (uint)entrySize, U32(stream, 0), 4, (uint)entrySize)) return false;
        for (var i = 0; i < U32(stream, 0); i++)
        {
            var entry = 4 + (i * entrySize);
            if (!Descriptor(bytes, stream, entry + 32) || !Descriptor(bytes, stream, entry + 40)
                || (extended && !Descriptor(bytes, stream, entry + 56))) return false;
        }
        return true;
    }

    private static bool Modules(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> stream)
    {
        if (stream.Length < 4 || !Table(stream, 4, 108, U32(stream, 0), 4, 108)) return false;
        for (var i = 0; i < U32(stream, 0); i++)
        {
            var entry = 4 + (i * 108);
            if (!OptionalString(bytes, U32(stream, entry + 20)) || !Descriptor(bytes, stream, entry + 76)
                || !Descriptor(bytes, stream, entry + 84)) return false;
            var start = U64(stream, entry);
            if (U32(stream, entry + 8) > ulong.MaxValue - start) return false;
        }
        return true;
    }

    private static bool Memory(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> stream)
    {
        if (stream.Length < 4 || !Table(stream, 4, 16, U32(stream, 0), 4, 16)) return false;
        for (var i = 0; i < U32(stream, 0); i++)
            if (!Descriptor(bytes, stream, 4 + (i * 16) + 8)) return false;
        return true;
    }

    private static bool Memory64(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> stream)
    {
        if (stream.Length < 16 || !Table(stream, 16, 16, U64(stream, 0), 16, 16)) return false;
        var offset = U64(stream, 8);
        for (var i = 0; (ulong)i < U64(stream, 0); i++)
        {
            var size = U64(stream, 16 + (i * 16) + 8);
            if (!Range(bytes, offset, size)) return false;
            offset += size; // Range guarantees both operands fit within the bounded file.
        }
        return Range(bytes, offset, 0);
    }

    private static bool UnloadedModules(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> stream)
    {
        if (stream.Length < 12 || !Table(stream, U32(stream, 0), U32(stream, 4), U32(stream, 8), 12, 24)) return false;
        for (var i = 0; i < U32(stream, 8); i++)
            if (!OptionalString(bytes, U32(stream, (int)U32(stream, 0) + (i * (int)U32(stream, 4)) + 20))) return false;
        return true;
    }

    private static bool ThreadNames(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> stream)
    {
        if (stream.Length < 4 || !Table(stream, 4, 12, U32(stream, 0), 4, 12)) return false;
        for (var i = 0; i < U32(stream, 0); i++)
        {
            var rva = U64(stream, 4 + (i * 12) + 4);
            if (rva > uint.MaxValue || !OptionalString(bytes, (uint)rva)) return false;
        }
        return true;
    }

    private static bool Table(ReadOnlySpan<byte> stream, uint header, uint entry, ulong count, uint minimumHeader, uint minimumEntry) =>
        header >= minimumHeader && header <= stream.Length && entry >= minimumEntry
        && count <= MaxRecords && count <= ((ulong)stream.Length - header) / entry;

    private static bool Descriptor(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> stream, int offset) =>
        Location(bytes, U32(stream, offset), U32(stream, offset + 4));

    private static bool Location(ReadOnlySpan<byte> bytes, uint size, uint rva) =>
        Range(bytes, rva, size) && (size == 0 || rva >= 32);

    private static bool OptionalString(ReadOnlySpan<byte> bytes, uint rva)
    {
        if (rva == 0) return true;
        if (rva < 32 || !Range(bytes, rva, 4)) return false;
        var length = U32(bytes, (int)rva);
        return length <= 2048 && length % 2 == 0 && Range(bytes, rva + 4UL, length);
    }

    private static string? SafeBasename(ReadOnlySpan<byte> bytes, uint rva)
    {
        if (rva == 0) return null;
        var name = bytes.Slice((int)rva + 4, (int)U32(bytes, (int)rva));
        var start = 0;
        for (var i = 0; i < name.Length; i += 2)
            if (U16(name, i) is '\\' or '/') start = i + 2;
        var chars = (name.Length - start) / 2;
        if (chars is < 1 or > 96) return null;
        Span<char> result = stackalloc char[chars];
        for (var i = 0; i < chars; i++)
        {
            var character = U16(name, start + (i * 2));
            if (!(character is >= 'a' and <= 'z' or >= 'A' and <= 'Z' or >= '0' and <= '9' or '.' or '_' or '-')) return null;
            result[i] = (char)character;
        }
        return new string(result);
    }

    private static bool Overlaps(ulong a, ulong lengthA, ulong b, ulong lengthB) => a < b + lengthB && b < a + lengthA;
    private static bool Range(ReadOnlySpan<byte> bytes, ulong offset, ulong size) => offset <= (ulong)bytes.Length && size <= (ulong)bytes.Length - offset;
    private static ushort U16(ReadOnlySpan<byte> bytes, int offset) => BinaryPrimitives.ReadUInt16LittleEndian(bytes[offset..]);
    private static uint U32(ReadOnlySpan<byte> bytes, int offset) => BinaryPrimitives.ReadUInt32LittleEndian(bytes[offset..]);
    private static ulong U64(ReadOnlySpan<byte> bytes, int offset) => BinaryPrimitives.ReadUInt64LittleEndian(bytes[offset..]);
}
