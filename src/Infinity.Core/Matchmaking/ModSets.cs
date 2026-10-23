// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using Infinity.Core.Mods;

namespace Infinity.Core.Matchmaking;

/// <summary>Where a session's lobby mod set stands (OD-112, docs/launcher/MODDED_LOBBIES.md §2).</summary>
public enum ModSetPhase
{
    /// <summary>Mods are planned but not confirmed yet: no hosting, no announce, no join (never as vanilla).</summary>
    NotReady,

    /// <summary><see cref="ModSetState.Mods"/> is the set to send (empty: vanilla).</summary>
    Ready,

    /// <summary>The set cannot be known (a mod failed, the plan did not build): no hosting and no join.</summary>
    Failed,
}

/// <summary>A snapshot of the lobby mod set: <see cref="Mods"/> is sorted by id and only meaningful when <see cref="Ready"/>.</summary>
public sealed record ModSetState(ModSetPhase Phase, IReadOnlyList<ModEntry> Mods, string? Reason = null)
{
    public static ModSetState Vanilla { get; } = new(ModSetPhase.Ready, []);

    public static ModSetState NotReady { get; } = new(ModSetPhase.NotReady, []);

    public static ModSetState ReadyWith(IReadOnlyList<ModEntry> mods) => new(ModSetPhase.Ready, ModSets.Checked(mods));

    public static ModSetState FailedWith(string reason) => new(ModSetPhase.Failed, [], reason);

    public bool Ready => Phase == ModSetPhase.Ready;
}

/// <summary>
/// The player's multiplayer mod set for one session (OD-111/OD-112), the same for host and guest.
/// <see cref="Planned"/> is known before the game starts (the gate's <c>modded</c> flag, GATE_API.md §2);
/// <see cref="State"/> is what lobbies use. The interim source (<see cref="LaunchPlanModSetSource"/>) is
/// ready with the plan at once; N-H2's runtime source starts <see cref="ModSetPhase.NotReady"/> and turns
/// ready with the mods the native runtime reports as actually initialized (hook control frame, WINDOWS),
/// raising <see cref="Changed"/> on every transition. Mod-agnostic: no mod is named here.
/// </summary>
public interface IModSetSource
{
    /// <summary>The launch plan's set (sorted, ≤ 32): what this session was started with.</summary>
    IReadOnlyList<ModEntry> Planned { get; }

    ModSetState State { get; }

    /// <summary>Raised after <see cref="State"/> changed (any thread).</summary>
    event EventHandler<ModSetState>? Changed;
}

/// <summary>
/// One mod of a launch plan with what N-H2's runtime report is matched against: the manifest id and
/// version, whether the native runtime initializes it, its binary's SHA-256 (native mods) and the
/// SHA-256 of the settings file the fingerprint was computed over (null without settings).
/// </summary>
/// <param name="DataSql">FIMOD004: a data mod whose SQL the runtime applies; like a native mod it is in the set only once the runtime reports
/// it (zero binary hash, its planned settings hash or zero, its fingerprint).</param>
public sealed record PlannedMod(ModEntry Entry, string ModId, string ModVersion, bool Native, string? BinarySha256, string? SettingsSha256, bool DataSql = false)
{
    /// <summary>Entered into the set from the runtime's report (native and SQL data mods), not from the verified launch plan alone.</summary>
    public bool Reported => Native || DataSql;
}

/// <summary>A session without mods: always ready and vanilla.</summary>
public sealed class VanillaModSet : IModSetSource
{
    public static VanillaModSet Instance { get; } = new();

    private VanillaModSet() { }

    public IReadOnlyList<ModEntry> Planned => [];

    public ModSetState State => ModSetState.Vanilla;

    public event EventHandler<ModSetState>? Changed { add { } remove { } }
}

/// <summary>
/// The interim source (until N-H2): the launcher's enabled set for the current launch plan, ready at
/// once. Built once per session off the UI thread (<see cref="FromManagedModsAsync"/>); a plan that
/// cannot be built is <see cref="ModSetPhase.Failed"/>, never vanilla.
/// </summary>
public sealed class LaunchPlanModSetSource : IModSetSource
{
    public LaunchPlanModSetSource(IReadOnlyList<PlannedMod> plan)
    {
        ArgumentNullException.ThrowIfNull(plan);
        Plan = plan;
        Planned = ModSets.Checked(plan.Select(m => m.Entry).ToList());
        State = ModSetState.ReadyWith(Planned);
    }

