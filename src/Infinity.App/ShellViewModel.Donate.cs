// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// CR-L52: the sidebar's DONATE opens the Ko-fi page in the default browser. Signed in, the page is <c>links.kofi</c> of
/// <c>GET /core/v1/me/support</c>, read once per session (<see cref="RemoteCache"/>, cleared on sign-in and sign-out) under the busy
/// overlay. Owner 2026-11-16: when that page is unavailable (offline or no session, the read failed, not https, a self-hosted server
/// that sets none) <see cref="DonatePage.Fallback"/> opens instead, and the reason is logged.
/// </summary>
public sealed partial class ShellViewModel
{
    private static string SupportCacheKey => RemoteCache.Key("support");

    /// <summary>"DONATE": the page chosen as above, then the browser (a browser that does not open says so).</summary>
    public async Task OpenDonateAsync()
    {
        if (_busy) return;
        string? kofi = null;
        string? unavailable = null;
        if (SignedIn && !OfflineMode && _session is not null && _remote.TryGet<SupportInfo>(SupportCacheKey, out var known))
        {
            kofi = known.Links?.Kofi; // read earlier this session: no request, no overlay
        }
        else if (SignedIn && !OfflineMode && _session is { } session)
        {
            await Run(S["busy.donate"], async () =>
            {
                try
                {
                    kofi = (await _remote.GetAsync(SupportCacheKey, () => session.SupportAsync())).Links?.Kofi;
                }
                catch (ApiException e)
                {
                    unavailable = "support links not read (" + e.Code + ")";
                }
                catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException or JsonException)
                {
                    unavailable = "support links not read (" + e.GetType().Name + ")";
                }
            });
        }
        else
        {
            unavailable = OfflineMode ? "offline" : "no session";
        }
        var (page, reason) = unavailable is null ? DonatePage.Choose(kofi) : (new Uri(DonatePage.Fallback), unavailable);
        Log(reason is null ? "donate: opening the server's Ko-fi page" : "donate: opening " + DonatePage.Fallback + " (" + reason + ")");
        if (OpenDonateBrowser(page)) return;
        Message = S["donate.browser_failed"];
    }

    private bool OpenDonateBrowser(Uri page)
    {
        if (OpenBrowserForTests is { } open) { open(page); return true; }
        try
        {
            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(page.AbsoluteUri) { UseShellExecute = true })?.Dispose();
            return true;
        }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
            Log("donate: browser not opened (" + e.GetType().Name + ")");
            return false;
        }
    }
}
