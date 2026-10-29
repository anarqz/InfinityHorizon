// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Windows.Threading;
using Infinity.Core.Api;
using Infinity.Core.Overlay;
using Infinity.Core.Presence;
using Infinity.Core.Shell;
using Infinity.Core.Supervisor;

namespace Infinity.App;

/// <summary>
/// OD-136 (CR-L41.1): the player's Discord card, built into the launcher and on by default. Composed every few seconds from the
/// launcher's state (the active profile, the play session, the overlay bridge's "my world", the hook's readiness, level, credits and
/// car), sent through <see cref="DiscordPresenceService"/> (one send per 15 s at most, unchanged cards never re-sent). Settings →
/// Discord keeps a master switch and a toggle per field in the state folder (per install).
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>Bullet4fun's Discord RPC (the catalog's <c>discord-rpc</c>): duplicates this presence (DISCORD_PRESENCE.md §3).</summary>
    internal const string DuplicateDiscordModId = "discord-rpc";
    private static readonly TimeSpan PresenceTick = TimeSpan.FromSeconds(3);
    private static readonly TimeSpan ServerPresenceEvery = TimeSpan.FromMinutes(5);
    private static readonly TimeSpan PresenceWorldPollEvery = TimeSpan.FromSeconds(30);

    private DiscordPresenceService? _presence;
    private PresenceToggles? _presenceToggles;
    private DispatcherTimer? _presenceTimer;
    private readonly DateTimeOffset _launcherStarted = DateTimeOffset.UtcNow;
    private DateTimeOffset? _gameStarted;
    private uint? _hookLevel;
    private ulong? _hookCredits;
    private string? _hookCarName;
    private readonly Dictionary<int, string?> _carNames = []; // per session: an ordinal is read from the install once

    /// <summary>
    /// T10 (CR-L41.4): the car's "<c>Year Make Model</c>" for <paramref name="ordinal"/> from the player's own game database, in the
    /// launcher's language, cached per session; null when unknown. Read only on the player's PC and sent only to their own Discord.
    /// </summary>
    private string? CarName(int ordinal)
    {
        if (ordinal <= 0 || _install is not { } install) return null;
        if (_carNames.TryGetValue(ordinal, out var known)) return known;
        string? name = null;
        try { name = ResolveCarName(install.Root, ordinal, S.Locale); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException or InvalidOperationException)
        { Log("discord: car name unavailable (" + error.GetType().Name + ")"); }
        return _carNames[ordinal] = name;
    }

    /// <summary>The resolver seam: Infinity.Core.Process.CarNames (T10) over the remembered game folder's Mount; the key is Data_Car.Id × 1000.</summary>
    private string? ResolveCarName(string azimuthRoot, int carKey, string locale) =>
        azimuthRoot.Length == 0 || ContentLocation?.Mount is not { } mount ? null : new Infinity.Core.Process.CarNames(mount, locale).Name(carKey);
    private (string Xuid, ProfilePresence Value)? _serverPresence;
    private CancellationTokenSource? _serverPresencePoll;
    private long _serverPresenceNextTicks;
    private bool _presenceOverlay;
    private bool _presenceClearPending;
    private DateTimeOffset _presenceWorldPolled = DateTimeOffset.MinValue;

    private PresenceToggles PresenceToggles => _presenceToggles ??= PresenceToggles.Load(_stateDirectory);

    public bool PresenceEnabled { get => PresenceToggles.Enabled; set => SetPresenceToggles(PresenceToggles with { Enabled = value }); }
    public bool PresenceClub { get => PresenceToggles.Club; set => SetPresenceToggles(PresenceToggles with { Club = value }); }
    public bool PresenceMode { get => PresenceToggles.Mode; set => SetPresenceToggles(PresenceToggles with { Mode = value }); }
    public bool PresenceName { get => PresenceToggles.Name; set => SetPresenceToggles(PresenceToggles with { Name = value }); }
    public bool PresenceLevel { get => PresenceToggles.Level; set => SetPresenceToggles(PresenceToggles with { Level = value }); }
    public bool PresenceCredits { get => PresenceToggles.Credits; set => SetPresenceToggles(PresenceToggles with { Credits = value }); }
    public bool PresenceCar { get => PresenceToggles.Car; set => SetPresenceToggles(PresenceToggles with { Car = value }); }
    public bool PresenceLobby { get => PresenceToggles.Lobby; set => SetPresenceToggles(PresenceToggles with { Lobby = value }); }
    public bool PresenceJoin { get => PresenceToggles.Join; set => SetPresenceToggles(PresenceToggles with { Join = value }); }

    private void SetPresenceToggles(PresenceToggles next)
    {
        if (next == PresenceToggles) return;
        _presenceToggles = next;
        try { next.Save(_stateDirectory); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { Log("discord: settings not saved (" + error.GetType().Name + ")"); }
        foreach (var name in new[] { nameof(PresenceEnabled), nameof(PresenceClub), nameof(PresenceMode), nameof(PresenceName), nameof(PresenceLevel),
            nameof(PresenceCredits), nameof(PresenceCar), nameof(PresenceLobby), nameof(PresenceJoin) }) Changed(name);
        RefreshPresence(); // the card follows at the next coalesced send
    }

    /// <summary>At startup: the presence follows the launcher from here until it exits.</summary>
    internal void StartPresence()
    {
        if (_presenceTimer is not null) return;
        _presenceTimer = new DispatcherTimer(PresenceTick, DispatcherPriority.Background, (_, _) => RefreshPresence(), Dispatcher.CurrentDispatcher);
        _presenceTimer.Start();
        RefreshPresence();
    }

    /// <summary>The launcher is closing: the card is cleared now (bounded) and the pipe closed.</summary>
    private void StopPresenceForExit()
    {
        _presenceTimer?.Stop();
        _presenceTimer = null;
        _serverPresencePoll?.Cancel();
        if (Interlocked.Exchange(ref _presence, null) is { } presence)
        {
            try { Task.Run(() => presence.DisposeAsync().AsTask()).Wait(TimeSpan.FromSeconds(3)); }
            catch (AggregateException) { }
        }
    }

    private void RefreshPresence()
    {
        if (!PresenceToggles.Enabled)
        {
            // master off: cleared at once and the pipe closed; nothing talks to Discord until it is on again
            if (Interlocked.Exchange(ref _presence, null) is { } stopped) _ = stopped.StopAsync(TimeSpan.FromSeconds(2));
            return;
        }
        PollPresenceWorld();
        TryPendingJoin(); // CR-L41.2: a link waiting for the game to load
        var inputs = PresenceInputsNow();
        if (inputs is null && _presence is null) return; // nothing to show and nothing shown: Discord is never touched
        _presence ??= DiscordPresenceService.ForDiscord(Log);
        if (_presenceClearPending)
        {
            // the game exited: the clear goes out first (at the next allowed send), then the launcher's card
            if (_presence.Rules.Connected && _presence.Rules.Shown is not null) { _presence.Want((string?)null); return; }
            _presenceClearPending = false;
        }
        _presence.Want(inputs is null ? null : PresenceComposer.Compose(inputs, PresenceToggles, key => S[key], CultureInfo.GetCultureInfo(S.Locale)));
    }

    /// <summary>The card's inputs now, or null when no profile is chosen (no card).</summary>
    private PresenceInputs? PresenceInputsNow()
    {
        if (!InShell || Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey) is not { } profile) return null;
        var game = _gameStarted is not null && Playing;
        var activity = PresenceActivity.Launcher;
        PresenceWorld? world = null;
        if (game)
        {
            var mine = _playWorlds?.LastMyWorld;
            if (mine is { Role: UiWorldRole.Hosting or UiWorldRole.Seated } inWorld && JoinLink.IsWorldId(inWorld.WorldId))
            {
                activity = PresenceActivity.Lobby;
                world = new PresenceWorld(inWorld.WorldId.ToLowerInvariant(), AudienceName(inWorld.Audience), inWorld.Players, inWorld.Max,
                    Math.Max(0, inWorld.Max - inWorld.Players));
            }
            else activity = _playWorlds?.GameReadiness.CurrentGeneration is not null ? PresenceActivity.FreeRoam : PresenceActivity.Menus;
        }
        // OD-118: an offline game has no hook pipe (no readiness, no world): it reads as free roam
        else if (_offlineChild is not null) activity = PresenceActivity.FreeRoam;
        var server = _serverPresence is { } known && known.Xuid == profile.Xuid ? known.Value : null;
        return new PresenceInputs
        {
            ProfileName = ProfileName.Plain(profile.Gamertag),
            Mode = profile.Mode,
            Tier = IsSupporter ? "vip" : "free",
            ClubTag = server?.Club?.Tag,
            Level = game ? _hookLevel ?? server?.Level : server?.Level,
            Credits = game ? _hookCredits ?? server?.Credits : server?.Credits,
            CarName = game ? _hookCarName : null, // T10: "<Year> <Make> <Model>" from the player's own install; empty until resolved
            Activity = activity,
            World = world,
            StartedAt = game ? _gameStarted!.Value : _launcherStarted,
            LauncherVersion = Version,
        };
    }

    private static string AudienceName(byte audience) => audience switch
    {
        0 => "everyone",
        1 => "supporters",
        2 => "friends",
        _ => "private",
    };

    /// <summary>Without the overlay nothing reads "my world" during play; the presence reads it (throttled by the bridge) every 30 s.</summary>
    private void PollPresenceWorld()
    {
        if (_presenceOverlay || _gameStarted is null || _playWorlds is not { } worlds) return;
        var now = DateTimeOffset.UtcNow;
        if (now - _presenceWorldPolled < PresenceWorldPollEvery) return;
        _presenceWorldPolled = now;
        _ = Task.Run(async () =>
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(20));
            try { await worlds.PollMyWorldAsync(bound.Token).ConfigureAwait(false); }
            catch (Exception error) when (error is OperationCanceledException or HttpRequestException or IOException or TimeoutException)
            { Log("discord: world read skipped (" + error.GetType().Name + ")"); }
        });
    }

    /// <summary>The presence's side of each supervisor event.</summary>
    private void PresenceOn(SupervisorEvent e)
    {
        switch (e)
        {
            case SupervisorEvent.Spawned:
                _gameStarted = DateTimeOffset.UtcNow;
                _hookLevel = null; _hookCredits = null; _hookCarName = null; _carNames.Clear();
                StartServerPresence();
                break;
            case SupervisorEvent.OverlayConnected:
                _presenceOverlay = true;
                break;
            case SupervisorEvent.GameProgress progress:
                _hookLevel = progress.Level;
                _hookCredits = progress.Credits;
                break;
            case SupervisorEvent.GameCar car:
                _hookCarName = CarName(car.CarKey); // T10: the key is Data_Car.Id × 1000; CarNames maps it
                break;
            case SupervisorEvent.Exited or SupervisorEvent.Ended:
                if (_gameStarted is null && e is SupervisorEvent.Ended && _presence is null) break;
                _gameStarted = null;
                _hookLevel = null; _hookCredits = null; _hookCarName = null; _carNames.Clear();
                _presenceOverlay = false;
                _serverPresencePoll?.Cancel();
                _serverPresencePoll = null;
                _presenceClearPending = _presence is not null; // DISCORD_PRESENCE.md §3: cleared when the game exits
                break;
            default:
                return;
        }
        RefreshPresence();
    }

    /// <summary>CR-S72 at game start and every 5 minutes in game; a server without the route gives nothing (no club).</summary>
    private void StartServerPresence()
    {
        _serverPresencePoll?.Cancel();
        if (_session is not { } session || Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey) is not { } profile || OfflineMode) return;
        var stop = _serverPresencePoll = new CancellationTokenSource();
        var xuid = profile.Xuid;
        _ = Task.Run(async () =>
        {
            // a relaunch or a new session never reads sooner than 5 minutes (or the last Retry-After) after the previous read
            var early = Volatile.Read(ref _serverPresenceNextTicks) - DateTimeOffset.UtcNow.UtcTicks;
            if (early > 0)
            {
                try { await Task.Delay(TimeSpan.FromTicks(early), stop.Token).ConfigureAwait(false); }
                catch (OperationCanceledException) { return; }
            }
            while (!stop.IsCancellationRequested)
            {
                var wait = ServerPresenceEvery;
                Volatile.Write(ref _serverPresenceNextTicks, (DateTimeOffset.UtcNow + wait).UtcTicks);
                try
                {
                    var presence = await session.Tokens.WithAccessAsync(access => session.Api.ProfilePresenceAsync(access, xuid, stop.Token), stop.Token).ConfigureAwait(false);
                    _ui.Post(_ =>
                    {
                        if (stop.IsCancellationRequested) return;
                        _serverPresence = presence is null ? null : (xuid, presence);
                        if (presence is null) Log("discord: server presence not offered (CR-S72)");
                        RefreshPresence();
                    }, null);
                    if (presence is null) return; // not supported: no retries this session
                }
                catch (OperationCanceledException) when (stop.IsCancellationRequested) { return; }
                catch (Exception error) when (error is ApiException or HttpRequestException or IOException or System.Text.Json.JsonException
                    or OperationCanceledException or InvalidOperationException)
                {
                    Log("discord: server presence " + (error is ApiException api ? api.Code : error.GetType().Name));
                    // 429 REQUEST.RATE_LIMITED: never before its Retry-After (and never faster than every 5 minutes)
                    if (error is ApiException { RetryAfter: { } retry } && retry > wait)
                    {
                        wait = retry;
                        Volatile.Write(ref _serverPresenceNextTicks, (DateTimeOffset.UtcNow + wait).UtcTicks);
                    }
                }
                try { await Task.Delay(wait, stop.Token).ConfigureAwait(false); }
                catch (OperationCanceledException) { return; }
            }
        });
    }

    /// <summary>The one-time note (DISCORD_PRESENCE.md §3) when Bullet4fun's Discord RPC is installed: it duplicates the built-in presence.</summary>
    private void NoteDuplicateDiscordMod()
    {
        if (!_modHealth.Installed.Any(mod => mod.Id == DuplicateDiscordModId)) return;
        var shown = Path.Combine(_stateDirectory, "discord-rpc-duplicate-noted");
        if (File.Exists(shown)) return;
        try { File.WriteAllText(shown, DateTimeOffset.UtcNow.ToString("O", CultureInfo.InvariantCulture)); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { Log("discord: duplicate note not kept (" + error.GetType().Name + ")"); }
        Message = S["presence.duplicate_mod"];
        Log("discord: Bullet4fun's discord-rpc is installed; noted once");
    }
}
