// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>The next thing a resumed online start does (owner 2026-11-10, "1. Opened launcher").</summary>
public enum StartupStep
{
    /// <summary>Read the service status (<c>/core/v1/online-status</c>).</summary>
    CheckServices,
    /// <summary>The status is closed or paused: the Online Services half screen, nothing else is read.</summary>
    ServicesClosed,
    /// <summary>Read the release floor (<c>/launcher/v1/info</c> minLauncher).</summary>
    CheckLauncherVersion,
    /// <summary>The floor is above this launcher: the forced update screen, no sign-in.</summary>
    ForcedUpdate,
    /// <summary>Find this PC's games (the Azimuth folder) before the profile, so PLAY's tiles and careers have their files.</summary>
    FindGames,
    /// <summary>Sign in with the kept session (refresh token), then <c>/me</c>.</summary>
    SignIn,
    /// <summary>The account's profiles, careers and this PC's cloud-save state, and the friends list: the main pane's data.</summary>
    LoadProfile,
    /// <summary>Everything the main pane shows is loaded: PLAY opens on the last game, its cards filled.</summary>
    Shell,
}

/// <summary>What the start knows so far; null is "not read yet".</summary>
/// <param name="LauncherFloorUnknown">
/// The release floor could not be read (<c>/launcher/v1/info</c> failed): not a forced update. AUTO_UPDATE.md: a backend policy failure
/// keeps Play restricted until the policy is readable, while account operations (the sign-in) stay available; the launcher says so
/// with a retry.
/// </param>
public sealed record StartupFacts(bool? ServicesOpen = null, bool? LauncherAllowed = null, bool GamesFound = false, bool? SignedIn = null,
    bool ProfileLoaded = false, bool LauncherFloorUnknown = false);

/// <summary>
/// Owner 2026-11-10, the startup order of a launcher that resumes its online session: the service status, then the release floor
/// (forced update when minLauncher is above this build), then this PC's games, the sign-in and the profile data (profiles, careers,
/// cloud-save state, friends), and only then the main pane. A closed service or a forced update stops the start on its own screen; a
/// sign-in that fails ends on the welcome screen with its reason (the caller says it, never silently).
/// </summary>
public static class StartupFlow
{
    public static StartupStep Next(StartupFacts facts)
    {
        ArgumentNullException.ThrowIfNull(facts);
        if (facts.ServicesOpen is null) return StartupStep.CheckServices;
        if (facts.ServicesOpen == false) return StartupStep.ServicesClosed;
        if (facts.LauncherAllowed is null) return StartupStep.CheckLauncherVersion;
        // an unreadable floor is no verdict: the start goes on to the sign-in, Play waits for a readable floor (never silently)
        if (facts.LauncherAllowed == false && !facts.LauncherFloorUnknown) return StartupStep.ForcedUpdate;
        if (!facts.GamesFound) return StartupStep.FindGames;
        if (facts.SignedIn is null) return StartupStep.SignIn;
        if (facts.SignedIn == false) return StartupStep.Shell; // the welcome screen, with the reason
        return facts.ProfileLoaded ? StartupStep.Shell : StartupStep.LoadProfile;
    }

    /// <summary>A step that ends the start (nothing more is read).</summary>
    public static bool IsFinal(StartupStep step) => step is StartupStep.ServicesClosed or StartupStep.ForcedUpdate or StartupStep.Shell;

    /// <summary>
    /// A final step that only stops the start (a closed service, a forced update): the start resumes from it when that screen clears
    /// (<see cref="Reopened"/>), so a later "Check again" that succeeds signs the kept session in instead of leaving the welcome screen
    /// on "Signing in…".
    /// </summary>
    public static bool IsStop(StartupStep step) => step is StartupStep.ServicesClosed or StartupStep.ForcedUpdate;

    /// <summary>
    /// The facts a stopped start continues from once its screen cleared: an open service is read again for the floor, a floor that now
    /// allows this launcher goes on to this PC's games and the sign-in. Null when the stop still holds.
    /// </summary>
    /// <param name="floorUnknown">The last floor read failed (no verdict): a forced update no longer holds, the start signs in.</param>
    public static StartupFacts? Reopened(StartupFacts stopped, bool servicesOpen, bool launcherAllowed, bool floorUnknown = false)
    {
        ArgumentNullException.ThrowIfNull(stopped);
        return Next(stopped) switch
        {
            StartupStep.ServicesClosed when servicesOpen => stopped with { ServicesOpen = true, LauncherAllowed = null, LauncherFloorUnknown = false },
            StartupStep.ForcedUpdate when servicesOpen && (launcherAllowed || floorUnknown) =>
                stopped with { LauncherAllowed = launcherAllowed, LauncherFloorUnknown = !launcherAllowed && floorUnknown },
            _ => null,
        };
    }
}