    private LaunchPlanModSetSource(string reason)
    {
        Plan = [];
        Planned = [];
        State = ModSetState.FailedWith(reason);
    }

    /// <summary>The planned mods with their hashes (what a runtime report is checked against).</summary>
    public IReadOnlyList<PlannedMod> Plan { get; }

    public IReadOnlyList<ModEntry> Planned { get; }

    public ModSetState State { get; }

    public event EventHandler<ModSetState>? Changed { add { } remove { } }

    public static LaunchPlanModSetSource Failed(string reason) => new(reason);

    /// <summary>
    /// The enabled managed mods' set, computed on the thread pool (it opens the mod store and reads manifests
    /// and settings files). <paramref name="open"/> answering null (no game installation): vanilla.
    /// </summary>
    public static Task<LaunchPlanModSetSource> FromManagedModsAsync(Func<ManagedModFiles?> open, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(open);
        return Task.Run(() =>
        {
            try { return new LaunchPlanModSetSource(open() is { } mods ? ModSets.Plan(mods) : []); }
            catch (Exception error) when (error is ModException or ModSetException or IOException or UnauthorizedAccessException
                or JsonException or InvalidOperationException or KeyNotFoundException or FormatException)
            {
                // a ModSetException's message is a bounded reason (ids only, never a path); anything else is named by its type
                return Failed(error is ModSetException ? error.Message : error.GetType().Name);
            }
        }, cancellation);
    }
}

/// <summary>
/// N-H2's runtime report as this seam takes it (the hook's control frame 0x1b <c>ModsReport</c>, decoded and
/// validated by WINDOWS' codec in Session/Frame.cs, maps onto it field for field): the launch's nonce, the
/// plan's SHA-256, the phase (<see cref="Loading"/>, <see cref="Ready"/>, <see cref="Failed"/>) and each mod.
/// </summary>
public sealed record RuntimeModReport(byte[] Nonce, byte[] PlanSha256, byte Phase, IReadOnlyList<RuntimeModReportEntry> Mods)
{
    public const byte Loading = 0, Ready = 1, Failed = 2;

    /// <summary>The decoded hook frame (WINDOWS' <see cref="Session.Frame.ModsReport"/>), field for field.</summary>
    public static RuntimeModReport From(Session.Frame.ModsReport frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        return new(frame.Nonce, frame.PlanSha256, frame.Phase,
            frame.Mods.Select(m => new RuntimeModReportEntry(m.Id, m.Version, m.BinarySha256, m.SettingsSha256, m.CompatibilitySha256, m.State)).ToList());
    }
}

/// <summary>One reported mod: the plan's manifest id and version, the verified file hashes, the echoed fingerprint and its state (<see cref="Initialized"/>).</summary>
public sealed record RuntimeModReportEntry(string Id, string Version, byte[] BinarySha256, byte[] SettingsSha256, byte[] CompatibilitySha256, byte State)
{
    public const byte Pending = 0, Initialized = 1, Failed = 2;
}

/// <summary>
/// N-H2's source: the launch plan's set becomes the lobby set only when the native runtime reports it actually
/// initialized. Not ready while native mods are planned and no ready report arrived (no hosting, no announce,
/// never as vanilla); a report counts only with this launch's nonce and plan digest (<c>NativeModPlan.Nonce</c>,
/// <c>NativeModPlan.PlanSha256</c>), and a ready one only when every planned native mod is initialized with the
/// binary and settings hashes it was planned with and echoes its own fingerprint. Plan mods the runtime does not
/// initialize (file mods) are ready from the start. Resent reports (phase changes, a reconnected pipe) are idempotent.
/// </summary>
public sealed class RuntimeModSetSource : IModSetSource
{
    private readonly IReadOnlyList<PlannedMod> _plan;
    private readonly byte[] _nonce;
    private readonly byte[] _planSha256;
    private readonly object _lock = new();
    private ModSetState _state;

