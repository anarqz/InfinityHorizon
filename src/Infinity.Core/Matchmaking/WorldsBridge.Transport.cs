// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// The seat's transport as the overlay and the connection details say it. OD-208: an official world is joined direct only
/// (WORLDS_OFFICIAL §3; a relay join or renew is 409 <c>official_direct</c>), so the launcher pushes no relay leg, answers no relay
/// fallback and selects no relay transport; the seat's grant (its legacy "relay" name) and its FIUP socket proof stay (Proof.cs).
/// </summary>
public sealed partial class WorldsBridge
{
    /// <summary>T35: the joiner's log line when the server shows its seat <paramref name="grant"/> ended, with the server's reason.</summary>
    private string SeatEndedLine(SeatEnded? ended, string grant)
    {
        var world = Volatile.Read(ref _seatWorld) ?? "its world";
        var reason = ended is { } end && end.GrantId == grant ? end.Reason switch
        {
            SeatEnds.Released => "released: the server no longer lists this player, or the seat was let go",
            SeatEnds.Expired => "expired: the reserved seat was never used in time",
            SeatEnds.Kicked => "kicked",
            SeatEnds.WorldClosed => "its world closed",
            SeatEnds.Pool => "moved to another pool",
            var other => other,
        } : "the server shows no seat";
        return $"worlds: the seat in {world} ended ({reason}); its context is cleared";
    }

    /// <summary>"Direct" for the connection details while a seat is held (OD-208: the launcher never selects a relay); null without one.</summary>
    private string? TransportText() => _seat.GrantId is null ? null : _text("connection.direct");
}
