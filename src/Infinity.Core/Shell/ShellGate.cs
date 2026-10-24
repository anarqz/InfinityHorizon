// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>What the shell needs before any page opens (owner's layout of 2026-10-18, as built in <c>79d2e06</c>).</summary>
public enum ShellStep
{
    /// <summary>Sign in to the Infinity account first.</summary>
    SignIn,

    /// <summary>Choose a game.</summary>
    PickGame,

    /// <summary>Choose a profile (profiles come from the server only).</summary>
    PickProfile,

    /// <summary>Every page is open.</summary>
    Ready,
}

/// <summary>The sidebar's pages, top group then bottom group.</summary>
public enum ShellPage
{
    Home,
    News,
    Servers,
    Help,
    Account,
    Donate,
}

/// <summary>
/// Nothing opens before sign-in (or, OD-118, the welcome screen's Play offline) → game → profile; a sign-out or a game change
/// re-locks what depends on it. <see cref="Offline"/>: the player chose to use the launcher without an account; every area opens,
/// online ones say they are offline, and profiles are the local ones.
/// </summary>
public sealed record ShellGate(string? AccountEmail, string? Game, string? ProfileTag, bool Offline = false)
{
    public static ShellGate SignedOut { get; } = new(null, null, null);

    public ShellStep Step => AccountEmail is null && !Offline ? ShellStep.SignIn
        : Game is null ? ShellStep.PickGame
        : ProfileTag is null ? ShellStep.PickProfile
        : ShellStep.Ready;

    /// <summary>An account is signed in (online features); false on the offline path.</summary>
    public bool SignedIn => AccountEmail is not null;

    /// <summary>Every page waits for the profile (HANDOFF_WINDOWS.md §2.1 "UX"); the top bar's sign-out stays available.</summary>
    public bool CanOpen(ShellPage page) => Enum.IsDefined(page) && Step == ShellStep.Ready;

    /// <summary>Signing in replaces the offline path; the local profile chosen offline is chosen again from the account's list.</summary>
    public ShellGate SignIn(string email) => this with { AccountEmail = email, Game = null, ProfileTag = null, Offline = false };

    public ShellGate SignOut() => this with { AccountEmail = null, Game = null, ProfileTag = null, Offline = false };

    /// <summary>OD-118 "Play offline" on the welcome screen: the launcher without an account.</summary>
    public ShellGate PlayOffline() => this with { AccountEmail = null, Game = null, ProfileTag = null, Offline = true };

    /// <summary>A different game drops the profile, which belongs to the game.</summary>
    public ShellGate ChooseGame(string game) => AccountEmail is null && !Offline ? this : this with { Game = game, ProfileTag = game == Game ? ProfileTag : null };

    public ShellGate ChooseProfile(string tag) => Game is null ? this : this with { ProfileTag = tag };
}
