// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Session;

/// <summary>
/// The file handoff (<c>FI_LAUNCH=v1;file=&lt;nonce&gt;</c>, ADR 0105 §4): <c>FISESS01</c>, the 16-byte
/// nonce, then one encoded <see cref="Frame.TicketCode"/>. It never holds a bearer token: a copy
/// of it is worth one redeem code, accepted once before the ticket expires.
/// </summary>
public static class SessionFile
{
    public static ReadOnlySpan<byte> Magic => "FISESS01"u8;

    public static byte[] Encode(ReadOnlySpan<byte> nonce, Frame ticket)
    {
        ArgumentNullException.ThrowIfNull(ticket);
        if (ticket is not Frame.TicketCode)
        {
            throw new ProtocolException("a session file holds a TicketCode frame");
        }
        if (nonce.Length != Launch.NonceLength)
        {
            throw new ProtocolException("bad nonce");
        }
        return [.. Magic, .. nonce, .. ticket.Encode()];
    }

    public static Frame.TicketCode Decode(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> expectedNonce)
    {
        if (!bytes.StartsWith(Magic))
        {
            throw new ProtocolException("not a session file");
        }
        var rest = bytes[Magic.Length..];
        if (rest.Length < Launch.NonceLength || !rest[..Launch.NonceLength].SequenceEqual(expectedNonce))
        {
            throw new TicketException("session file belongs to another launch");
        }
        var body = rest[Launch.NonceLength..];
        if (Frame.Decode(body, out var used) is Frame.TicketCode frame && used == body.Length)
        {
            return frame;
        }
        throw new ProtocolException("session file is not one TicketCode frame");
    }
}
