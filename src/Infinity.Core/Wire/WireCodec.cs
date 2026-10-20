// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Text;

namespace Infinity.Core.Wire;

/// <summary>
/// The envelope both pipes share (ORCHESTRATOR.md §4, session.rs): <c>u32 LE</c> length of
/// (type + payload), <c>u8</c> type, payload. Integers little-endian, strings and blobs
/// <c>u16 LE</c> length + bytes, flags 0 or 1. Fixed width so the C hook parses it without
/// allocation.
/// </summary>
internal sealed class WireWriter
{
    private readonly List<byte> _body;

    public WireWriter(byte type) => _body = [type];

    public void U8(byte value) => _body.Add(value);

    public void Flag(bool value) => _body.Add(value ? (byte)1 : (byte)0);

    public void U16(ushort value)
    {
        Span<byte> b = stackalloc byte[2];
        BinaryPrimitives.WriteUInt16LittleEndian(b, value);
        _body.AddRange(b);
    }

    public void U32(uint value)
    {
        Span<byte> b = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(b, value);
        _body.AddRange(b);
    }

    public void U64(ulong value)
    {
        Span<byte> b = stackalloc byte[8];
        BinaryPrimitives.WriteUInt64LittleEndian(b, value);
        _body.AddRange(b);
    }

    /// <summary>Exactly <paramref name="length"/> raw bytes (signature, hash).</summary>
    public void Fixed(ReadOnlySpan<byte> bytes, int length, string what)
    {
        if (bytes.Length != length)
        {
            throw new ProtocolException($"{what} must be {length} bytes, not {bytes.Length}");
        }
        _body.AddRange(bytes);
    }

    public void Bytes(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length > ushort.MaxValue)
        {
            throw new ProtocolException("field over 65535 bytes");
        }
        U16((ushort)bytes.Length);
        _body.AddRange(bytes);
    }

    /// <summary>A lone surrogate is refused, never written as U+FFFD (a Rust <c>&amp;str</c> cannot hold one).</summary>
    public void Str(string text)
    {
        byte[] bytes;
        try
        {
            bytes = StrictUtf8.GetBytes(text);
        }
        catch (EncoderFallbackException)
        {
            throw new ProtocolException("string is not valid UTF-16");
        }
        Bytes(bytes);
    }

    private static readonly UTF8Encoding StrictUtf8 = new(encoderShouldEmitUTF8Identifier: false, throwOnInvalidBytes: true);

    public byte[] Finish(string what)
    {
        if (_body.Count > Frames.MaxFrame)
        {
            throw new ProtocolException($"{what} of {_body.Count} bytes");
        }
        var output = new byte[4 + _body.Count];
        BinaryPrimitives.WriteUInt32LittleEndian(output, (uint)_body.Count);
        _body.CopyTo(output, 4);
        return output;
    }
}

internal ref struct WireReader(ReadOnlySpan<byte> data, string what)
{
    private static readonly UTF8Encoding StrictUtf8 = new(encoderShouldEmitUTF8Identifier: false, throwOnInvalidBytes: true);

    private ReadOnlySpan<byte> _data = data;

    public readonly bool IsEmpty => _data.IsEmpty;

    public ReadOnlySpan<byte> Take(int n)
    {
        if (_data.Length < n)
        {
            throw new ProtocolException($"truncated {what}");
        }
        var head = _data[..n];
        _data = _data[n..];
        return head;
    }

    public byte U8() => Take(1)[0];

    public bool Flag(string field) => U8() switch
    {
        0 => false,
        1 => true,
        var other => throw new ProtocolException($"{field} = {other}"),
    };

    public ushort U16() => BinaryPrimitives.ReadUInt16LittleEndian(Take(2));

    public uint U32() => BinaryPrimitives.ReadUInt32LittleEndian(Take(4));

    public ulong U64() => BinaryPrimitives.ReadUInt64LittleEndian(Take(8));

    public byte[] Bytes() => Take(U16()).ToArray();

    public string Str()
    {
        var bytes = Take(U16());
        try
        {
            return StrictUtf8.GetString(bytes);
        }
        catch (DecoderFallbackException)
        {
            throw new ProtocolException($"string in {what} is not UTF-8");
        }
    }

    public readonly void End()
    {
        if (!_data.IsEmpty)
        {
            throw new ProtocolException($"trailing bytes in {what}");
        }
    }
}

/// <summary>Envelope constants and the length check shared by both frame spaces.</summary>
public static class Frames
{
    /// <summary>Largest type + payload (session.rs <c>MAX_FRAME</c>).</summary>
    public const int MaxFrame = 64 * 1024;

    /// <summary>
    /// The body length of the frame at the start of <paramref name="buffer"/>, or null when fewer
    /// than four bytes arrived. Zero and anything over <see cref="MaxFrame"/> are refused.
    /// </summary>
    internal static int? BodyLength(ReadOnlySpan<byte> buffer, string what)
    {
        if (buffer.Length < 4)
        {
            return null;
        }
        var length = BinaryPrimitives.ReadUInt32LittleEndian(buffer);
        if (length == 0 || length > MaxFrame)
        {
            throw new ProtocolException($"{what} length {length}");
        }
        return (int)length;
    }

    /// <summary>
    /// Reads frames from <paramref name="source"/> until EOF or a broken pipe (the reader-thread
    /// loop of session.rs <c>read_frames</c>). A protocol error is thrown after the frames before it.
    /// </summary>
    public static IEnumerable<T> ReadAll<T>(Stream source, Decoder<T> decode)
        where T : class
    {
        ArgumentNullException.ThrowIfNull(source);
        ArgumentNullException.ThrowIfNull(decode);
        var buffer = new List<byte>(4096);
        var chunk = new byte[4096];
        while (true)
        {
            while (true)
            {
                var frame = DecodeHead(buffer, decode, out var used);
                if (frame is null)
                {
                    break;
                }
                buffer.RemoveRange(0, used);
                yield return frame;
            }
            int n;
            try
            {
                n = source.Read(chunk, 0, chunk.Length);
            }
            catch (IOException)
            {
                yield break;
            }
            catch (ObjectDisposedException)
            {
                yield break;
            }
            if (n == 0)
            {
                yield break;
            }
            buffer.AddRange(chunk.AsSpan(0, n));
        }
    }

    private static T? DecodeHead<T>(List<byte> buffer, Decoder<T> decode, out int used)
        where T : class
        => decode(System.Runtime.InteropServices.CollectionsMarshal.AsSpan(buffer), out used);

    /// <summary>Decodes one frame: null when more bytes are needed, otherwise the frame and the bytes it used.</summary>
    public delegate T? Decoder<T>(ReadOnlySpan<byte> buffer, out int used)
        where T : class;
}
