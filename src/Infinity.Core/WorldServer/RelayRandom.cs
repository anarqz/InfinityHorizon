// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

/// <summary>The seeds a world draws when its settings leave them null (<see cref="RelaySettings"/>).</summary>
internal enum RelaySeed : byte { Weather, Tag, Team }

/// <summary>
/// Every random value a <see cref="RelayEngine"/> draws: the seeds its settings leave null, its OpenConnection attempt ids and
/// the transaction-id bytes of the STUN datagrams it writes. A world uses the OS's cryptographic generator
/// (<see cref="CryptoRelayRandom"/>); a test can pass a seeded source so that a run replays exactly.
/// </summary>
internal interface IRelayRandom
{
    /// <summary>A seed for a <see cref="Random"/> sequence, in [0, <see cref="int.MaxValue"/>).</summary>
    int Seed(RelaySeed purpose);

    /// <summary>The engine's own OpenConnection attempt id of a new peer.</summary>
    uint Attempt();

    /// <summary>Fills a STUN transaction id (or its random tail).</summary>
    void Fill(Span<byte> transaction);
}

/// <summary>The engine's default randomness: the OS's cryptographic generator.</summary>
internal sealed class CryptoRelayRandom : IRelayRandom
{
    internal static CryptoRelayRandom Instance { get; } = new();

    private CryptoRelayRandom() { }

    public int Seed(RelaySeed purpose) => RandomNumberGenerator.GetInt32(int.MaxValue);

    public uint Attempt() => BitConverter.ToUInt32(RandomNumberGenerator.GetBytes(4));

    public void Fill(Span<byte> transaction) => RandomNumberGenerator.Fill(transaction);
}
