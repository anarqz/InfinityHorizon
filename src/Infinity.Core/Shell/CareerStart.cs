// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Saves;

namespace Infinity.Core.Shell;

/// <summary>
/// What a Start card offers (CAREERS.md §4, §6.2, §6.7). <see cref="Sources"/>: the account's other live profiles' careers of the same game
/// (<see cref="ProfileSeed.Sources"/>), the only "existing profile" a career starts from; never another game, never Azimuth's own save for
/// Hardcore (B2).
/// </summary>
public sealed record CareerOffers(bool Fresh, bool FromExisting, IReadOnlyList<Profile> Sources)
{
    public static CareerOffers Nothing { get; } = new(false, false, []);

    public bool Any => Fresh || FromExisting;
}

/// <summary>How <c>POST /players/{xuid}/careers</c> ended for the launcher (CAREERS.md §3, §6.12; plan §5).</summary>
public enum CareerCreateOutcome
{
    /// <summary>201: the empty career exists now.</summary>
    Created,

    /// <summary>409 <c>ACCOUNTS.CAREER_EXISTS</c>: it existed already (a lost 201, another PC): refresh <c>/me</c> and continue.</summary>
    Exists,

    /// <summary>400 <c>profile.mode</c>: the game allows no career of the profile's mode (FM5 Hardcore).</summary>
    ModeNotAllowed,

    /// <summary>400 <c>profile.game</c>: an unknown or closed game.</summary>
    GameClosed,

    /// <summary>404: the profile is gone (deleted elsewhere): refresh.</summary>
    ProfileGone,

    /// <summary>403 <c>ACCOUNTS.BANNED</c>.</summary>
    Banned,

    /// <summary>409 <c>ACCOUNTS.DELETION_PENDING</c>.</summary>
    DeletionPending,

    /// <summary>409 <c>REQUEST.IDEMPOTENCY_KEY_REUSED</c>: a new key, one retry.</summary>
    KeyReused,

    /// <summary>5xx, a transport failure or anything else: keep the key, try again later.</summary>
    Unavailable,
}

/// <summary>OD-214: the one tested place for what starting a career offers and how its creation answers read.</summary>
public static class CareerStart
{
    /// <summary>
    /// The Start card of a <paramref name="mode"/> profile in <paramref name="game"/>. Hardcore: none when the game allows no Hardcore career
    /// (§6.2); Fresh always; "from an existing profile" only on a game with cloud saves, while the server allows it and the profile keeps a
    /// cloud line (<see cref="ProfileSeed.Offered"/>, §4.10, OD-214) and the account has another profile's career of the game. Noob: Fresh;
    /// "from a save on this PC" on a game with cloud saves only when an Azimuth install with a readable save of the game exists (OD-41, review L1). FM5 Noob: Fresh only (no cloud saves).
    /// </summary>
    public static CareerOffers Offers(string mode, GameDefinition game, BoardFlags flags, IReadOnlyList<Profile> sources)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(flags);
        ArgumentNullException.ThrowIfNull(sources);
        var sameGame = sources.Where(s => s.Game == game.Id).ToList();
        if (mode == "hardcore")
        {
            if (!game.HardcoreCareers) return CareerOffers.Nothing;
            var seeded = game.FullFeatures && ProfileSeed.Offered(flags.HardcoreFromProfile, flags.HardcoreKeepsCloud) && sameGame.Count > 0;
            return new CareerOffers(true, seeded, seeded ? sameGame : []);
        }
        // review L1: the flow runs the OD-41 local picker only, so it is offered only with a readable Azimuth save of the game on this PC
        var copy = game.FullFeatures && flags.AzimuthSaves;
        return new CareerOffers(true, copy, game.FullFeatures ? sameGame : []);
    }

    /// <summary>Reads a refusal of <c>POST /players/{xuid}/careers</c>; null error = 201.</summary>
    public static CareerCreateOutcome Classify(Exception? error) => error switch
    {
        null => CareerCreateOutcome.Created,
        ApiException { Code: ErrorCodes.AccountsCareerExists } => CareerCreateOutcome.Exists,
        ApiException { Code: ErrorCodes.RequestInvalid } e when Field(e) == "profile.mode" => CareerCreateOutcome.ModeNotAllowed,
        ApiException { Code: ErrorCodes.RequestInvalid } e when Field(e) == "profile.game" => CareerCreateOutcome.GameClosed,
        ApiException { Code: ErrorCodes.AccountsBanned } => CareerCreateOutcome.Banned,
        ApiException { Code: ErrorCodes.AccountsDeletionPending } => CareerCreateOutcome.DeletionPending,
        ApiException { Code: ErrorCodes.RequestIdempotencyKeyReused } => CareerCreateOutcome.KeyReused,
        ApiException { Status: HttpStatusCode.NotFound } => CareerCreateOutcome.ProfileGone,
        _ => CareerCreateOutcome.Unavailable,
    };

    /// <summary>The career exists after this outcome (the start goes on).</summary>
    public static bool Continues(CareerCreateOutcome outcome) => outcome is CareerCreateOutcome.Created or CareerCreateOutcome.Exists;

    /// <summary>The career of <paramref name="game"/> on <paramref name="xuid"/>'s profile in a fresh <c>/me</c>, as its <see cref="Profile"/> row.</summary>
    public static Profile? CareerRow(Me me, string xuid, string game)
    {
        ArgumentNullException.ThrowIfNull(me);
        var careerId = me.Players?.FirstOrDefault(p => p.Xuid == xuid)?.Careers.FirstOrDefault(c => c.Game == game)?.CareerId;
        return careerId is not null
            ? me.Profiles.FirstOrDefault(p => p.ProfileId == careerId)
            : me.Profiles.FirstOrDefault(p => p.Xuid == xuid && p.Game == game && p.Status == "active");
    }

    private static string? Field(ApiException e) => ProfileSeed.Detail(e.Details, "field");
}
