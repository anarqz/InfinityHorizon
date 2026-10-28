// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core;
using Infinity.Core.Announce;
using Infinity.Core.Auth;

namespace Infinity.App;

/// <summary>One row of the server picker: the official server, a server the beholder lists, or a typed address.</summary>
public sealed record ServerPickItem(string Kind, string Title, string Detail, string Badge, string? Endpoint, string Status = "")
{
    public const string OfficialKind = "official", ListedKind = "listed", CustomKind = "custom";
    public bool HasBadge => Badge.Length > 0;
    /// <summary>CR-L40: the server's <c>/core/v1/status</c> line under the detail (empty while it is not read).</summary>
    public bool HasStatus => Status.Length > 0;
}

/// <summary>One row of the relay picker: automatic (lowest latency) or a relay the beholder lists.</summary>
public sealed record RelayPickItem(string Title, string Detail, string Badge, string? Endpoint, bool Official)
{
    public bool HasBadge => Badge.Length > 0;
}

/// <summary>
/// OD122-L9 (owner: "na tela de signin, é preciso que exista um seletor de servidores e uma lista de relays... compatível com controle.
/// Também deve ser permitido digitar um endereço customizado"): the CR-L39 server editor lists the beholder's servers (name, host,
/// region, round trip) beside the official server and a typed address, and its relays (latency, official or community) beside
/// "Automatic". A listed server is saved exactly as a typed one (CustomServer rules, key pins, the official-account allow-list). The
/// relay choice is only saved (the fallback CR uses it). While the beholder is down the last saved lists show, marked as such.
/// </summary>
public sealed partial class ShellViewModel
{
    public ObservableCollection<ServerPickItem> ServerItems { get; } = [];
    public ObservableCollection<RelayPickItem> RelayItems { get; } = [];

    private ServerPickItem? _serverPick;
    private RelayPickItem? _relayPick;
    private bool _relayOfficialOnly;
    private BeholderLists? _beholder;
    private readonly Dictionary<string, Reachability> _reach = new(StringComparer.OrdinalIgnoreCase);
    private bool _beholderLoading;
    private int _pickerGeneration;

    /// <summary>
    /// The pickers' own client: OD-118 sends nothing to Infinity hosts before SIGN IN, and opening the picker (or Refresh) is the
    /// player's own request for these lists and states, so it is the only thing that uses it (no session, no token, no redirect).
    /// </summary>
    private readonly Lazy<HttpClient> _pickerHttp = new(() => new HttpClient(new HttpClientHandler { AllowAutoRedirect = false }) { Timeout = TimeSpan.FromSeconds(10) });

    /// <summary>The announcement server: the release default; a development build may name another (<c>FI_ANNOUNCER</c>).</summary>
    private static Uri Announcer => !BuildProfile.ProductionOnly && Environment.GetEnvironmentVariable("FI_ANNOUNCER") is { Length: > 0 } custom
        && Uri.TryCreate(custom, UriKind.Absolute, out var uri) ? uri : BeholderClient.DefaultAnnouncer;

    public ServerPickItem? ServerPick
    {
        get => _serverPick;
        set
        {
            if (value is null || ReferenceEquals(value, _serverPick)) return;
            _serverPick = value;
            _serverCustomSelected = value.Kind != ServerPickItem.OfficialKind;
            if (value.Kind == ServerPickItem.ListedKind) _serverAddress = value.Endpoint ?? "";
            _serverEditError = null;
            Changed(string.Empty);
        }
    }

    /// <summary>The address field is the "Custom address…" row's own.</summary>
    public bool ShowServerAddressField => _serverPick?.Kind == ServerPickItem.CustomKind;

    public RelayPickItem? RelayPick
    {
        get => _relayPick;
        set { if (value is null || ReferenceEquals(value, _relayPick)) return; _relayPick = value; Changed(nameof(RelayPick)); }
    }

    public bool RelayOfficialOnly
    {
        get => _relayOfficialOnly;
        set { if (_relayOfficialOnly == value) return; _relayOfficialOnly = value; FillRelayItems(); Changed(string.Empty); }
    }

    public bool BeholderLoading => _beholderLoading;

