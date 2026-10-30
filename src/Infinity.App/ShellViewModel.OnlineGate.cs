// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// OD-118, "not a single request to Infinity offline": the launcher is offline from its start until the player clicks SIGN IN, and
/// again from Play offline on. Only then (or when the last run was online and kept a session, which resumes) is the service status
/// read, the release floor fetched (/launcher/v1/info) and the IdP used. Every Infinity client goes through <see cref="InfinityGate"/>,
/// which refuses anything sent while offline. The GitHub release discovery and the mod catalog and downloads are not Infinity hosts
/// and stay available offline.
/// </summary>
public sealed partial class ShellViewModel
{
    private readonly HttpClient Http;
    private readonly Lazy<HttpClient> ProductionMatchmakingHttp;

    /// <summary>
    /// CR-S40: matchmaking over IPv4 (A records only), so the server observes the same address family as the IPv4 STUN/UPnP
    /// external port. Every HTTPS origin uses it, a custom choice of the production server included: with IPv6 internet
    /// the server saw an IPv6 address beside an IPv4 port and refused the world's endpoint. Only a plain-HTTP development
    /// stack on the LAN keeps the default client.
    /// </summary>
    private HttpClient MatchmakingHttp(Uri matchmaking) =>
        _config.Production || matchmaking.Scheme == Uri.UriSchemeHttps ? ProductionMatchmakingHttp.Value : Http;
    private bool _onlineEngaged, _onlineAnswered, _onOfflineStatus;

    /// <summary>OD-118: an offline game has exited and the launcher is finishing locally: no Infinity request, signed in or not.</summary>
    private bool _offlineWrapUp;

    /// <summary>The handler in front of every Infinity client (counts and refuses; tests stub it).</summary>
    internal InfinityRequestGate InfinityGate { get; }

    /// <summary>Infinity hosts may be contacted: the player clicked SIGN IN, or resumed a session chosen online.</summary>
    public bool OnlineEngaged => _onlineEngaged;

    /// <summary>The last run ended online with a session kept (G2): this start resumes it. Otherwise nothing is sent until SIGN IN.</summary>
    public bool StartsOnline => LaunchModeStore.Read(_stateDirectory) == LaunchModeStore.Online && SessionStore.Exists(_stateDirectory);

    /// <summary>The last run chose Play offline (G2): this start opens offline, with no request and no token refresh.</summary>
    public bool StartsOffline => LaunchModeStore.Read(_stateDirectory) == LaunchModeStore.Offline;

    private InfinityRequestGate CreateInfinityGate(HttpMessageHandler transport)
    {
        var gate = new InfinityRequestGate(() => _onlineEngaged && !_offlineWrapUp, Log, DevServer, transport);
        gate.Violation += what => Message = "DEV: offline mode refused " + what;
        return gate;
    }

    /// <summary>The production matchmaking transport's own gate (same rule; a DelegatingHandler has one transport).</summary>
    internal InfinityRequestGate? MatchmakingGate { get; private set; }

    private InfinityRequestGate CreateMatchmakingGate(HttpMessageHandler transport) => MatchmakingGate = CreateInfinityGate(transport);

    /// <summary>The player asked for the online side (SIGN IN, or a resumed online session): the first Infinity requests may follow.</summary>
    private void EngageOnline()
    {
        if (_onlineEngaged) return;
        _onlineEngaged = true;
        _onlineEngagement++;
        Log("online: engaged (the player chose Sign in); service status and release floor are read now");
        Changed(string.Empty);
    }

    /// <summary>
    /// OD-118 T1h: the start's own folder search may open Configure when Azimuth is not ready, but not under a player who already
    /// landed on PLAY offline (a remembered offline start): the page never switches by itself; PLAY shows the setup reason instead.
    /// First runs and signed-in starts keep opening Configure.
    /// </summary>
    internal bool OpenSetupOnStart => !OfflineMode;

    /// <summary>A start that resumes the online run's kept session (G2): the player chose online last time.</summary>
    public void ResumeOnline() => EngageOnline();

    /// <summary>Back to offline (Play offline, sign-out): nothing more is sent until the next SIGN IN.</summary>
    private void DisengageOnline()
    {
        _onlineEngaged = false;
        _onlineEngagement++;
        _onlineWaiting = false;
        _onlineAnswered = false;
        _onlineStarted = false;
        _onlineStatus = null;
        _onlineViewing = _onlineSupport = false;
        KeepOfflineUpdatePhase(); // T1c: a failed or pending /info never holds the offline launcher at the update gate
        Changed(string.Empty);
    }

    private void RememberLastLocalProfile(string id)
    {
        try { LaunchModeStore.WriteLastProfile(_stateDirectory, id); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException) { Log("shell: last profile not kept: " + e.GetType().Name); }
    }

    private void RememberLaunchMode(string mode)
    {
        try { LaunchModeStore.Write(_stateDirectory, mode); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("shell: launch mode not kept: " + e.GetType().Name); }
    }

    /// <summary>
    /// The launcher's start after the window loads (OD-46 folder search; OD-118 G1/G2). No Infinity request unless the last run ended
    /// online with a kept session: then the service status, the release floor and the resume, as before. A run that chose Play offline
    /// opens offline again. GitHub's release list is read either way.
    /// </summary>
    public async Task StartupAsync(string? azimuth)
    {
        var online = StartsOnline;
        if (online) ResumeOnline();
        // A run that chose Play offline opens offline at once, before any check, so a SIGN IN click meanwhile is never undone.
        else if (StartsOffline) PlayOffline();
        // T1c: the status first; the update check after it (it reads /launcher/v1/info only when engaged and the service is open).
        if (online) await CheckOnlineStatusAsync(manual: false);
        var onlineCheck = Task.CompletedTask;
        var updateCheck = ProbeServerAsync();
        _ = LoadNewsAsync(); // OD-119: GitHub only, signed out and offline too (the last good copy first)
        await VerifyBuildOriginAsync();
        if (azimuth is not null) Install = Infinity.Core.Process.AzimuthInstall.Detect(azimuth);
        else await FindAzimuthAsync(null, openSetupWhenNeeded: OpenSetupOnStart);
        // Owner 2026-10-27: Azimuth before sign-in; none found, its setup is the first screen (local only, nothing sent: OD-118).
        AskAzimuthFirstWhenMissing();
        await Task.WhenAll(updateCheck, onlineCheck);
        if (online) await ResumeSessionAsync();
        await RefreshIntegrityAutomaticallyAsync();
    }
}