    public RuntimeModSetSource(IReadOnlyList<PlannedMod> plan, byte[] nonce, string planSha256)
    {
        ArgumentNullException.ThrowIfNull(plan);
        ArgumentNullException.ThrowIfNull(nonce);
        ArgumentNullException.ThrowIfNull(planSha256);
        if (nonce.Length != 16 || nonce.All(b => b == 0)) throw new ArgumentException("a 16-byte launch nonce", nameof(nonce));
        _plan = plan;
        _nonce = (byte[])nonce.Clone();
        _planSha256 = Convert.FromHexString(planSha256);
        Planned = ModSets.Checked(plan.Select(m => m.Entry).ToList());
        _state = plan.Any(m => m.Reported) ? ModSetState.NotReady : ModSetState.ReadyWith(Planned);
    }

    public IReadOnlyList<ModEntry> Planned { get; }

    public ModSetState State { get { lock (_lock) return _state; } }

    public event EventHandler<ModSetState>? Changed;

    /// <summary>
    /// OD-113: no ready report in time (the supervisor's deadline after the hook's handshake): a set still not ready fails for this
    /// launch, so a hook without the mod host or a silent runtime never leaves the session waiting. False when it was ready or failed already.
    /// </summary>
    public bool Expire(string reason)
    {
        ArgumentNullException.ThrowIfNull(reason);
        ModSetState next;
        lock (_lock)
        {
            if (_state.Phase != ModSetPhase.NotReady) return false;
            _state = next = ModSetState.FailedWith(reason);
        }
        Changed?.Invoke(this, next);
        return true;
    }

    /// <summary>One report; false when it is not this launch's (ignored). Raises <see cref="Changed"/> when the state moved.</summary>
    public bool Accept(RuntimeModReport report)
    {
        ArgumentNullException.ThrowIfNull(report);
        if (!report.Nonce.AsSpan().SequenceEqual(_nonce) || !report.PlanSha256.AsSpan().SequenceEqual(_planSha256)) return false;
        var next = report.Phase switch
        {
            RuntimeModReport.Ready => Verified(report),
            RuntimeModReport.Loading => ModSetState.NotReady,
            _ => ModSetState.FailedWith("runtime"),
        };
        ModSetState previous;
        lock (_lock)
        {
            previous = _state;
            // a failure is final for this launch: a later report never turns it back into a set
            if (previous.Phase == ModSetPhase.Failed) return true;
            _state = next;
        }
        if (previous.Phase != next.Phase || !ModSets.Same(previous.Mods, next.Mods)) Changed?.Invoke(this, next);
        return true;
    }

    private ModSetState Verified(RuntimeModReport report)
    {
        var ready = new List<ModEntry>();
        foreach (var planned in _plan)
        {
            if (!planned.Reported) { ready.Add(planned.Entry); continue; } // data mods without SQL: the verified launch plan
            var mod = report.Mods.FirstOrDefault(m => string.Equals(m.Id, planned.ModId, StringComparison.Ordinal));
            if (mod is null || mod.State != RuntimeModReportEntry.Initialized || !string.Equals(mod.Version, planned.ModVersion, StringComparison.Ordinal)
                || !Hex(mod.BinarySha256, planned.BinarySha256) || (planned.SettingsSha256 is not null && !Hex(mod.SettingsSha256, planned.SettingsSha256))
                || !Hex(mod.CompatibilitySha256, planned.Entry.Fingerprint))
                return ModSetState.FailedWith("unverified");
            ready.Add(planned.Entry);
        }
        return ModSetState.ReadyWith(ready);
    }

    private static bool Hex(byte[] bytes, string? hex) => hex is { Length: 64 } && string.Equals(Convert.ToHexStringLower(bytes), hex, StringComparison.OrdinalIgnoreCase);
}

/// <summary>A mod set that the contract would refuse (MATCHMAKING_API.md §2.7).</summary>
public sealed class ModSetException(string message) : Exception(message);

