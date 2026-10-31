// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.Globalization;
using System.Net;
using Infinity.Core.Overlay;
using Infinity.Core.Session;
using Infinity.Core.WorldServer;

namespace Infinity.Core.Matchmaking;

// AC-2 (MATCHMAKING_API §3.9, OD-147, OD-158; docs/launcher/ANTICHEAT.md): in a Hardcore world (the profile's mode Hardcore, the
// world not open to both modes) this player measures the other players' cars, never its own: as the host in its world server, as a
// seated joiner in its hook. The rules come from GET /mm/v1/watch once per world (never compiled in); every ended episode is posted
// once to POST …/observations with an id minted here (a retry sends the same id), at most 20 a minute (bucket mm.observe).
public sealed partial class WorldsBridge
{
    /// <summary>The server's bucket <c>mm.observe</c>: 20 a minute per play session.</summary>
    public const int ObservationsPerMinute = 20;
    internal const int MaxPendingObservations = 64;
    public static readonly TimeSpan WatchRulesRetry = TimeSpan.FromSeconds(30);

    /// <summary>Writes <see cref="Session.Frame.Watch"/> to the hook (the supervisor's control pipe).</summary>
    public Func<Session.Frame.Watch, CancellationToken, Task>? SendWatch { get; set; }

    private int _watchCapable;
    /// <summary>The hook reported <see cref="Session.CaptureWhat.Watch"/> ok in this session: a seated player's hook measures.</summary>
    public bool WatchCapable { get => Volatile.Read(ref _watchCapable) != 0; set => Volatile.Write(ref _watchCapable, value ? 1 : 0); }

    /// <summary>
    /// Test builds only (AC-5): one synthetic speed episode for the next eligible peer (<see cref="WatchTestSwitch"/>). A production
    /// launcher never sets it.
    /// </summary>
    public bool WatchForceSpeed { get; set; }

    private int _partsCapable;
    /// <summary>NR-6: the hook reported <see cref="Session.CaptureWhat.Parts"/> ok: it sends the other players' part rows.</summary>
    public bool PartsCapable { get => Volatile.Read(ref _partsCapable) != 0; set => Volatile.Write(ref _partsCapable, value ? 1 : 0); }

    /// <summary>NR-6: this PC's own game database (gamedbRC.slt, read-only) the parts rule judges against; null: no parts rule.</summary>
    public string? GameDatabase { get; set; }

    private PartsCatalog? _partsCatalog;
    private bool _partsCatalogRead;
    private readonly Lock _hookPartsGate = new();
    private PartsWatch? _hookParts; // a seated player's: fed by the hook's PlayerParts frames (supervisor loop), set by the seat loop

    // read once per bridge, on the first Hardcore world (seat loop only); a missing table or database is logged once
    private PartsCatalog? Parts()
    {
        if (_partsCatalogRead) return _partsCatalog;
        _partsCatalogRead = true;
        _partsCatalog = PartsCatalog.Load(GameDatabase, line => _log("worlds: watch: " + line));
        return _partsCatalog;
    }

    private sealed record PendingObservation(string WorldId, PeerObservation Observation, byte PeerIndex);

    // reports land here from the world server's actor and the supervisor's frame loop; the seat loop alone drains and posts
    private readonly ConcurrentQueue<PendingObservation> _observed = new();
    private readonly List<PendingObservation> _pendingObservations = [];
    private sealed record WatchKey(string WorldId, byte Role);
    private volatile WatchKey? _watchKey;
    private WatchRules? _watchRules;
    private DateTimeOffset _watchRulesRetryAt;
    private volatile bool _hookWatching;
    private bool _watchApplied, _watchHookMissingLogged;
    private string? _hookGraceWorld;
    private DateTimeOffset _hookGraceUntil;
    internal static readonly TimeSpan HookGrace = TimeSpan.FromSeconds(10);
    private IHostRelay? _watchRelay;
    private double _observeTokens = ObservationsPerMinute;
    private long _observeRefilledAt;
    private DateTimeOffset _observeRetryAt;

    /// <summary>The world this player measures in now (its id), or null.</summary>
    public string? WatchedWorldId => _watchKey?.WorldId;

    /// <summary>Observations waiting to be posted (tests and logs).</summary>
    public int PendingObservations => _pendingObservations.Count + _observed.Count;

