// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net.Http;
using Infinity.Core;
using Infinity.Core.Api;
using Infinity.Core.Auth;

namespace Infinity.App;

/// <summary>
/// CR-L40 (docs/launcher/LOBBIES_AND_SERVER_STATUS.md §1.2): a server's public figures from <c>GET /core/v1/status</c>, as
/// <c>Online 12/50 · Hardcore 5 (210) · Noob 7 (340) · Lobbies 3</c> (online now per mode, registered profiles in parentheses; the
/// four labels are the same words in English and pt-BR). The picker reads each row's server when it opens or refreshes (the
/// player's own request, as its round trips: OD-118), the sidebar the connected server every 30 s while the window is shown and
/// right after sign-in. A figure the server did not send is left out, never shown as zero; an unreachable server says so.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>How often the sidebar reads the connected server's figures while the window is shown.</summary>
    public static readonly TimeSpan ShellStatusEvery = TimeSpan.FromSeconds(30);

    // the picker: origin (scheme://host:port) → its figures; null when it could not be read
    private readonly Dictionary<string, ServerStatus?> _pickerStatus = new(StringComparer.OrdinalIgnoreCase);
    private readonly HashSet<string> _pickerStatusAsked = new(StringComparer.OrdinalIgnoreCase);
    private int _pickerStatusGeneration = -1;

    // the sidebar: the connected server's last answer (null unreadable) once one came
    private ServerStatus? _shellStatus;
    private bool _shellStatusAnswered;
    private bool _windowShown = true;
    private long? _shellStatusTimestamp;
    private CancellationTokenSource? _shellStatusLoop;

    /// <summary>"Online 12/50", or "Status unavailable": the sidebar's prominent line for the connected server.</summary>
    public string ShellStatusPrimary => !_shellStatusAnswered ? "" : _shellStatus is { } status ? Split(status).Primary : S["status.unavailable"];

    /// <summary>"Hardcore 5 (210) · Noob 7 (340) · Lobbies 3" (only the figures the server sent).</summary>
    public string ShellStatusSecondary => _shellStatusAnswered && _shellStatus is { } status ? Split(status).Secondary : "";

    public bool ShowShellStatus => SignedIn && _shellStatusAnswered;

    public bool ShowShellStatusSecondary => ShowShellStatus && ShellStatusSecondary.Length > 0;

    public string ServerStatusTip => S["status.tooltip"];

    private static (string Primary, string Secondary) Split(ServerStatus status)
    {
        var line = status.Line();
        var cut = line.IndexOf(" · ", StringComparison.Ordinal);
        return cut < 0 ? (line, "") : (line[..cut], line[(cut + 3)..]);
    }

    /// <summary>The harness gives a synthetic server here; the launcher's own clients otherwise.</summary>
    partial void PrepareServerStatusHttp(ref HttpClient http);

    private HttpClient StatusHttp(HttpClient http)
    {
        var chosen = http;
        PrepareServerStatusHttp(ref chosen);
        return chosen;
    }

    private static string? StatusKey(string? origin) =>
        origin is not null && Uri.TryCreate(origin, UriKind.Absolute, out var uri) && uri.Scheme is "http" or "https"
            ? uri.GetLeftPart(UriPartial.Authority) : null;

    /// <summary>The official row of a development build: the server this launcher runs on, when it is not a custom one.</summary>
    private string? OfficialStatusOrigin => _config.Custom ? null : _config.Server.GetLeftPart(UriPartial.Authority);

    /// <summary>The typed address once it is a server address (CustomServer rules).</summary>
    private string? CustomStatusOrigin
    {
        get
        {
            if (string.IsNullOrWhiteSpace(_serverAddress)) return null;
            try { return CustomServer.ParseOrigin(_serverAddress).GetLeftPart(UriPartial.Authority); }
            catch (ArgumentException) { return null; }
        }
    }

    /// <summary>A row's status line: empty until its server answered or failed.</summary>
    private string PickerStatusText(string? origin) =>
        StatusKey(origin) is not { } key || !_pickerStatus.TryGetValue(key, out var status) ? ""
        : status is null ? S["status.unavailable"] : status.Line();

    /// <summary>Reads every row's server not read yet in this picker generation (OpenPickers and Refresh start one each).</summary>
    private async Task ReadPickerStatusAsync(int generation)
    {
        if (generation != _pickerStatusGeneration)
        {
            _pickerStatusGeneration = generation;
            _pickerStatusAsked.Clear();
        }
        var origins = new List<string?> { BuildProfile.ProductionOnly ? CustomServer.OwnerPublicOrigin : OfficialStatusOrigin, CustomStatusOrigin };
        if (!BuildProfile.ProductionOnly) origins.AddRange((_beholder?.Servers ?? []).Select(s => s.Endpoint));
        var reads = new List<Task>();
        foreach (var key in origins.Select(StatusKey).OfType<string>())
            if (_pickerStatusAsked.Add(key)) reads.Add(ReadOnePickerStatusAsync(key, generation));
        await Task.WhenAll(reads);
    }

    private async Task ReadOnePickerStatusAsync(string origin, int generation)
    {
        var status = await ReadStatusAsync(StatusHttp(_pickerHttp.Value), new Uri(origin), CancellationToken.None);
        if (generation != _pickerStatusGeneration) return;
        _pickerStatus[origin] = status;
        FillServerItems();
    }

    /// <summary>One read, bounded at 10 s: null when the server is unreachable, refuses, does not list the game or sends nonsense.</summary>
    private async Task<ServerStatus?> ReadStatusAsync(HttpClient http, Uri core, CancellationToken cancellation)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _onlineStop.Token);
        bound.CancelAfter(TimeSpan.FromSeconds(10));
        try { return await ApiClient.ServerStatusAsync(http, core, ClientHeader, bound.Token); }
        // a stalled server (the 10 s bound), a non-JSON body or a bad origin is "unavailable" too, never the loop's end
        catch (Exception error) when ((UpdateFailure(error) || error is OperationCanceledException or System.Text.Json.JsonException or ArgumentException)
            && !cancellation.IsCancellationRequested)
        {
            Log("server status: " + core.Authority + " unavailable (" + error.GetType().Name + ")");
            return null;
        }
    }

    /// <summary>MainWindow: the window was shown, restored, hidden or minimized; the sidebar reads only while it is shown.</summary>
    public void SetWindowShown(bool shown)
    {
        if (_windowShown == shown) return;
        _windowShown = shown;
        RestartShellStatus();
    }

    /// <summary>Right after sign-in (and when the window is shown again): the connected server's figures now, then every 30 s.</summary>
    private void RestartShellStatus(bool now = false)
    {
        StopShellStatus();
        if (!_windowShown || _session?.Api.Core is not { } core) return;
        if (now) _shellStatusTimestamp = null;
        _shellStatusLoop = CancellationTokenSource.CreateLinkedTokenSource(_onlineStop.Token);
        _ = ShellStatusLoopAsync(core, _shellStatusLoop.Token);
    }

    private void StopShellStatus()
    {
        _shellStatusLoop?.Cancel();
        _shellStatusLoop?.Dispose();
        _shellStatusLoop = null;
    }

    /// <summary>Signed out: the figures go with the session.</summary>
    private void ClearShellStatus()
    {
        StopShellStatus();
        _shellStatus = null;
        _shellStatusAnswered = false;
        _shellStatusTimestamp = null;
    }

    private async Task ShellStatusLoopAsync(Uri core, CancellationToken cancellation)
    {
        try
        {
            // shown again inside the 30 s: the next read waits for the rest of them
            if (_shellStatusTimestamp is { } last && _onlineClock.GetElapsedTime(last) < ShellStatusEvery)
                await Task.Delay(ShellStatusEvery - _onlineClock.GetElapsedTime(last), _onlineClock, cancellation);
            while (!cancellation.IsCancellationRequested)
            {
                await RefreshShellStatusAsync(core, cancellation);
                await Task.Delay(ShellStatusEvery, _onlineClock, cancellation);
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { }
    }

    /// <summary>One read of the connected server's figures for the sidebar.</summary>
    internal async Task RefreshShellStatusAsync(Uri core, CancellationToken cancellation = default)
    {
        var status = await ReadStatusAsync(StatusHttp(Http), core, cancellation);
        if (cancellation.IsCancellationRequested) return;
        _shellStatusTimestamp = _onlineClock.GetTimestamp();
        _shellStatus = status;
        _shellStatusAnswered = true;
        Changed(nameof(ShellStatusPrimary));
        Changed(nameof(ShellStatusSecondary));
        Changed(nameof(ShowShellStatus));
        Changed(nameof(ShowShellStatusSecondary));
    }
}
