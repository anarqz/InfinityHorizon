// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.Runtime.CompilerServices;
using Infinity.Core.Api;

namespace Infinity.Core.Shell;

/// <summary>
/// CR-L47 (CR-S83, OD-168 follow-up DB-Q4): one profile's "Show me on the website" switch. The change shows at once (optimistic), the
/// server's <see cref="Profile"/> is kept, and any failure puts the last server value back before the error reaches the caller.
/// </summary>
public sealed class WebsiteSwitch : INotifyPropertyChanged
{
    private Profile _profile;
    private bool _listed;
    private bool _busy;

    public WebsiteSwitch(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        _profile = profile;
        _listed = profile.WebsiteListed ?? true;
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    /// <summary>
    /// The account's live profiles whose server says <c>websiteListed</c>: none from a server before CR-S83, and never a
    /// <c>deletion_pending</c> one.
    /// </summary>
    public static IEnumerable<Profile> Eligible(IEnumerable<Profile> profiles) =>
        (profiles ?? throw new ArgumentNullException(nameof(profiles))).Where(p => p.WebsiteListed is not null && p.Status == "active");

    /// <summary>The profile as the server last answered it.</summary>
    public Profile Profile => _profile;

    public string Xuid => _profile.Xuid;

    /// <summary>The plain name (OD-120).</summary>
    public string Gamertag => ProfileName.Plain(_profile.Gamertag);

    public bool Listed
    {
        get => _listed;
        private set { _listed = value; Changed(); }
    }

    public bool Busy
    {
        get => _busy;
        private set { _busy = value; Changed(); Changed(nameof(Idle)); }
    }

    public bool Idle => !_busy;

    /// <summary>
    /// Sets the switch to <paramref name="wanted"/> through <paramref name="post"/>. While a change is in flight, or when nothing changes,
    /// nothing is sent (the view is told the current value again, so a clicked box snaps back).
    /// </summary>
    public async Task SetAsync(bool wanted, Func<bool, CancellationToken, Task<Profile>> post, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(post);
        if (_busy || wanted == _listed) { Changed(nameof(Listed)); return; }
        var before = _listed;
        Listed = wanted;
        Busy = true;
        try
        {
            var answer = await post(wanted, cancellation).ConfigureAwait(true);
            _profile = answer;
            Listed = answer.WebsiteListed ?? wanted;
            Changed(nameof(Profile));
            Changed(nameof(Gamertag));
        }
        catch
        {
            Listed = before;
            throw;
        }
        finally
        {
            Busy = false;
        }
    }

    private void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>The website route's refusals in the player's words (an i18n key), before the generic <c>error.&lt;CODE&gt;</c> text.</summary>
public static class WebsiteErrors
{
    /// <summary>The key for <paramref name="error"/>, or null to show the code's generic text.</summary>
    public static string? TextKey(ApiException error)
    {
        ArgumentNullException.ThrowIfNull(error);
        return error.Code switch
        {
            ErrorCodes.AccountsNotFound => "website.profile_gone",
            ErrorCodes.AccountsDeletionPending => "website.deletion_pending",
            ErrorCodes.RequestInvalid => "website.invalid",
            _ => null,
        };
    }

    /// <summary>The refusals after which the account's profiles are read again (the profile is gone or now pending deletion).</summary>
    public static bool RereadsProfiles(ApiException error) =>
        error?.Code is ErrorCodes.AccountsNotFound or ErrorCodes.AccountsDeletionPending;
}