    /// <summary>A Hardcore world this player hosts or is seated in (the last "My world" answer), with the role; null otherwise.</summary>
    private WatchKey? WatchTarget()
    {
        var mine = Volatile.Read(ref _mineSnapshot);
        if (!OnlineOpen || _mode != WorldModes.Hardcore || mine.AnyMode || string.IsNullOrEmpty(mine.WorldId)
            || mine.Role is not (UiWorldRole.Hosting or UiWorldRole.Seated)) return null;
        return new(mine.WorldId, mine.Role);
    }

    /// <summary>
    /// The seat loop's step: follows the world this player is in, hands the rules to the world server (host) or the hook (joiner),
    /// and posts what was observed. Failures are logged and retried; nothing here throws but cancellation.
    /// </summary>
    public async Task WatchTickAsync(CancellationToken cancellation)
    {
        var target = WatchTarget();
        if (target != _watchKey)
        {
            await StopWatchAsync(cancellation).ConfigureAwait(false);
            _watchKey = target;
            (_watchRules, _watchApplied, _watchRulesRetryAt, _watchHookMissingLogged) = (null, false, DateTimeOffset.MinValue, false);
            if (target is { } now) _log($"worlds: watch: Hardcore world {now.WorldId} as {(now.Role == UiWorldRole.Hosting ? "host" : "seated player")}; reading the rules");
        }
        if (_watchKey is { } key && !_watchApplied)
        {
            if (_watchRules is null && Now >= _watchRulesRetryAt) await ReadWatchRulesAsync(cancellation).ConfigureAwait(false);
            if (_watchRules is { } rules) await ApplyWatchAsync(key, rules, cancellation).ConfigureAwait(false);
        }
        await PostObservationsAsync(cancellation).ConfigureAwait(false);
    }

    private async Task ReadWatchRulesAsync(CancellationToken cancellation)
    {
        try
        {
            var answer = await _mm.WatchAsync(cancellation).ConfigureAwait(false);
            if (answer is { Ok: true, Value: { Valid: true } rules })
            {
                _watchRules = rules;
                _log(string.Create(CultureInfo.InvariantCulture,
                    $"worlds: watch: rules version {rules.Version}: speed over {rules.Speed.MaxKmh} km/h for {rules.Speed.SustainMs} ms, climb over {rules.Climb.MaxKmh} km/h for {rules.Climb.SustainMs} ms, jump {rules.Jump.MaxMeters} m (log only)"));
                return;
            }
            _log($"worlds: watch: no usable rules ({answer.Error?.Code ?? "invalid"}); retried in {WatchRulesRetry.TotalSeconds:0} s");
        }
        catch (Exception error) when (error is HttpRequestException or TimeoutException or IOException or InvalidOperationException or Api.ApiException
            || error is OperationCanceledException && !cancellation.IsCancellationRequested)
        { _log("worlds: watch: rules not read (" + error.GetType().Name + "); retried later"); }
        _watchRulesRetryAt = Now + WatchRulesRetry;
    }

    private async Task ApplyWatchAsync(WatchKey key, WatchRules rules, CancellationToken cancellation)
    {
        var force = WatchForceSpeed;
        if (key.Role == UiWorldRole.Hosting)
        {
            IHostRelay? relay;
            lock (_hostState) relay = _relay;
            if (relay is null || HostedWorldId != key.WorldId) return; // the world server is not this world's yet
            var self = ulong.TryParse(_ownXuid, NumberStyles.None, CultureInfo.InvariantCulture, out var own) ? own : 0;
            var world = key.WorldId;
            relay.Watch(PeerWatchRules.From(rules, force) with { Parts = Parts() }, self, report => Observed(world, report));
            _watchRelay = relay;
            _watchApplied = true;
            _log($"worlds: watch: the world server measures the other players' cars (rules version {rules.Version}{(force ? ", TEST forced speed episode" : string.Empty)})");
            return;
        }
        if (!WatchCapable || SendWatch is null)
        {
            if (!_watchHookMissingLogged) _log("worlds: watch: this hook does not measure the other players' cars (an older hook); nothing is sent");
            _watchHookMissingLogged = true;
            return;
        }
        try
        {
            var parts = PartsCapable ? Parts() : null;
            var seated = key.WorldId;
            if (parts is not null) lock (_hookPartsGate) _hookParts = new PartsWatch(parts, rules.Version, report => Observed(seated, report), _log);
            await SendWatch(Session.Frame.Watch.From(rules, force, sendParts: parts is not null), cancellation).ConfigureAwait(false);
            (_hookWatching, _watchApplied) = (true, true);
            _log($"worlds: watch: the hook measures the other players' cars (rules version {rules.Version}{(force ? ", TEST forced speed episode" : string.Empty)}{(parts is not null ? ", their parts too" : PartsCapable ? ", no parts rule (no readable game database)" : ", no parts rule (an older hook)")})");
        }
        catch (Exception error) when (error is IOException or ObjectDisposedException or ProtocolException
            || error is OperationCanceledException && !cancellation.IsCancellationRequested)
        { _log("worlds: watch: rules not sent to the hook (" + error.GetType().Name + "); retried"); }
    }

