// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Integrity;

/// <summary>What Play does about the critical-file check before it starts a session.</summary>
public enum IntegrityStep
{
    /// <summary>This profile has no integrity check: Play declares no car database and binds no scan.</summary>
    NotRequired,
    /// <summary>The current compatible check (its files unchanged) is reused; admission rebinds it to the session.</summary>
    Ready,
    /// <summary>A silent check runs first (remembered digests of unchanged files are reused).</summary>
    Check,
}

/// <summary>What a finished check means for Play.</summary>
public enum IntegrityOutcome
{
    /// <summary>Compatible: Play goes on.</summary>
    Ready,
    /// <summary>A real mismatch (an incompatible install): shown with its message.</summary>
    Mismatch,
    /// <summary>Could not be verified now (unreadable file, server unavailable, cancelled): Play stops with a message and
    /// the next Play checks again; never a configuration prompt.</summary>
    Unverified,
}

/// <summary>
/// OD-113: the online integrity check (the short critical-file scan and the server's content and car-database checks)
/// applies only to a Supporter (<c>vip</c>) + Hardcore profile, never to a dedicated host (OD-109). It runs in the
/// background; the player is never asked to verify again, and only a real mismatch is shown.
/// </summary>
public static class IntegrityPolicy
{
    /// <summary>Whether a session of this account tier and profile mode is bound to the integrity check.</summary>
    public static bool Applies(string? tier, string? mode, bool dedicated = false) => !dedicated && tier == "vip" && mode == "hardcore";

    /// <summary>Before Play: reuse a current compatible check, or check silently first.</summary>
    public static IntegrityStep BeforePlay(bool applies, InstallationState? state, bool filesCurrent)
        => !applies ? IntegrityStep.NotRequired : state == InstallationState.Compatible && filesCurrent ? IntegrityStep.Ready : IntegrityStep.Check;

    /// <summary>After a check: compatible with a car database digest, a real mismatch, or unverified.</summary>
    public static IntegrityOutcome After(InstallationState? state, bool filesCurrent, string? carDbSha256)
        => state == InstallationState.Incompatible ? IntegrityOutcome.Mismatch
            : state == InstallationState.Compatible && filesCurrent && carDbSha256 is { Length: 64 } ? IntegrityOutcome.Ready
            : IntegrityOutcome.Unverified;

    /// <summary>
    /// Whether Configure needs the player: only a profile with the check, with no game folder chosen or a real mismatch.
    /// A pending, running, expired or inconclusive check never does (it is redone silently).
    /// </summary>
    public static bool NeedsConfiguration(bool applies, bool contentLocationSet, InstallationState? state)
        => applies && (!contentLocationSet || state == InstallationState.Incompatible);

    /// <summary>Whether a content refusal of the server during a session is a real mismatch (shown) rather than a re-check (silent).</summary>
    public static bool IsMismatch(string? errorCode) => errorCode == ErrorCodes.ContentIncompatible;
}