/// <summary>Building, checking and comparing mod sets (MODDED_LOBBIES.md §2; MATCHMAKING_API.md §2.7).</summary>
public static class ModSets
{
    /// <summary>
    /// The plan of the enabled managed mods: every enabled mod with multiplayer metadata whose impact is not
    /// <c>cosmetic</c> (<c>gameplay</c> and <c>unknown</c> count), its fingerprint over the effective settings;
    /// sorted by compatibility id. A mod without multiplayer metadata has no identity to compare and is left out.
    /// OD-113 (MODS.md §6): the fingerprint names the manifest's file hashes, so each such mod's mapped files must hold
    /// exactly those bytes now (<see cref="ManagedModFiles.ManifestMismatches"/>), data mods included; otherwise
    /// <see cref="ModSetException"/> (<c>files_changed:&lt;id&gt;</c>) and the set fails instead of claiming files the game will not load.
    /// </summary>
    public static IReadOnlyList<PlannedMod> Plan(ManagedModFiles mods)
    {
        ArgumentNullException.ThrowIfNull(mods);
        var enabled = mods.InstalledManifests().Where(m => mods.Receipt(m) is { Enabled: true }).ToList();
        foreach (var manifest in enabled)
            if (manifest.Multiplayer is { } multiplayer && Relevant(multiplayer) && mods.ManifestMismatches(manifest).Count > 0)
                throw new ModSetException("files_changed:" + manifest.Id);
        return Plan(enabled.Select(m => (m, m.ConfigPath is null ? null : mods.ReadSettings(m))));
    }

    /// <summary>The same over manifests and their settings files (null: the mod has no settings).</summary>
    public static IReadOnlyList<PlannedMod> Plan(IEnumerable<(ModManifest Manifest, ManifestIni? Settings)> enabled)
    {
        ArgumentNullException.ThrowIfNull(enabled);
        var plan = new List<PlannedMod>();
        foreach (var (manifest, settings) in enabled)
        {
            if (manifest.Multiplayer is not { } multiplayer || !Relevant(multiplayer)) continue;
            var values = settings?.Values ?? new Dictionary<string, JsonElement>(StringComparer.Ordinal);
            var entry = new ModEntry
            {
                Id = multiplayer.CompatibilityId,
                Version = multiplayer.Revision.ToString(CultureInfo.InvariantCulture),
                Fingerprint = multiplayer.Fingerprint(manifest, values),
            };
            string? binary = null;
            if (manifest.IsNative && manifest.Document.TryGetProperty("nativeHost", out var host)
                && host.TryGetProperty("binary", out var path) && path.GetString() is { } destination)
                binary = manifest.Files.FirstOrDefault(f => f.Role == "binary" && f.Destination == destination)?.Hash;
            if (!manifest.IsNative && manifest.HasDatabaseSql)
            {
                // FIMOD004 data entry: the runtime reports a zero binary hash and the planned settings hash (or zero).
                var zero = new string('0', 64);
                plan.Add(new PlannedMod(entry, manifest.Id, manifest.Version, false, zero, settings?.SourceHash ?? zero, DataSql: true));
                continue;
            }
            plan.Add(new PlannedMod(entry, manifest.Id, manifest.Version, manifest.IsNative, binary, settings?.SourceHash));
        }
        plan.Sort((a, b) => string.CompareOrdinal(a.Entry.Id, b.Entry.Id));
        Checked(plan.Select(p => p.Entry).ToList());
        return plan;
    }

    /// <summary>
    /// The gate's <c>modded</c> flag (GATE_API.md §2 "Modded sessions"): exactly when the launch's multiplayer set is
    /// not empty. The server then requires that set on every host PUT and refuses it from a non-modded session, so
    /// a cosmetic-only plan (an empty set) never sets it. A plan that failed to build is not modded, and its
    /// <see cref="ModSetPhase.Failed"/> state keeps the session out of every lobby instead.
    /// </summary>
    public static bool Modded(IModSetSource? source) => source is not null && source.Planned.Count > 0;

