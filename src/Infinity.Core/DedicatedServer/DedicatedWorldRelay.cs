// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Runtime.Versioning;
using System.Text;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Matchmaking;
using Infinity.Core.Online;
using Infinity.Core.WorldServer;

namespace Infinity.Core.DedicatedServer;

/// <summary>
/// CR-L50 stage 2: one hosted world on one <c>worlds serve</c> child (DEDICATED_SERVER.md §2.2), as the bridge sees a
/// world server (<see cref="IWorldHostRelay"/>). A single pump reads the child's events and keeps the latest
/// <c>world.state</c> and <c>world.network</c>; every read the bridge, the overlay and the away-host make is answered from
/// them, with no round trip. Commands go over the control channel; the snapshots (<c>world.context</c>,
/// <c>world.admission</c>, voice, voice blocks, watch) are latest-wins in the client.
/// </summary>
[SupportedOSPlatform("windows")]
public sealed class DedicatedWorldRelay : IWorldHostRelay, IWireWatchRelay
{
    /// <summary>A car meet session call's budget: the engine's 15 s wait minus 1 s (§4.4).</summary>
    public static readonly TimeSpan GameSessionBudget = TimeSpan.FromSeconds(14);

    /// <summary>The bound of a close: <c>world.close</c>, then <c>shutdown</c>, each this long at most.</summary>
    public static readonly TimeSpan CloseBound = TimeSpan.FromSeconds(3);

    private readonly DedicatedServerProcess _child;
    private readonly DsHostingHealth _health;
    private readonly string _hostXuid;
    private readonly bool _isPlayer;
    private readonly TimeProvider _clock;
    private readonly Action<string> _log;
    private readonly CancellationTokenSource _stop = new();
    private readonly Task _pump;
    private DsWorldState? _state;
    private DsNetworkFacts? _network;
    private readonly DsCatalogs _catalogs;
    private int _catalogPending;
    private long _catalogDropped;
    private long _catalogFaults;
    private int _ended;
    private int _endNoted;
    private int _closing;
    private int _disposed;
    private Action<PeerWatchReport, string?>? _watchReport;
    private Action<string>? _diagnosticLog;
    private Diagnostics.CrashContext? _crashContext;
    private Func<string, IReadOnlyList<string>, CancellationToken, Task<(string Name, string Template)?>>? _gameSessions;

    private DedicatedWorldRelay(DedicatedServerProcess child, DsHostingHealth health, string worldId, string hostXuid, bool isPlayer,
        DsWorldOpenResult opened, ushort requestedPort, DsCatalogs catalogs, TimeProvider clock, Action<string> log)
    {
        _child = child;
        _catalogs = catalogs;
        _health = health;
        WorldId = worldId;
        _hostXuid = hostXuid;
        _isPlayer = isPlayer;
        _clock = clock;
        _log = log;
        _network = opened.Network;
        Lobbies = opened.Lobbies is { Count: > 0 } lobbies ? [.. lobbies] : [LobbyKinds.CarMeet];
        Port = opened.Network?.Port is int port and > 0 and <= ushort.MaxValue ? (ushort)port : requestedPort;
        _pump = Task.Run(PumpAsync);
    }

    /// <summary>The world this child serves (the matchmaking id, minted once per world).</summary>
    public string WorldId { get; }

    /// <summary>The child's process id (tests and diagnostics).</summary>
    public uint ProcessId => _child.Pid;

    /// <summary>Events of a wrong shape the channel dropped (diagnostics).</summary>
    public long MalformedEvents => _child.Client.MalformedEvents;

    public ushort Port { get; }

    public IReadOnlyList<string> Lobbies { get; }

    public bool Running => Volatile.Read(ref _disposed) == 0 && Volatile.Read(ref _ended) == 0;

    public bool EngineEnded => Volatile.Read(ref _ended) != 0 && Volatile.Read(ref _closing) == 0;

    /// <summary>The engine's last <c>world.state</c> (null before the first).</summary>
    public DsWorldState? State => Volatile.Read(ref _state);