    private async Task StopWatchAsync(CancellationToken cancellation)
    {
        lock (_hookPartsGate) { _hookParts?.Flush(); _hookParts = null; }
        if (_watchRelay is { } relay)
        {
            _watchRelay = null;
            relay.Watch(null, 0, null);
        }
        if (_hookWatching)
        {
            _hookWatching = false;
            // the clear ends the hook's episodes in flight: what it reports for them still belongs to the world just left
            if (_watchKey is { } left) (_hookGraceWorld, _hookGraceUntil) = (left.WorldId, Now + HookGrace);
            try { if (SendWatch is { } send) await send(Session.Frame.Watch.Clear(), cancellation).ConfigureAwait(false); }
            catch (Exception error) when (error is IOException or ObjectDisposedException
                || error is OperationCanceledException && !cancellation.IsCancellationRequested)
            { _log("worlds: watch: clear not sent to the hook (" + error.GetType().Name + ")"); }
        }
        if (_watchKey is { } old) _log($"worlds: watch: off (left {old.WorldId} or it is no longer a Hardcore world)");
    }

    /// <summary>The hook's <see cref="Session.Frame.Observed"/>: an episode about another player in the world this player is seated in.</summary>
    public void HookObserved(Session.Frame.Observed frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        var world = _watchKey is { Role: UiWorldRole.Seated } key && _hookWatching ? key.WorldId
            : Now < _hookGraceUntil ? _hookGraceWorld : null;
        if (world is null)
        {
            _log($"worlds: watch: an observation about peer {frame.PeerIndex} with no Hardcore seat; dropped");
            return;
        }
        if (ObservedKind.Name(frame.Kind) is not { } kind) return;
        var counted = frame.Kind is ObservedKind.Speed or ObservedKind.Climb;
        Observed(world,new PeerWatchReport(frame.Xuid, kind, frame.Value / 1000.0, counted ? frame.SustainMs : null, frame.RulesVersion, frame.PeerIndex));
    }

    /// <summary>
    /// NR-6: the hook's <see cref="Session.Frame.PlayerParts"/>: another player's model and part rows in the Hardcore world this player
    /// is seated in, judged here against this PC's own game database. Dropped when this player watches no seat with the parts rule.
    /// </summary>
    public void HookPlayerParts(Session.Frame.PlayerParts frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        lock (_hookPartsGate)
        {
            if (_hookParts is not { } parts || !_hookWatching) return;
            parts.Observe(frame.PeerIndex, frame.Xuid, (frame.Flags & Session.Frame.PlayerParts.HasModel) != 0 ? frame.Model : null,
                (frame.Flags & Session.Frame.PlayerParts.HasParts) != 0 ? [.. frame.Parts] : null, (frame.Flags & Session.Frame.PlayerParts.Fresh) != 0);
        }
    }

    // any thread: an ended episode, queued for the seat loop (never the reporter itself)
    private void Observed(string worldId, PeerWatchReport report)
    {
        if (report.Xuid == 0 || string.Equals(report.Xuid.ToString(CultureInfo.InvariantCulture), _ownXuid, StringComparison.Ordinal)) return;
        _observed.Enqueue(new(worldId, new PeerObservation
        {
            Id = Guid.NewGuid().ToString(),
            Xuid = report.Xuid.ToString(CultureInfo.InvariantCulture),
            Kind = report.Kind,
            Value = report.Value,
            SustainMs = report.Kind is ObservationKinds.Speed or ObservationKinds.Climb ? report.SustainMs ?? 0 : null,
            RulesVersion = report.RulesVersion,
        }, report.PeerIndex));
    }