    /// <summary>
    /// Whether a mod's multiplayer impact puts it in the lobby set (a cosmetic mod never makes a lobby modded): the one
    /// classification Hardcore profiles share (<see cref="ModMultiplayer.EntersLobbySet"/>).
    /// </summary>
    public static bool Relevant(ModMultiplayer multiplayer)
    {
        ArgumentNullException.ThrowIfNull(multiplayer);
        return multiplayer.EntersLobbySet;
    }

    /// <summary>
    /// The session's source (N-H2): with a native plan, a <see cref="RuntimeModSetSource"/> over the launch plan bound to that
    /// plan's <see cref="NativeModPlan.Nonce"/> and <see cref="NativeModPlan.PlanSha256"/>; without one (or when the launch plan
    /// failed to build), the launch plan's own source.
    /// </summary>
    public static IModSetSource ForLaunch(LaunchPlanModSetSource launchPlan, NativeModPlan? native)
    {
        ArgumentNullException.ThrowIfNull(launchPlan);
        if (native is null || launchPlan.State.Phase == ModSetPhase.Failed) return launchPlan;
        return new RuntimeModSetSource(launchPlan.Plan, native.Nonce, native.PlanSha256);
    }

    /// <summary>
    /// Routes a hook ModsReport to the session's source: null when the source is not a runtime source (no native plan),
    /// otherwise <see cref="RuntimeModSetSource.Accept"/>'s answer (false: another launch's nonce or plan).
    /// </summary>
    public static bool? Report(IModSetSource? source, Session.Frame.ModsReport frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        return source is RuntimeModSetSource runtime ? runtime.Accept(RuntimeModReport.From(frame)) : null;
    }

    /// <summary>The set as the contract takes it (≤ 32, valid ids, versions and fingerprints, unique, sorted), or <see cref="ModSetException"/>.</summary>
    public static IReadOnlyList<ModEntry> Checked(IReadOnlyList<ModEntry> mods)
    {
        ArgumentNullException.ThrowIfNull(mods);
        if (mods.Count > MatchmakingLimits.ModsMax) throw new ModSetException("more than 32 multiplayer mods");
        for (var i = 0; i < mods.Count; i++)
        {
            var mod = mods[i];
            if (!Id(mod.Id)) throw new ModSetException("invalid mod id");
            if (mod.Version is not { Length: >= 1 and <= 16 } || !mod.Version.All(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '_' or '+' or '-'))
                throw new ModSetException("invalid mod version");
            if (mod.Fingerprint is not { Length: 64 } || !mod.Fingerprint.All(c => char.IsAsciiDigit(c) || c is >= 'a' and <= 'f'))
                throw new ModSetException("invalid mod fingerprint");
            if (i > 0 && string.CompareOrdinal(mods[i - 1].Id, mod.Id) >= 0) throw new ModSetException("mod ids repeat or are not sorted");
        }
        return mods;
    }

    private static bool Id(string? id) => id is { Length: >= 1 and <= 64 } && (char.IsAsciiDigit(id[0]) || char.IsAsciiLetterLower(id[0]))
        && id.All(c => char.IsAsciiDigit(c) || char.IsAsciiLetterLower(c) || c is '.' or '_' or '-');

    /// <summary>The join rule on the client: the same entries (id, version, fingerprint) in the same order (both sorted).</summary>
    public static bool Same(IReadOnlyList<ModEntry>? a, IReadOnlyList<ModEntry>? b)
    {
        a ??= []; b ??= [];
        if (a.Count != b.Count) return false;
        for (var i = 0; i < a.Count; i++)
            if (!string.Equals(a[i].Id, b[i].Id, StringComparison.Ordinal) || !string.Equals(a[i].Version, b[i].Version, StringComparison.Ordinal)
                || !string.Equals(a[i].Fingerprint, b[i].Fingerprint, StringComparison.Ordinal)) return false;
        return true;
    }

    /// <summary>The entries of <paramref name="from"/> that <paramref name="other"/> does not hold exactly (by id, version and fingerprint).</summary>
    public static IEnumerable<ModEntry> Missing(IReadOnlyList<ModEntry>? from, IReadOnlyList<ModEntry>? other) =>
        (from ?? []).Where(m => !(other ?? []).Any(o => o.Id == m.Id && o.Version == m.Version && o.Fingerprint == m.Fingerprint));
}