    public Action<string>? DiagnosticLog { get => Volatile.Read(ref _diagnosticLog); set => Volatile.Write(ref _diagnosticLog, value); }

    public Diagnostics.CrashContext? CrashContext { get => Volatile.Read(ref _crashContext); set => Volatile.Write(ref _crashContext, value); }

    public Func<string, IReadOnlyList<string>, CancellationToken, Task<(string Name, string Template)?>>? GameSessions
    { get => Volatile.Read(ref _gameSessions); set => Volatile.Write(ref _gameSessions, value); }

    public RelayNetworkStatus NetworkStatus => ToStatus(Volatile.Read(ref _network));

    public int LobbyPlayers => Running && State is { } state ? Math.Max(0, state.LobbyPlayers) : 0;

    public string? Activity => Running && _isPlayer ? State?.Activity : null;

    public Task<IReadOnlyList<string>> PeersAsync(CancellationToken cancellation)
    {
        cancellation.ThrowIfCancellationRequested();
        IReadOnlyList<string> peers = Running && State is { } state ? [.. state.Peers.Where(p => p is not null).Distinct(StringComparer.Ordinal)] : [];
        return Task.FromResult(peers);
    }

    /// <summary>
    /// Opens <paramref name="open"/> on a started child. On refusal the child is shut down and the error returned
    /// (<c>bind_failed</c> carries its <c>reason</c>); a channel failure ends the child and throws.
    /// </summary>
    internal static async Task<(DedicatedWorldRelay? Relay, DsError? Error)> OpenAsync(DedicatedServerProcess child, DsHostingHealth health,
        DsWorldOpen open, DsCatalogs catalogs, TimeProvider clock, Action<string> log, CancellationToken cancellation)
    {
        DsResult<DsWorldOpenResult> opened;
        var step = DsOps.CatalogSet;
        try
        {
            // §9, CR-L51: the Road Trip rows before world.open (none: the world serves no Road Trip, never a failure)
            if (await Task.Run(catalogs.RoadTrip, cancellation).ConfigureAwait(false) is { } catalog)
            {
                var set = await child.Client.SetCatalogAsync(catalog, cancellation).ConfigureAwait(false);
                if (set is { Ok: false, Superseded: false }) log($"ds catalog: catalog.set refused ({set.Error?.Code}); no Road Trip in this world");
            }
            step = DsOps.WorldOpen;
            opened = await child.Client.OpenWorldAsync(open, cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException)
        {
            child.Kill(new DsExit(DsExitKind.Crashed, null, step + ": " + e.GetType().Name));
            health.Record(await EndAsync(child).ConfigureAwait(false));
            throw;
        }
        if (opened is { Ok: true, Value: { } value })
            return (new DedicatedWorldRelay(child, health, open.WorldId, open.Host.Xuid, open.Host.IsPlayer, value,
                (ushort)(open.Network?.Port ?? HostRelay.DefaultPort), catalogs, clock, log), null);
        var error = opened.Error ?? new DsError { Code = DsErrorCodes.Internal };
        health.Record(await ShutdownChildAsync(child).ConfigureAwait(false));
        return (null, error);
    }

    private static async Task<DsExit> ShutdownChildAsync(DedicatedServerProcess child)
    {
        try { return await child.ShutdownAsync(CloseBound).ConfigureAwait(false); }
        finally { await child.DisposeAsync().ConfigureAwait(false); }
    }

    private static async Task<DsExit> EndAsync(DedicatedServerProcess child)
    {
        try { return await child.Exited.WaitAsync(DedicatedServerProcess.DisposeBound).ConfigureAwait(false); }
        catch (TimeoutException) { return new DsExit(DsExitKind.Crashed, null, "no exit"); }
        finally { await child.DisposeAsync().ConfigureAwait(false); }
    }

    // ---- events ----

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1031:Do not catch general exception types",
        Justification = "The pump is the world's only reader: a fault in one handler must never stop the stream (the child would block).")]
    private async Task PumpAsync()
    {
        try
        {
            await foreach (var e in _child.Events.ReadAllAsync().ConfigureAwait(false))
            {
                try { On(e); }
                catch (Exception error) { _log("ds world: event handler failed (" + error.GetType().Name + ")"); }
            }
        }
        catch (Exception error) { _log("ds world: event pump ended (" + error.GetType().Name + ")"); }
        finally { Volatile.Write(ref _ended, 1); }
    }

    private bool Mine(string? worldId) => string.Equals(worldId, WorldId, StringComparison.Ordinal);

    private void On(DsEvent e)
    {
        switch (e)
        {
            case DsWorldState state when Mine(state.WorldId):
                Volatile.Write(ref _state, state);
                if (state.Network is { } facts) Volatile.Write(ref _network, facts);
                if (!state.Running) Ended("world.state running false");
                break;
            case DsWorldNetwork network when network.Facts.WorldId is null || Mine(network.Facts.WorldId):
                Volatile.Write(ref _network, network.Facts);
                break;
            case DsWorldClosed closed when Mine(closed.WorldId):
                Ended("world.closed " + closed.Reason + (closed.Fault is { } fault ? " (" + fault + ")" : string.Empty));
                break;
            case DsGameSessionRequest request when Mine(request.WorldId):
                _ = AnswerGameSessionAsync(request);
                break;
            case DsWatchReport report when Mine(report.WorldId):
                Report(report);
                break;
            case DsPeerRejected rejected when Mine(rejected.WorldId):
                if (ulong.TryParse(rejected.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var sender))
                    CrashContext?.Rejected(sender, rejected.Kind, rejected.Message, rejected.Router, rejected.Value);
                break;
            case DsDiag diag when diag.WorldId is null || Mine(diag.WorldId):
                if (DiagnosticLog is { } sink) Diagnostics.UdpDiagnostics.Enqueue(sink, DiagLine(diag.Body));
                break;
            case DsCatalogNeed need:
                AnswerCatalogNeed(need);
                break;
            case DsPeerLeft left when Mine(left.WorldId):
                // a hint (world.state stays authoritative); close: the game's CloseConnection, timeout: it went silent
                _log(string.Create(CultureInfo.InvariantCulture, $"ds world {Short}: peer {left.Peer} left {left.Engine ?? "?"} ({LeaveReason(left.Reason)})"));
                break;
            case DsNotice notice when notice.WorldId is null || Mine(notice.WorldId):
                _log($"ds world {Short}: notice {notice.Id} ({notice.Level})");
                break;
            case DsGone gone:
                _health.Record(gone.Exit);
                Ended(string.Create(CultureInfo.InvariantCulture, $"process ended ({gone.Exit.Kind}, code {gone.Exit.Code?.ToString(CultureInfo.InvariantCulture) ?? "-"})"));
                break;
        }
    }

    private string Short => WorldId[..Math.Min(8, WorldId.Length)];

    /// <summary><c>peer.left.reason</c> (§4.5) as logged: one of the contract's words, else <c>other</c>.</summary>
    internal static string LeaveReason(string? reason) =>
        reason is "close" or "timeout" or "kicked" or "not_allowed" or "moved" or "doomed" or "other" ? reason : "other";

    private void Ended(string why)
    {
        if (Interlocked.Exchange(ref _endNoted, 1) != 0) return;
        // logged first: whoever sees !Running also finds the reason in the log
        if (Volatile.Read(ref _closing) == 0) _log($"ds world {Short}: ended on its own: {why}");
        Volatile.Write(ref _ended, 1);
    }

    private async Task AnswerGameSessionAsync(DsGameSessionRequest request)
    {
        (string Name, string Template)? session = null;
        var sessions = GameSessions;
        if (!_isPlayer) _log("world: car meet session not asked: a dedicated host cannot create sessions yet");
        else if (sessions is null) _log("world: car meet session not asked: this world server has no platform call for it");
        else if (request.Kind != GameModeSessionKinds.FreeRoam) _log("world: car meet session not asked: invitation type not served");
        else if (request.Xuids.Count == 0) _log("world: car meet session not asked: nobody joined");
        else
        {
            using var bounded = CancellationTokenSource.CreateLinkedTokenSource(_stop.Token);
            bounded.CancelAfter(GameSessionBudget);
            try
            {
                session = await sessions(request.Kind, request.Xuids, bounded.Token).ConfigureAwait(false);
                if (session is null) _log("world: car meet session: none");
            }
            catch (OperationCanceledException) when (bounded.IsCancellationRequested)
            {
                _log(_stop.IsCancellationRequested ? "world: car meet session abandoned: the world server stopped" : "world: car meet session timed out");
            }
            catch (Exception e) when (e is HttpRequestException or IOException or InvalidOperationException or TimeoutException or ApiException
                or System.Text.Json.JsonException or OperationCanceledException)
            {
                _log("world: car meet session failed: " + e.GetType().Name);
            }
        }
        if (!Running) return;
        await QuietlyAsync("world.gameSession.answer", _child.Client.AnswerGameSessionAsync(new DsGameSessionAnswer
        {
            WorldId = WorldId,
            Rid = request.Rid,
            Session = session is { } made ? new DsGameSession { Name = made.Name, TemplateName = made.Template } : null,
        })).ConfigureAwait(false);
    }

    /// <summary>The most <c>catalog.need</c> answers in flight (§4.5: the engine holds at most 256 pending); past it the need is dropped and counted.</summary>
    internal const int MaxCatalogPending = 256;

    /// <summary><c>catalog.need</c> answers dropped because <see cref="MaxCatalogPending"/> were in flight (the engine asks again after 60 s).</summary>
    public long CatalogNeedsDropped => Interlocked.Read(ref _catalogDropped);

    /// <summary>§9: answers a model's parts need from this PC's game database, off the pump (a database read); counts only in the log.</summary>
    private void AnswerCatalogNeed(DsCatalogNeed need)
    {
        if (Interlocked.Increment(ref _catalogPending) > MaxCatalogPending)
        {
            Interlocked.Decrement(ref _catalogPending);
            Interlocked.Increment(ref _catalogDropped);
            return;
        }
        _ = Task.Run(AnswerAsync);

        [System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1031:Do not catch general exception types",
            Justification = "A fault answering one need is counted and logged, never left unobserved; the engine asks again after 60 s.")]
        async Task AnswerAsync()
        {
            try
            {
                var parts = _catalogs.Parts(need.Model);
                if (Running) await QuietlyAsync(DsOps.CatalogParts, _child.Client.SetCatalogPartsAsync(parts)).ConfigureAwait(false);
            }
            catch (Exception error)
            {
                var faults = Interlocked.Increment(ref _catalogFaults);
                _log(string.Create(CultureInfo.InvariantCulture, $"ds catalog: a parts answer failed ({error.GetType().Name}; {faults} so far)"));
            }
            finally { Interlocked.Decrement(ref _catalogPending); }
        }
    }

    /// <summary><c>catalog.parts</c> answers that failed (a database or channel fault), logged by count.</summary>
    public long CatalogFaults => Interlocked.Read(ref _catalogFaults);

    private void Report(DsWatchReport report)
    {
        if (Volatile.Read(ref _watchReport) is not { } sink
            || !ulong.TryParse(report.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) || xuid == 0) return;
        uint? sustain = report.SustainMs is long ms and >= 0 ? (uint)Math.Min(ms, uint.MaxValue) : null;
        sink(new PeerWatchReport(xuid, report.Kind, report.Value, sustain, report.RulesVersion, (byte)Math.Clamp(report.Peer, 0, 255),
            report.CarId, report.HeldKmh), Guid.TryParse(report.Id, out _) ? report.Id : null);
    }

    /// <summary>The <c>diag</c> fields the launcher logs (§4.5); anything else the engine adds is left out.</summary>
    private static readonly string[] DiagCounters = ["observations", "punchHints", "punchesSent", "sendErrors"];

    /// <summary>
    /// The <c>diag</c> event as one line: the allow-listed counters, the <c>nat</c>/<c>upnp</c> words and the <c>fiup</c>
    /// counters (integers under short letter-only names); never a string the engine wrote otherwise.
    /// </summary>
    private string DiagLine(JsonElement body)
    {
        var line = new StringBuilder("ds diag: world ").Append(Short);
        if (body.ValueKind != JsonValueKind.Object) return line.ToString();
        foreach (var field in body.EnumerateObject())
        {
            if (line.Length > 1024) break;
            if (field.Name is "nat" or "upnp" && field.Value.ValueKind == JsonValueKind.String
                && field.Value.GetString() is { Length: > 0 and <= 16 } word && word.All(c => char.IsAsciiLetterLower(c) || c == '_'))
                line.Append(' ').Append(field.Name).Append('=').Append(word);
            else if (DiagCounters.Contains(field.Name) && Integer(field.Value) is { } count)
                line.Append(' ').Append(field.Name).Append('=').Append(count.ToString(CultureInfo.InvariantCulture));
            else if (field.Name == "fiup" && field.Value.ValueKind == JsonValueKind.Object)
            {
                foreach (var counter in field.Value.EnumerateObject().Take(32))
                {
                    if (counter.Name is { Length: > 0 and <= 32 } name && name.All(char.IsAsciiLetter) && Integer(counter.Value) is { } value)
                        line.Append(" fiup.").Append(name).Append('=').Append(value.ToString(CultureInfo.InvariantCulture));
                }
            }
        }
        return line.ToString();

        static long? Integer(JsonElement value) => value.ValueKind == JsonValueKind.Number && value.TryGetInt64(out var n) ? n : null;
    }

    private static RelayNetworkStatus ToStatus(DsNetworkFacts? facts)
    {
        if (facts is null) return new(RelayNatKind.Unknown, [], RelayUpnpState.Off, null, false, false);
        static RelayNetworkAddress? Address(DsPublicAddress? a) => a is { Port: >= 0 and <= ushort.MaxValue } ? new(a.Address, (ushort)a.Port) : null;
        static uint Count(long n) => (uint)Math.Clamp(n, 0, uint.MaxValue);
        return new(facts.Nat switch { "cone" => RelayNatKind.Cone, "symmetric" => RelayNatKind.Symmetric, _ => RelayNatKind.Unknown },
            [.. facts.PublicAddresses.Select(Address).OfType<RelayNetworkAddress>().Take(8)],
            facts.Upnp switch
            {
                "pending" => RelayUpnpState.Pending,
                "not_found" => RelayUpnpState.NotFound,
                "mapped" => RelayUpnpState.Mapped,
                "failed" => RelayUpnpState.Failed,
                _ => RelayUpnpState.Off,
            },
            Address(facts.UpnpAddress), facts.BehindAnotherNat, facts.DualStack, Count(facts.PunchHints), Count(facts.PunchesSent), Count(facts.SendErrors));
    }

    // ---- commands ----

    private DateTimeOffset ServerNow => _clock.GetUtcNow();

    /// <summary>
    /// A failed command that leaves the world unusable: a hung engine (no answer in time) is ended like a crash, and either
    /// way the bridge closes the world (<see cref="InvalidOperationException"/> is a host failure to it).
    /// </summary>
    private InvalidOperationException Fatal(string op, Exception? cause, DsError? error)
    {
        if (cause is TimeoutException) _child.Kill(new DsExit(DsExitKind.Crashed, null, op + " timed out"));
        if (cause is TimeoutException or DedicatedServerGoneException) Ended(op + " " + cause.GetType().Name);
        _log($"ds world {Short}: {op} failed ({error?.Code ?? cause?.GetType().Name ?? "error"}); the world closes");
        return new InvalidOperationException("Dedicated world " + op + " failed", cause);
    }

    public async Task UpdateWorldAsync(WorldAnnounced world, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(world);
        ObjectDisposedException.ThrowIf(!Running, this);
        cancellation.ThrowIfCancellationRequested();
        if (!Mine(world.Id)) throw Fatal(DsOps.WorldContext, null, new DsError { Code = "world_id" });
        var (context, admission) = DsCommands.Announced(world, ServerNow);
        DsResult<DsContextResult> lease;
        try { lease = await _child.Client.ContextAsync(context, cancellation).ConfigureAwait(false); }
        catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException) { throw Fatal(DsOps.WorldContext, e, null); }
        if (!lease.Ok && !lease.Superseded) throw Fatal(DsOps.WorldContext, null, lease.Error);
        DsResult<DsAdmissionResult> admitted;
        try { admitted = await _child.Client.AdmissionAsync(admission, cancellation).ConfigureAwait(false); }
        catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException) { throw Fatal(DsOps.WorldAdmission, e, null); }
        if (!admitted.Ok && !admitted.Superseded) throw Fatal(DsOps.WorldAdmission, null, admitted.Error);
        // W-10: the world's first admission says its result. The host's own game needs no grant: the engine admits the host's
        // XUID from loopback only (DEDICATED_SERVER.md §6.2), with no FIUP hello and no development LAN rule.
        if (Interlocked.Exchange(ref _admissionLogged, 1) == 0)
            _log($"ds world {Short}: admission installed: context {(lease.Superseded ? "superseded" : "ok")}, "
                + (admitted.Superseded ? "admission superseded" : $"{admitted.Value?.Admitted ?? 0} grant(s) admitted")
                + "; the host's own game is admitted from loopback (no grant, no FIUP)");
    }

    private int _admissionLogged;

    public async Task RefreshAdmissionsAsync(IMatchmakingApi matchmaking, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(matchmaking);
        ObjectDisposedException.ThrowIf(!Running, this);
        var answer = await matchmaking.PunchesAsync(cancellation).ConfigureAwait(false);
        if (answer.Error is { } error && OnlinePause.From(error.Code, answer.Status, error.Details) is not null)
        {
            await EndAdmissionAsync("pause").ConfigureAwait(false);
            throw new ApiException(answer.Status, error.Code, "Online activity closed", error.Details, answer.RetryAfter);
        }
        cancellation.ThrowIfCancellationRequested();
        ObjectDisposedException.ThrowIf(!Running, this);
        if (answer is { Ok: true, Value: { } punches })
        {
            DsResult<DsAdmissionResult> admitted;
            try { admitted = await _child.Client.AdmissionAsync(DsCommands.Admission(WorldId, punches, ServerNow), cancellation).ConfigureAwait(false); }
            catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException) { throw Fatal(DsOps.WorldAdmission, e, null); }
            if (!admitted.Ok && !admitted.Superseded) throw Fatal(DsOps.WorldAdmission, null, admitted.Error);
            return;
        }
        if (answer.Error?.Kind is MmErrorKind.Unauthorized or MmErrorKind.WorldNotFound or MmErrorKind.NotHost or MmErrorKind.WrongPool)
        {
            await EndAdmissionAsync(answer.Error.Kind switch
            {
                MmErrorKind.Unauthorized => "unauthorized",
                MmErrorKind.WorldNotFound => "not_found",
                MmErrorKind.NotHost => "not_host",
                _ => "wrong_pool",
            }).ConfigureAwait(false);
            throw new InvalidOperationException("Dedicated world admission ended");
        }
        // the engine keeps the verified snapshot until it expires; the bridge's circuit keeps the retry metadata
        throw new ApiException(answer.Status, answer.Error?.Code ?? "HTTP.BAD_BODY", "Admission refresh unavailable",
            answer.Error?.Details, answer.RetryAfter ?? answer.Error?.RetryAfter);
    }

    private Task EndAdmissionAsync(string reason) =>
        QuietlyAsync(DsOps.WorldAdmissionEnd, _child.Client.EndAdmissionAsync(new DsWorldAdmissionEnd { WorldId = WorldId, Reason = reason }));

    public void SetVoice(string mode, IReadOnlyCollection<ulong> mutedForLobby)
    {
        ArgumentNullException.ThrowIfNull(mutedForLobby);
        if (!Running) return;
        if (mode is not (VoiceModes.Off or VoiceModes.Open or VoiceModes.Proximity))
        {
            _log($"ds world {Short}: voice mode not sent (not one of off, open, proximity)");
            return;
        }
        _ = QuietlyAsync(DsOps.WorldVoice, _child.Client.VoiceAsync(DsCommands.Voice(WorldId, mode, mutedForLobby)));
    }

    public void SetRosterVoiceBlocks(IEnumerable<WorldMember> members)
    {
        ArgumentNullException.ThrowIfNull(members);
        if (!Running) return;
        _ = QuietlyAsync(DsOps.WorldVoiceBlocks, _child.Client.VoiceBlocksAsync(DsCommands.VoiceBlocks(WorldId, members)));
    }

    /// <summary>The in-process form has no wire rules to send; the bridge uses <see cref="WatchWire"/> (only a stop is honoured here).</summary>
    public void Watch(PeerWatchRules? rules, ulong self, Action<PeerWatchReport>? report)
    {
        if (rules is null) WatchWire(null, false, false, self, null);
    }

    public void WatchWire(WatchRules? rules, bool forceSpeed, bool parts, ulong self, Action<PeerWatchReport, string?>? report)
    {
        Volatile.Write(ref _watchReport, rules is null ? null : report);
        if (!Running) return;
        _ = QuietlyAsync(DsOps.WorldWatch, _child.Client.WatchAsync(DsCommands.Watch(WorldId, rules, _isPlayer ? self : null, parts, forceSpeed)));
    }

    public void Kick(string xuid)
    {
        if (!Running || !ulong.TryParse(xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var id) || id == 0) return;
        _ = QuietlyAsync(DsOps.WorldKick, _child.Client.KickAsync(new DsWorldKick { WorldId = WorldId, Xuid = id.ToString(CultureInfo.InvariantCulture) }));
    }

    /// <summary>A command whose failure changes nothing for the bridge: logged by op and code, never thrown.</summary>
    private async Task QuietlyAsync<T>(string op, Task<DsResult<T>> sent)
    {
        try
        {
            var result = await sent.ConfigureAwait(false);
            if (result is { Ok: false, Superseded: false }) _log($"ds world {Short}: {op} refused ({result.Error?.Code})");
        }
        catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException or OperationCanceledException or ObjectDisposedException)
        {
            if (Running) _log($"ds world {Short}: {op} not applied ({e.GetType().Name})");
        }
    }

    // ---- close ----

    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        if (Volatile.Read(ref _ended) == 0) Volatile.Write(ref _closing, 1); // a close asked for is never "ended on its own"
        try { await _stop.CancelAsync().ConfigureAwait(false); } catch (AggregateException) { }
        if (Volatile.Read(ref _ended) == 0)
        {
            try
            {
                await _child.Client.CloseWorldAsync(new DsWorldClose { WorldId = WorldId, Reason = "controller" }).WaitAsync(CloseBound).ConfigureAwait(false);
            }
            catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException or OperationCanceledException) { }
        }
        var exit = await ShutdownChildAsync(_child).ConfigureAwait(false);
        try { await _pump.WaitAsync(DedicatedServerProcess.DisposeBound).ConfigureAwait(false); }
        catch (TimeoutException) { _log($"ds world {Short}: event pump did not end in time"); }
        _log(string.Create(CultureInfo.InvariantCulture, $"ds world {Short}: closed ({exit.Kind})"));
        _stop.Dispose();
    }

    public void Dispose()
    {
        try { Task.Run(() => DisposeAsync().AsTask()).Wait(DedicatedServerProcess.DisposeBound + CloseBound + CloseBound); }
        catch (AggregateException) { }
    }
}
