// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>What the shell needs before any page opens (owner's layout of 2026-10-18, as built in <c>79d2e06</c>).</summary>
public enum ShellStep
{
    /// <summary>Sign in to the Infinity account first.</summary>
    SignIn,

    /// <summary>No game yet. Owner 2026-11-08: no page asks for it; the launcher chooses one at sign-in (the last one), PLAY's tiles change it.</summary>
    PickGame,

    /// <summary>A game, no profile chosen yet: PLAY is open (its PLAY HARDCORE / PLAY NOOB buttons choose the profile as they launch).</summary>
    PickProfile,

    /// <summary>Every page is open.</summary>
    Ready,
}

/// <summary>The sidebar's pages, top group then bottom group.</summary>
public enum ShellPage
{
    Home,

    /// <summary>Owner 2026-11-08: profile management only (no Play, no Start); the save tools act on PLAY's game.</summary>
    Profiles,
    News,
    Servers,
    Help,
    Account,
    Donate,
}

/// <summary>
/// Nothing opens before sign-in (or, OD-118, the welcome screen's Play offline) → game → profile; a sign-out or a game change
/// re-locks what depends on it. <see cref="Offline"/>: the player chose to use the launcher without an account; every area opens,
/// online ones say they are offline, and profiles are the local ones. <see cref="ProfileTag"/> is the chosen profile's shown name;
/// <see cref="ProfileKey"/> is what finds it again (its row id: the account's profileId, or the local profile's id), never the name,
/// which a server rename changes (OD-120).
/// </summary>
public sealed record ShellGate(string? AccountEmail, string? Game, string? ProfileTag, bool Offline = false, string? ProfileKey = null)
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
    /// <remarks>Owner 2026-11-08: with <paramref name="game"/> (the last one played, else the first offered) no page asks for a game.</remarks>
    public ShellGate SignIn(string email, string? game = null) => this with { AccountEmail = email, Game = game, ProfileTag = null, ProfileKey = null, Offline = false };

    public ShellGate SignOut() => this with { AccountEmail = null, Game = null, ProfileTag = null, ProfileKey = null, Offline = false };

    /// <summary>OD-118 "Play offline" on the welcome screen: the launcher without an account.</summary>
    public ShellGate PlayOffline(string? game = null) => this with { AccountEmail = null, Game = game, ProfileTag = null, ProfileKey = null, Offline = true };

    /// <summary>A different game drops the profile, which belongs to the game.</summary>
    public ShellGate ChooseGame(string game) => AccountEmail is null && !Offline ? this : this with { Game = game, ProfileTag = game == Game ? ProfileTag : null, ProfileKey = game == Game ? ProfileKey : null };

    public ShellGate ChooseProfile(string tag, string? key = null) => Game is null ? this : this with { ProfileTag = tag, ProfileKey = key };
}

/// <summary>Owner 2026-11-08: the shell's frame around the pages (header, sidebar, the right column).</summary>
public static class ShellLayout
{
    /// <summary>
    /// The FRIENDS sidebar on the right, at shell level (any page): open when the header's friends button opened it, and only signed in
    /// (offline has no friends to read). Owner 2026-11-08 (second pass): no longer tied to PLAY.
    /// </summary>
    public static bool FriendsSidebar(bool open, bool signedIn) => open && signedIn;

    /// <summary>The header's friends button shows signed in only.</summary>
    public static bool FriendsButton(bool signedIn) => signedIn;
}