    public string BeholderStatusText => _beholderLoading ? S["picker.loading"]
        : _beholder is not { } lists ? ""
        : !lists.Cached ? S["picker.updated"]
        : lists.FetchedAt == DateTimeOffset.MinValue ? S["picker.unreachable"]
        : S.Format("picker.cached", ("time", lists.FetchedAt.ToLocalTime().ToString("HH:mm", CultureInfo.InvariantCulture)));

    public bool NoListedServers => _beholder is { Servers.Count: 0, Cached: false } && !_beholderLoading;

    /// <summary>Opens the picker on the saved lists at once, then reads the beholder and measures every node in the background.</summary>
    private void OpenPickers()
    {
        _relayOfficialOnly = RelayChoice.Read(StateRoot).OfficialOnly;
        _beholder ??= BeholderClient.Load(StateRoot);
        FillServerItems();
        FillRelayItems();
        _ = RefreshPickersAsync();
    }

    public async Task RefreshPickersAsync()
    {
        if (_beholderLoading) return;
        var generation = ++_pickerGeneration;
        _beholderLoading = true;
        Changed(string.Empty);
        _ = ReadPickerStatusAsync(generation); // the rows known without the beholder: play, official, the typed address
        try
        {
            var lists = await new BeholderClient(_pickerHttp.Value, Announcer).ListsAsync(StateRoot);
            if (generation != _pickerGeneration) return;
            _beholder = lists;
            Log($"picker: {lists.Servers.Count} server(s), {lists.Relays.Count} relay(s){(lists.Cached ? " (saved list)" : "")}");
            FillServerItems();
            FillRelayItems();
            _ = ReadPickerStatusAsync(generation); // CR-L40: each row's /core/v1/status, beside the round trips
            var probes = lists.Servers.Select(async s => (s.Endpoint, await NodeProbe.ServerAsync(_pickerHttp.Value, s.Endpoint, ClientHeader)))
                .Concat(lists.Relays.Select(async r => (r.Endpoint, await NodeProbe.RelayAsync(r.Endpoint))));
            foreach (var probe in probes.ToList())
            {
                var (endpoint, state) = await probe;
                if (generation != _pickerGeneration) return;
                _reach[endpoint] = state;
                FillServerItems();
                FillRelayItems();
            }
        }
        catch (Exception e) when (e is ArgumentException or InvalidOperationException or IOException)
        {
            Log("picker: the announcement server could not be used (" + e.GetType().Name + ")");
        }
        finally
        {
            if (generation == _pickerGeneration) { _beholderLoading = false; Changed(string.Empty); }
        }
    }

    private string ReachText(string endpoint) => !_reach.TryGetValue(endpoint, out var state) ? S["picker.measuring"]
        : state.RoundTrip is { } rtt ? S.Format("picker.ms", ("ms", Math.Max(1, (int)rtt.TotalMilliseconds).ToString(CultureInfo.InvariantCulture)))
        : S["picker.offline"];

