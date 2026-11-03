// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Runtime.Versioning;
using Infinity.Core.Matchmaking;

namespace Infinity.Core.DedicatedServer;

/// <summary>What a session's dedicated hosting needs to start a world's child (CR-L50 stage 2).</summary>
public sealed record DedicatedWorldHostingOptions
{
    /// <summary>The fixed path of <c>worlds.exe</c> (<see cref="DedicatedServerProcess.ResolveExecutable"/>).</summary>
    public required string Executable { get; init; }

    /// <summary>The launcher's version (<c>hello.controller.version</c>).</summary>
    public required string ControllerVersion { get; init; }

    /// <summary>The runtime descriptor's engine version (§10); null until the exe ships in the descriptor (stage 3).</summary>
    public string? ExpectedVersion { get; init; }

    /// <summary>The host's XUID (decimal).</summary>
    public required string HostXuid { get; init; }

    /// <summary>False for an OD-109 dedicated host (no game of its own, §6.3).</summary>
    public required bool IsPlayer { get; init; }

    /// <summary>The relay-grant public ring; empty: hosting unavailable (fail closed, as the in-process engine).</summary>
    public required RelayGrantVerifyKeys Keys { get; init; }

    /// <summary>Production and custom servers: the socket proof is required.</summary>
    public bool RequireProof { get; init; }

    /// <summary>Development stack only (never with <see cref="RequireProof"/>).</summary>
    public bool DevLanPeers { get; init; }

    /// <summary>The host's game database: the Road Trip rows (<c>catalog.set</c>) and the parts rows (<c>catalog.parts</c>) the launcher sends (CR-L51; never <c>--game-db</c>); null: none.</summary>
    public string? GameDatabase { get; init; }

    /// <summary>The world socket's options (<c>world.open.network</c>); null: the engine's defaults (port 5101, STUN, UPnP).</summary>
    public DsNetworkOptions? Network { get; init; }

    /// <summary>The trusted platform clock for <c>serverTimeMs</c> when the proof is required; null: the launcher's UTC.</summary>
    public TimeProvider? Clock { get; init; }

    /// <summary>
    /// §2.4: run <c>--probe-bind</c> before the first world of the session (and after the exe changed), so the first-bind
    /// firewall prompt comes at a calm moment. Only while no game is running (the OD-109 dedicated host); off otherwise.
    /// </summary>
    public bool ProbeBeforeFirstWorld { get; init; }

    /// <summary>§2.4: a firewall message key for the player (<see cref="DsFirewallStatus.MessageKey"/>); hosting goes on either way.</summary>
    public Action<string>? FirewallNotice { get; init; }

    public Action<string>? Log { get; init; }

    /// <summary>The signature the exe must carry before every spawn and before its firewall rule is offered (default: the release key).</summary>
    public DsSignaturePolicy Signature { get; init; } = DsSignaturePolicy.Official;

    /// <summary>Tests only: extra launch options for the fake (its <c>--fake</c> modes, a recorder).</summary>
    internal Func<DsLaunchOptions, DsLaunchOptions>? TestOnlyLaunch { get; init; }
}

/// <summary>
/// The session's dedicated hosting (CR-L50 stage 2): starts one child per world (<see cref="StartAsync"/>), keeps the
/// session's <see cref="DsHostingHealth"/>, and says why the last start failed (<see cref="FailureKey"/>). It never falls
/// back to the in-process engine: a missing exe, an unavailable engine or a failed start is "hosting unavailable".
/// </summary>
[SupportedOSPlatform("windows")]
public sealed class DedicatedWorldHosting(DedicatedWorldHostingOptions options)
{
    /// <summary>The key the bridge shows when dedicated hosting is unavailable (DEDICATED_SERVER.md §2.1; launcher-owned, EN and pt-BR).</summary>
    public const string UnavailableKey = "worlds.hosting_unavailable";

    /// <summary>A refused bind (§2.1: <c>bind_failed</c> in use or denied).</summary>
    public const string PortInUseKey = "lobby.port_in_use";

    private static readonly object ProbedGate = new();
    private static string? _probed; // the exe identity probed in this launcher process (path, size, write time)

    private readonly DedicatedWorldHostingOptions _options = options ?? throw new ArgumentNullException(nameof(options));
    private readonly Action<string> _log = options?.Log ?? (_ => { });
    private string? _failureKey;
    private DsCatalogs? _catalogs;

    // CR-L51: the rows the engine uses, read from this PC's game database (once per session for the parts; per world for Road Trip)
    private DsCatalogs Catalogs => LazyInitializer.EnsureInitialized(ref _catalogs, () => new DsCatalogs(_options.GameDatabase, _log));

    public DsHostingHealth Health { get; } = new();

    /// <summary>Why the last <see cref="StartAsync"/> returned null (a <c>worlds.*</c>/<c>lobby.*</c> key); null after a success.</summary>
    public string? FailureKey => Volatile.Read(ref _failureKey);

