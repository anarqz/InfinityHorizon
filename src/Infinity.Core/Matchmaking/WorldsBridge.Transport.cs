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

    private sealed record NoticeWorld(string Id, string? Modes, string Transport);
    private sealed record SeatWire(string Grant, string? Transport);
    private NoticeWorld? _noticeWorld;
    private SeatWire? _seatWire;

    /// <summary>OD-160's notice inputs: a world as the server shows it (<c>modes</c>, absent from older servers: its <c>mode</c>).</summary>
    private void ObserveNoticeWorld(WorldView world) => Volatile.Write(ref _noticeWorld, new(world.Id, world.Modes ?? world.Mode, world.Transport));

    /// <summary>OD-160's notice inputs: the seat's transport as the server last gave it (join answer, seat read, renew).</summary>
    private void ObserveSeatTransport(string grant, string? transport) => Volatile.Write(ref _seatWire, new(grant, transport));

    /// <summary>
    /// OD-160's lobby notice (MATCHMAKING_API §5b "The lobby notice"): true the relay notice, false the direct one, null none. A Noob-only
    /// world (or no seat or world, or a world not read yet) shows none; a Hardcore world (modes hardcore/any) shows the relay notice only
    /// while the server still says the world and this seat are on a relay (an older player world), else the direct notice.
    /// </summary>
    private bool? RelayNotice()
    {
        var worldId = _seat.GrantId is not null ? Volatile.Read(ref _seatWorld) : null;
        if (worldId is null || Volatile.Read(ref _noticeWorld) is not { } world || world.Id != worldId || !WorldModes.HardcoreWorld(world.Modes)) return null;
        if (world.Transport != Transports.Relay) return false;
        return !(Volatile.Read(ref _seatWire) is { } wire && wire.Grant == _seat.GrantId && wire.Transport == Transports.Direct);
    }

    /// <summary>OD-160's lobby notice keys: the direct one is said in one place per language (en.json, pt-BR.json).</summary>
    internal const string NoticeRelayKey = "worlds.notice_relay", NoticeDirectKey = "worlds.notice_direct";

    /// <summary>OD-160: the lobby notice text, or null (none).</summary>
    private string? ConnectionNotice() => RelayNotice() switch
    {
        true => _text(NoticeRelayKey),
        false => _text(NoticeDirectKey),
        null => null,
    };

    /// <summary>"My world" with OD-160's notice (computed when sent); none: unchanged.</summary>
    private UiFrame.MyWorld WithConnection(UiFrame.MyWorld mine)
    {
        if (mine.Role == UiWorldRole.None || RelayNotice() is not { } relay) return mine;
        return mine with { Transport = relay ? UiTransport.Relay : UiTransport.Direct, Notice = _text(relay ? NoticeRelayKey : NoticeDirectKey) };
    }

    /// <summary>"Direct" for the connection details while a seat is held (OD-208: the launcher never selects a relay); null without one.</summary>
    private string? TransportText() => _seat.GrantId is null ? null : _text("connection.direct");
}