    private void FillServerItems()
    {
        var chosen = _serverPick?.Kind == ServerPickItem.CustomKind ? null : _serverCustomSelected ? _serverAddress : null;
        var items = new List<ServerPickItem>();
        if (BuildProfile.ProductionOnly)
        {
            // Owner 2026-10-28: "Only the play.infinity-horizon.com stays": no official row and no other listed server in a release
            var play = CustomServer.OwnerPublicOrigin;
            var listed = (_beholder?.Servers ?? []).FirstOrDefault(s => SameOrigin(s.Endpoint, play));
            var host = new Uri(play).Authority;
            var detail = string.Join(" · ", new[] { host, listed?.Region ?? "", listed is null ? "" : ReachText(listed.Endpoint) }.Where(t => t.Length > 0));
            items.Add(new(ServerPickItem.ListedKind, listed?.Name ?? S["picker.play_server"], detail, S["picker.badge_official"], play, PickerStatusText(play)));
        }
        else items.Add(new(ServerPickItem.OfficialKind, S["server.official"], S["picker.official_detail"], "", null, PickerStatusText(OfficialStatusOrigin)));
        foreach (var server in BuildProfile.ProductionOnly ? [] : _beholder?.Servers ?? [])
        {
            string host;
            try { host = CustomServer.ParseOrigin(server.Endpoint).Authority; }
            catch (ArgumentException) { continue; } // never offered: it could not be saved (an official host, a path, a bad port)
            var detail = string.Join(" · ", new[] { host, server.Region ?? "", ReachText(server.Endpoint) }.Where(t => t.Length > 0));
            items.Add(new(ServerPickItem.ListedKind, server.Name, detail, S[OfficialNodes.IsOfficialServer(server) ? "picker.badge_official" : "picker.badge_community"], server.Endpoint,
                PickerStatusText(server.Endpoint)));
        }
        items.Add(new(ServerPickItem.CustomKind, S["picker.custom"], S["picker.custom_detail"], "", null, PickerStatusText(CustomStatusOrigin)));
        var previous = _serverPick;
        Replace(ServerItems, items);
        // keep the row the player is on; on open: the running choice (a listed one when its address matches, else the typed one)
        _serverPick = previous is null ? null : items.FirstOrDefault(i => i.Kind == previous.Kind && i.Endpoint == previous.Endpoint);
        _serverPick ??= !_serverCustomSelected ? items[0]
            : items.FirstOrDefault(i => i.Kind == ServerPickItem.ListedKind && SameOrigin(i.Endpoint, chosen ?? _serverAddress)) ?? items[^1];
        Changed(nameof(ServerPick));
        Changed(nameof(ShowServerAddressField));
    }

    private static bool SameOrigin(string? a, string? b)
    {
        try { return a is not null && b is not null && CustomServer.ParseOrigin(a) == CustomServer.ParseOrigin(b); }
        catch (ArgumentException) { return false; }
    }

    private void FillRelayItems()
    {
        var relays = (_beholder?.Relays ?? []).Where(r => NodeProbe.TryEndpoint(r.Endpoint, out _, out _) && (!_relayOfficialOnly || OfficialNodes.IsOfficialRelay(r)))
            .OrderBy(r => _reach.TryGetValue(r.Endpoint, out var s) && s.RoundTrip is { } t ? t : TimeSpan.MaxValue).ToList();
        var auto = RelayChoice.Automatic(relays.Select(r => (r, _reach.TryGetValue(r.Endpoint, out var s) ? s : new Reachability(null))));
        var items = new List<RelayPickItem> { new(S["picker.relay_auto"], auto is null ? S["picker.relay_auto_none"] : S.Format("picker.relay_auto_now", ("relay", auto.Name)), "", null, false) };
        foreach (var relay in relays)
        {
            var official = OfficialNodes.IsOfficialRelay(relay);
            var detail = string.Join(" · ", new[] { relay.Region ?? "", ReachText(relay.Endpoint) }.Where(t => t.Length > 0));
            items.Add(new(relay.Name, detail, S[official ? "picker.badge_official" : "picker.badge_community"], relay.Endpoint, official));
        }
        var saved = _relayPick?.Endpoint ?? RelayChoice.Read(StateRoot).Endpoint;
        Replace(RelayItems, items);
        _relayPick = items.FirstOrDefault(i => i.Endpoint == saved && saved is not null) ?? items[0];
        Changed(nameof(RelayPick));
    }

    private static void Replace<T>(ObservableCollection<T> target, List<T> items)
    {
        target.Clear();
        foreach (var item in items) target.Add(item);
    }

    /// <summary>
    /// CR-S57: the NAT test and relay round trips a world's open and a join report (the saved relay choice applies). Only the world
    /// bridges of a signed-in play or dedicated session call it (OD-118), when a world opens or before a join; results kept 5 minutes.
    /// </summary>
    private RelayReporter RelayReports => _relayReporter ??= new RelayReporter(
        async cancellation => (await new BeholderClient(_pickerHttp.Value, Announcer).ListsAsync(StateRoot, cancellation).ConfigureAwait(false)).Relays,
        () => RelayChoice.Read(StateRoot));
    private RelayReporter? _relayReporter;

    /// <summary>The relay part of Save: automatic or the picked relay, and "official relays only".</summary>
    private void SaveRelayChoice()
    {
        try { RelayChoice.Write(StateRoot, new(_relayPick?.Endpoint, _relayOfficialOnly)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("picker: the relay choice could not be saved (" + e.GetType().Name + ")"); }
    }
}