    /// <summary>The bridge's start (<see cref="WorldsBridge.StartRelayAsync"/>): the child, <c>hello</c>, <c>world.open</c>; null when hosting is unavailable.</summary>
    public async Task<IHostRelay?> StartAsync(HostRelayStart start, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(start);
        Volatile.Write(ref _failureKey, UnavailableKey);
        var world = start.WorldId[..Math.Min(8, start.WorldId.Length)];
        if (!Health.Available)
        {
            _log($"worlds: hosting unavailable for this session (dedicated engine: {Health.Reason?.Kind})");
            return null;
        }
        if (_options.Keys.Count == 0 || !ulong.TryParse(_options.HostXuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) || xuid == 0)
        {
            _log("worlds: dedicated engine not started: relay trust is not configured");
            return null;
        }
        if (!File.Exists(_options.Executable))
        {
            Health.Record(new DsExit(DsExitKind.ConfigInvalid, null, "worlds.exe is missing"));
            _log("worlds: hosting unavailable: " + DedicatedServerProcess.ExecutableName + " is missing next to the launcher");
            return null;
        }
        // before every spawn, the probe and the firewall offer: an exe whose signature does not verify never runs
        // (the spawn and the probe check it again, holding the file open until it has started)
        string? refusal = null;
        if (!DsSignature.Trusted(_options.Executable, _options.Signature, why => refusal = why))
        {
            Health.Record(new DsExit(DsExitKind.ConfigInvalid, null, "worlds.exe signature"));
            _log("worlds: hosting unavailable: " + DedicatedServerProcess.ExecutableName + " not run: " + refusal);
            return null;
        }
        var port = _options.Network?.Port ?? HostRelay.DefaultPort;
        if (await ProbeAsync(port, cancellation).ConfigureAwait(false) is { } refused)
        {
            Volatile.Write(ref _failureKey, refused);
            return null;
        }
        if (DsFirewall.Read(_options.Executable, port) is { MessageKey: { } firewall } status)
        {
            _log($"worlds: firewall: {status.State} for the world server; hosting goes on");
            _options.FirewallNotice?.Invoke(firewall);
        }
        var launch = new DsLaunchOptions
        {
            Executable = _options.Executable,
            ControllerVersion = _options.ControllerVersion,
            ExpectedVersion = _options.ExpectedVersion,
            GrantKeys = _options.Keys.Ring(),
            Log = _log,
            Signature = _options.Signature,
        };
        if (_options.TestOnlyLaunch is { } adjust) launch = adjust(launch);
        DedicatedServerProcess child;
        try
        {
            child = await DedicatedServerProcess.StartAsync(launch, cancellation).ConfigureAwait(false);
        }
        catch (DedicatedServerStartException e)
        {
            Health.Record(e.Exit);
            _log($"worlds: dedicated engine did not start for {world}: {e.Exit.Kind}{(Health.Available ? string.Empty : "; hosting unavailable for this session")}");
            return null;
        }
        var open = new DsWorldOpen
        {
            WorldId = start.WorldId,
            Host = new DsHost { Xuid = xuid.ToString(CultureInfo.InvariantCulture), IsPlayer = _options.IsPlayer },
            MaxPlayers = (int)Math.Clamp(start.MaxPlayers, 1u, 12u),
            RequireProof = _options.RequireProof,
            DevLanPeers = _options.DevLanPeers && !_options.RequireProof ? true : null,
            Network = _options.Network,
        };
        (DedicatedWorldRelay? Relay, DsError? Error) opened;
        try
        {
            // §9: catalog.set after hello and before world.open (a world lists roadtrip only with events in)
            opened = await DedicatedWorldRelay.OpenAsync(child, Health, open, Catalogs, _options.Clock ?? TimeProvider.System, _log, cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException or OperationCanceledException)
        {
            if (e is OperationCanceledException) await child.DisposeAsync().ConfigureAwait(false);
            _log($"worlds: dedicated engine ended while opening {world} ({e.GetType().Name})");
            if (e is OperationCanceledException) throw;
            return null;
        }
        if (opened.Relay is { } relay)
        {
            Volatile.Write(ref _failureKey, null);
            _log($"worlds: world {world} on the dedicated engine (pid {relay.ProcessId}), UDP {relay.Port}; awaiting world admission"
                + (open.DevLanPeers == true ? " (development LAN admission)" : string.Empty));
            return relay;
        }
        var error = opened.Error!;
        if (error.Code == DsErrorCodes.BindFailed && error.Reason is "in_use" or "denied") Volatile.Write(ref _failureKey, PortInUseKey);
        _log($"worlds: dedicated engine refused world {world}: {error}{(error.Reason is { } reason ? " " + reason : string.Empty)}");
        return null;
    }

    /// <summary>§2.4's probe: the first world of the process (or a changed exe), only when allowed; a refused bind is <see cref="PortInUseKey"/>.</summary>
    private async Task<string?> ProbeAsync(int port, CancellationToken cancellation)
    {
        if (!_options.ProbeBeforeFirstWorld) return null;
        string identity;
        try
        {
            var file = new FileInfo(_options.Executable);
            identity = string.Create(CultureInfo.InvariantCulture, $"{file.FullName}|{file.Length}|{file.LastWriteTimeUtc.Ticks}");
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
        lock (ProbedGate)
        {
            if (string.Equals(_probed, identity, StringComparison.OrdinalIgnoreCase)) return null;
        }
        var result = await DsProbeRunner.RunAsync(_options.Executable, port, _options.Signature, cancellation: cancellation).ConfigureAwait(false);
        _log($"worlds: world server bind probe on UDP {port}: {result}");
        if (result == DsProbeResult.Bound) lock (ProbedGate) _probed = identity;
        return DsProbe.Toast(result);
    }

    /// <summary>Tests only: forget the probe of this process.</summary>
    internal static void TestOnlyForgetProbe()
    {
        lock (ProbedGate) _probed = null;
    }
}
