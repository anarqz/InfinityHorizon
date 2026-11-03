// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.DedicatedServer;

/// <summary>
/// Splits a byte stream into <c>\n</c>-terminated lines of at most <see cref="Limit"/> bytes (the control channel and
/// the engine's stdout, DEDICATED_SERVER.md §2.3, §3.2). A partial line waits for its next bytes; a line longer than
/// the limit is never buffered past it: <see cref="Push"/> reports it once and the framer stays failed, because the
/// launcher kills a child that writes one.
/// </summary>
public sealed class DsLineFramer(int limit = DsProto.MaxLine)
{
    private byte[] _buffer = new byte[Math.Min(limit, 16 * 1024)];
    private int _length;

    public int Limit { get; } = limit > 0 ? limit : throw new ArgumentOutOfRangeException(nameof(limit));

    /// <summary>True once a line went past <see cref="Limit"/>.</summary>
    public bool Oversize { get; private set; }

    /// <summary>The bytes of an unfinished line (none once the stream ends cleanly).</summary>
    public int Pending => _length;

    /// <summary>Adds <paramref name="bytes"/>; each complete line (without its <c>\n</c>) goes to <paramref name="line"/>. False when a line is too long.</summary>
    public bool Push(ReadOnlySpan<byte> bytes, Action<ReadOnlyMemory<byte>> line)
    {
        ArgumentNullException.ThrowIfNull(line);
        if (Oversize)
        {
            return false;
        }
        while (!bytes.IsEmpty)
        {
            var end = bytes.IndexOf((byte)'\n');
            var take = end < 0 ? bytes : bytes[..end];
            if (_length + take.Length > Limit)
            {
                Oversize = true;
                _length = 0;
                _buffer = [];
                return false;
            }
            Append(take);
            if (end < 0)
            {
                return true;
            }
            line(_buffer.AsMemory(0, _length).ToArray());
            _length = 0;
            bytes = bytes[(end + 1)..];
        }
        return true;
    }

    /// <summary>The unfinished last line at the end of the stream (a final line without its newline), then empty.</summary>
    public byte[] TakePending()
    {
        var rest = _buffer.AsSpan(0, _length).ToArray();
        _length = 0;
        return rest;
    }

    private void Append(ReadOnlySpan<byte> bytes)
    {
        if (_length + bytes.Length > _buffer.Length)
        {
            Array.Resize(ref _buffer, Math.Min(Limit, Math.Max(_buffer.Length * 2, _length + bytes.Length)));
        }
        bytes.CopyTo(_buffer.AsSpan(_length));
        _length += bytes.Length;
    }
}
