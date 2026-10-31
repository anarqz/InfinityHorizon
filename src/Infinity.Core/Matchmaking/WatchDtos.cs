// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json.Serialization;

namespace Infinity.Core.Matchmaking;

/// <summary>One limit of <see cref="WatchRules"/>: over <see cref="MaxKmh"/> for at least <see cref="SustainMs"/> (MATCHMAKING_API §3.9).</summary>
public sealed record WatchLimit
{
    public required double MaxKmh { get; init; }

    public required uint SustainMs { get; init; }
}

/// <summary><see cref="WatchRules.Jump"/>: one sample-to-sample move past <see cref="MaxMeters"/> (stored, never counted).</summary>
public sealed record WatchJump
{
    public required double MaxMeters { get; init; }
}

/// <summary>
/// <c>GET /mm/v1/watch</c> → 200 (§3.9, OD-147/OD-158): what the other players' cars are measured against in a Hardcore world.
/// The client never compiles these in: it reads them once per world and hands them to its hook and its world server.
/// </summary>
public sealed record WatchRules
{
    public required uint Version { get; init; }

    public required WatchLimit Speed { get; init; }

    public required WatchLimit Climb { get; init; }

    public required WatchJump Jump { get; init; }

    /// <summary>Every value usable: a version, positive limits within the contract's ranges.</summary>
    [JsonIgnore]
    public bool Valid => Version > 0 && Speed is { MaxKmh: > 0 and <= 5000, SustainMs: > 0 and <= 3_600_000 }
        && Climb is { MaxKmh: > 0 and <= 5000, SustainMs: > 0 and <= 3_600_000 } && Jump is { MaxMeters: > 0 and <= 1_000_000 }
        && double.IsFinite(Speed.MaxKmh) && double.IsFinite(Climb.MaxKmh) && double.IsFinite(Jump.MaxMeters);
}

/// <summary><see cref="PeerObservation.Kind"/> (§3.9): <c>jump</c> and <c>clock</c> are stored and never count.</summary>
public static class ObservationKinds
{
    public const string Speed = "speed";
    public const string Climb = "climb";
    public const string Car = "car";
    public const string Parts = "parts";
    public const string Jump = "jump";
    public const string Clock = "clock";

    public static IReadOnlyList<string> All { get; } = [Speed, Climb, Car, Parts, Jump, Clock];
}

/// <summary>
/// <c>POST /mm/v1/worlds/{worldId}/observations</c> body → 204 (§3.9): what this player saw of another player's car.
/// <see cref="Id"/> is client-minted and the idempotency key (a retry sends the same one).
/// </summary>
public sealed record PeerObservation
{
    public required string Id { get; init; }

    /// <summary>The observed player, decimal; never the reporter.</summary>
    public required string Xuid { get; init; }

    /// <summary>One of <see cref="ObservationKinds"/>.</summary>
    public required string Kind { get; init; }

    /// <summary>speed/climb: the peak km/h; jump: metres; clock: the sender/receiver clock ratio.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public double? Value { get; init; }

    /// <summary>speed/climb only: how long it stayed over the limit, ms.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public uint? SustainMs { get; init; }

    public required uint RulesVersion { get; init; }
}