    private async Task PostObservationsAsync(CancellationToken cancellation)
    {
        while (_observed.TryDequeue(out var next))
        {
            if (_pendingObservations.Count >= MaxPendingObservations)
            {
                // a full queue drops a log-only observation first, else the oldest
                var drop = _pendingObservations.FindIndex(p => p.Observation.Kind is ObservationKinds.Jump or ObservationKinds.Clock);
                var dropped = _pendingObservations[drop < 0 ? 0 : drop];
                _pendingObservations.RemoveAt(drop < 0 ? 0 : drop);
                _log($"worlds: watch: queue full; a {dropped.Observation.Kind} observation about peer {dropped.PeerIndex} dropped");
            }
            _pendingObservations.Add(next);
        }
        if (_pendingObservations.Count == 0 || Now < _observeRetryAt || !OnlineOpen) return;
        var stamp = _clock.GetTimestamp();
        if (_observeRefilledAt != 0)
            _observeTokens = Math.Min(ObservationsPerMinute, _observeTokens + _clock.GetElapsedTime(_observeRefilledAt, stamp).TotalMinutes * ObservationsPerMinute);
        _observeRefilledAt = stamp;
        while (_pendingObservations.Count != 0 && _observeTokens >= 1)
        {
            var item = _pendingObservations[0];
            _observeTokens -= 1;
            MmResult<MmNone> answer;
            try { answer = await _mm.ObserveAsync(item.WorldId, item.Observation, cancellation).ConfigureAwait(false); }
            catch (Exception error) when (error is HttpRequestException or TimeoutException or IOException or InvalidOperationException or Api.ApiException
                || error is OperationCanceledException && !cancellation.IsCancellationRequested)
            {
                _log($"worlds: watch: a {item.Observation.Kind} observation about peer {item.PeerIndex} not posted ({error.GetType().Name}); retried with the same id");
                _observeRetryAt = Now + TimeSpan.FromSeconds(5);
                return;
            }
            if (answer.Ok)
            {
                _pendingObservations.RemoveAt(0);
                _log($"worlds: watch: a {item.Observation.Kind} observation about peer {item.PeerIndex} posted (rules version {item.Observation.RulesVersion})");
                continue;
            }
            if (answer.Status == HttpStatusCode.TooManyRequests || (int)answer.Status >= 500)
            {
                _observeRetryAt = Now + (answer.RetryAfter is { } after && after > TimeSpan.Zero ? after : TimeSpan.FromSeconds(10));
                _log($"worlds: watch: a {item.Observation.Kind} observation about peer {item.PeerIndex} waits ({answer.Error?.Code}); retried with the same id");
                return;
            }
            // refused for good (the world closed, the reporter left, a field out of range): never resent
            _pendingObservations.RemoveAt(0);
            _log($"worlds: watch: a {item.Observation.Kind} observation about peer {item.PeerIndex} refused ({answer.Error?.Code}{(answer.Error?.Field is { } field ? " " + field : string.Empty)}); dropped");
        }
    }
}

/// <summary>
/// AC-5's test-only switch: <c>FI_TEST_FORCE_OBSERVATION=speed</c> makes the next eligible peer produce one synthetic speed episode,
/// so a two-device check can see the whole path (hook or world server → launcher → server → quorum → pool). It is read only by a
/// build whose version has a SemVer prerelease (<c>build.ps1 -Version 2.0.1-ac5.1</c>); a released launcher (no prerelease: the
/// update verifier refuses any) ignores it. Never a user-facing setting (OD-144).
/// </summary>
public static class WatchTestSwitch
{
    public const string Variable = "FI_TEST_FORCE_OBSERVATION";

    public static bool ForceSpeed(string? value, string? informationalVersion) =>
        string.Equals(value, "speed", StringComparison.Ordinal)
        && Update.LauncherVersion.TryParse(informationalVersion, out var version) && version.Prerelease is not null;

    public static bool ForceSpeedFromEnvironment() => ForceSpeed(Environment.GetEnvironmentVariable(Variable),
        System.Reflection.CustomAttributeExtensions.GetCustomAttribute<System.Reflection.AssemblyInformationalVersionAttribute>(typeof(WatchTestSwitch).Assembly)?.InformationalVersion);
}
