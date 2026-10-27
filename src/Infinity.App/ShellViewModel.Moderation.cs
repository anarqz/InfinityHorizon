// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.Net.Http;
using System.IO;
using Infinity.Core.Api;
using Infinity.Core.Overlay;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>A player the moderation pane can act on: from the world roster (XUID only) or a lookup (with its account).</summary>
public sealed record ModerationTarget(string Xuid, string Gamertag, string? AccountId, string Line);

/// <summary>A choice of the cooldown or ban duration lists (<see cref="Value"/> minutes or hours; null: none / permanent).</summary>
public sealed record ModerationChoice(string Label, int? Value);

/// <summary>
/// CR-S62 (owner-decisions 2026-10-27 "Moderators kick a player; the launcher gets quick moderation actions"): the "Moderation" pane,
/// top right, for <c>moderator</c> and <c>admin</c> accounts only (<c>/launcher/v1/me</c> <c>role</c>). It lists the players of the
/// world this launcher's game is in (the overlay's roster, OD-63) and finds others by name or XUID, then offers Kick (a reason and a
/// 0-60 min cooldown), Strike (a reason) and, for admins, Ban (a reason, a duration or permanent, and the player's name typed to
/// confirm). One Idempotency-Key per action, kept for a retry after a lost answer and new once the server answered or the form
/// changed. The server checks the role on every call; one's own profiles are never offered.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _showModerationPane, _moderationBusy;
    private ModerationTarget? _modTarget;
    private ModerationAction _modAction = ModerationAction.Kick;
    private string _modReason = string.Empty, _modLookup = string.Empty, _modBanTyped = string.Empty;
    private int _modCooldown = 15;
    private int? _modBanHours = 24;
    private string? _modKey;
    private string? _modStatus;

    /// <summary>The players of the current world, never this account's own profiles.</summary>
    public ObservableCollection<ModerationTarget> ModerationPlayers { get; } = [];

    /// <summary>The last lookup's matches, never this account's own.</summary>
    public ObservableCollection<ModerationTarget> ModerationMatches { get; } = [];

    private string? ModerationRole
    {
        get
        {
            var role = _session?.Me?.Role;
            InspectModerationRole(ref role);
            return role;
        }
    }

    partial void InspectModerationRole(ref string? role);

    /// <summary>Only moderators and admins ever see the menu (players never do).</summary>
    public bool CanModerate => SignedIn && !OfflineMode && ModerationRoles.CanModerate(ModerationRole);

    public bool CanBan => CanModerate && ModerationRoles.CanBan(ModerationRole);

    /// <summary>One right sidebar at a time: a mod panel or the lobby pane opened later takes the place.</summary>
    public bool ShowModerationPane => _showModerationPane && CanModerate && _externalMod is null && !_showModInfo && !_showModOptions && !_showLobbyPane;

    public bool HasModerationPlayers => ModerationPlayers.Count > 0;

    public bool HasModerationMatches => ModerationMatches.Count > 0;

    public string ModerationLookup
    {
        get => _modLookup;
        set { _modLookup = value ?? string.Empty; Changed(); Changed(nameof(CanFindModerationTarget)); }
    }

    public bool CanFindModerationTarget => CanModerate && !_moderationBusy && _modLookup.Trim().Length > 0;

    public ModerationTarget? ModerationSelected
    {
        get => _modTarget;
        set
        {
            if (value == _modTarget) return;
            _modTarget = value; _modBanTyped = string.Empty;
            ModerationFormChanged();
            Changed(nameof(ModerationBanTyped));
            Changed(nameof(ModerationBanTypeHint));
        }
    }

    public string ModerationTargetText => _modTarget is { } t ? t.Gamertag + " · " + t.Xuid : S["moderation.target_none"];

    public bool ModerationKick { get => _modAction == ModerationAction.Kick; set { if (value) SetModerationAction(ModerationAction.Kick); } }

    public bool ModerationStrike { get => _modAction == ModerationAction.Strike; set { if (value) SetModerationAction(ModerationAction.Strike); } }

    public bool ModerationBan { get => _modAction == ModerationAction.Ban; set { if (value && CanBan) SetModerationAction(ModerationAction.Ban); } }

    public bool ShowModerationCooldown => _modAction == ModerationAction.Kick;

    public bool ShowModerationBanForm => _modAction == ModerationAction.Ban && CanBan;

    public string ModerationReason
    {
        get => _modReason;
        set { _modReason = value ?? string.Empty; Changed(); ModerationFormChanged(); }
    }

    public IReadOnlyList<ModerationChoice> ModerationCooldowns => ModerationRequests.CooldownChoices
        .Select(m => new ModerationChoice(m == 0 ? S["moderation.cooldown_none"] : S.Format("moderation.minutes", ("n", m.ToString(CultureInfo.CurrentCulture))), m))
        .ToList();

    public ModerationChoice? ModerationCooldown
    {
        get => ModerationCooldowns.FirstOrDefault(c => c.Value == _modCooldown);
        set { if (value?.Value is { } minutes && minutes != _modCooldown) { _modCooldown = minutes; Changed(); ModerationFormChanged(); } }
    }

    public IReadOnlyList<ModerationChoice> ModerationBanDurations => ModerationRequests.BanHours
        .Select(h => new ModerationChoice(S[h is { } hours ? "moderation.ban_" + hours.ToString(CultureInfo.InvariantCulture) + "h" : "moderation.ban_permanent"], h))
        .ToList();

    public ModerationChoice? ModerationBanDuration
    {
        get => ModerationBanDurations.FirstOrDefault(c => c.Value == _modBanHours);
        set { if (value is not null && value.Value != _modBanHours) { _modBanHours = value.Value; Changed(); ModerationFormChanged(); } }
    }

    public string ModerationBanTypeHint => _modTarget is { } t ? S.Format("moderation.ban_type", ("name", t.Gamertag)) : string.Empty;

    public string ModerationBanTyped
    {
        get => _modBanTyped;
        set { _modBanTyped = value ?? string.Empty; Changed(); Changed(nameof(CanApplyModeration)); }
    }

    /// <summary>What is wrong with the form (the reason), shown under it; empty when it can be sent.</summary>
    public string ModerationFormProblem => _modReason.Length == 0 || ModerationRequests.ReasonProblem(_modReason) is not { } key ? string.Empty : S[key];

    public string ModerationApplyText => _moderationBusy ? S["moderation.working"] : S[_modAction switch
    {
        ModerationAction.Strike => "moderation.apply_strike",
        ModerationAction.Ban => "moderation.apply_ban",
        _ => "moderation.apply_kick",
    }];

    public bool CanApplyModeration => CanModerate && !_moderationBusy && _modTarget is not null && ModerationRequests.ReasonProblem(_modReason) is null
        && (_modAction != ModerationAction.Ban || CanBan && ModerationRequests.BanConfirmed(_modBanTyped, _modTarget.Gamertag));

    /// <summary>The last action's result or refusal.</summary>
    public string? ModerationStatus => _modStatus;

    public bool HasModerationStatus => !string.IsNullOrEmpty(_modStatus);

    public void ToggleModerationPane()
    {
        if (!CanModerate) return;
        _showModerationPane = !ShowModerationPane;
        if (_showModerationPane)
        {
            _showLobbyPane = false; _showModInfo = false; _showModOptions = false; _externalMod = null;
            ModerationMatches.Clear(); // an earlier account's lookup never carries over
            if (_modTarget?.AccountId is not null) _modTarget = null;
            RefreshModerationPlayers();
        }
        Changed(string.Empty);
    }

    public void CloseModerationPane()
    {
        if (!_showModerationPane) return;
        _showModerationPane = false;
        Changed(string.Empty);
    }

    /// <summary>"Refresh": the world's roster read again (bounded), then listed.</summary>
    public async Task RefreshModerationPlayersAsync()
    {
        if (!CanModerate || _moderationBusy) return;
        if (_playWorlds is { } worlds)
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(10));
            try { await worlds.MyWorldAsync(bound.Token); }
            catch (Exception e) when (e is OperationCanceledException or HttpRequestException or IOException or ApiException or TimeoutException)
            {
                Log("moderation: roster not read (" + e.GetType().Name + ")");
            }
        }
        RefreshModerationPlayers();
    }

    /// <summary>The roster as last read; the selection stays when that player is still listed.</summary>
    private void RefreshModerationPlayers() => FillModerationPlayers(_playWorlds?.LastMembers ?? []);

    internal void FillModerationPlayers(IEnumerable<MemberRow> members)
    {
        ModerationPlayers.Clear();
        foreach (var m in members)
        {
            if (m.Friend == UiFriend.Self || IsOwnXuid(m.Xuid) || !ModerationRequests.IsXuid(m.Xuid)) continue;
            ModerationPlayers.Add(new ModerationTarget(m.Xuid, m.Gamertag, null, m.Gamertag));
        }
        Changed(nameof(HasModerationPlayers));
    }

    /// <summary>"Find": <c>GET /core/v1/admin/accounts?xuid=|gamertag=</c>.</summary>
    public async Task FindModerationTargetAsync()
    {
        if (!CanFindModerationTarget || _session is not { } session) return;
        await ModerationWork(async () =>
        {
            var found = await session.LookupAccountsAsync(_modLookup.Trim());
            ModerationMatches.Clear();
            var own = 0;
            foreach (var item in found.Items)
            {
                if (item.AccountId == session.Me?.AccountId || IsOwnXuid(item.Xuid)) { own++; continue; }
                ModerationMatches.Add(new ModerationTarget(item.Xuid, item.Gamertag, item.AccountId, item.Gamertag + " · " + item.Game));
            }
            Changed(nameof(HasModerationMatches));
            SetModerationStatus(ModerationMatches.Count > 0 ? null : S[own > 0 ? "moderation.self" : "moderation.lookup_none"]);
            if (ModerationMatches.Count == 1) ModerationSelected = ModerationMatches[0];
        });
    }

    /// <summary>The chosen action on the chosen player, under this form's Idempotency-Key.</summary>
    public async Task ApplyModerationAsync()
    {
        if (!CanApplyModeration || _session is not { } session || _modTarget is not { } target) return;
        if (IsOwnXuid(target.Xuid) || target.AccountId == session.Me?.AccountId) { SetModerationStatus(S["moderation.self"]); return; }
        var key = _modKey ??= Guid.CreateVersion7().ToString();
        var action = _modAction;
        await ModerationWork(async () =>
        {
            switch (action)
            {
                case ModerationAction.Kick:
                    var kick = await session.KickAsync(ModerationRequests.Kick(target.Xuid, _modReason, _modCooldown), key);
                    Log($"moderation: kick → {kick.SessionsEnded} session(s) ended, until {kick.Until ?? "none"}");
                    SetModerationStatus(KickResultText(target.Gamertag, kick));
                    break;
                case ModerationAction.Strike:
                    var strike = await session.StrikeAsync(ModerationRequests.Strike(target.Xuid, _modReason), key);
                    Log("moderation: strike " + strike.Id);
                    SetModerationStatus(S.Format("moderation.struck", ("name", target.Gamertag)));
                    break;
                case ModerationAction.Ban:
                    var accountId = target.AccountId ?? (await session.LookupAccountsAsync(target.Xuid)).Items.FirstOrDefault(i => i.Xuid == target.Xuid)?.AccountId;
                    if (accountId is null) { _modKey = null; SetModerationStatus(S["moderation.not_found"]); return; }
                    if (accountId == session.Me?.AccountId) { _modKey = null; SetModerationStatus(S["moderation.self"]); return; }
                    var ban = await session.BanAsync(ModerationRequests.Ban(accountId, _modBanHours, _modReason, session.Api.ServerNow(TimeProvider.System)), key);
                    Log("moderation: ban until " + (ban.Until ?? "permanent"));
                    SetModerationStatus(ban.Until is { } until ? S.Format("moderation.banned", ("name", target.Gamertag), ("time", LocalTime(until)))
                        : S.Format("moderation.banned_permanent", ("name", target.Gamertag)));
                    _modBanTyped = string.Empty; Changed(nameof(ModerationBanTyped));
                    break;
            }
            _modKey = null; // answered: the next press is a new action
        });
    }

    internal string KickResultText(string name, KickApplied kick)
    {
        ArgumentNullException.ThrowIfNull(kick);
        var n = kick.SessionsEnded.ToString(CultureInfo.CurrentCulture);
        return kick.Until is { } until
            ? S.Format("moderation.kicked_until", ("name", name), ("n", n), ("time", LocalTime(until)))
            : S.Format("moderation.kicked", ("name", name), ("n", n));
    }

    /// <summary>A role route's refusal in the moderator's words.</summary>
    internal string ModerationErrorText(ApiException error) => ModerationErrors.TextKey(error) is { } key
        ? S.Format(key, ("field", ModerationErrors.Field(error) ?? "?"))
        : ErrorText(error.Code);

    /// <summary>
    /// The kicked player's Play (403 <c>SESSIONS.KICKED</c>): the contract's text and, when the server said, the minutes left
    /// (<c>Retry-After</c>, else <c>details.until</c>).
    /// </summary>
    internal string KickedRefusalText(TimeSpan? wait) => KickCooldown.Minutes(wait) is { } minutes
        ? S.Format("kick.wait", ("minutes", minutes.ToString(CultureInfo.CurrentCulture)))
        : ErrorText(ErrorCodes.SessionsKicked);

    private async Task ModerationWork(Func<Task> work)
    {
        _moderationBusy = true;
        ChangedModeration();
        try
        {
            await work();
        }
        catch (ApiException e)
        {
            Log("moderation: refused " + e.Code);
            _modKey = null; // answered: a new press is a new request
            SetModerationStatus(ModerationErrorText(e));
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException)
        {
            // a lost answer: the key stays, so pressing again replays this same action instead of doing it twice
            Log("moderation: not answered (" + e.GetType().Name + ")");
            SetModerationStatus(S["native.unreachable"]);
        }
        finally
        {
            _moderationBusy = false;
            ChangedModeration();
        }
    }

    private bool IsOwnXuid(string xuid) => _session?.Me?.Profiles.Any(p => p.Xuid == xuid) == true;

    private static string LocalTime(string instant) =>
        DateTimeOffset.TryParse(instant, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var time)
            ? time.ToLocalTime().ToString("g", CultureInfo.CurrentCulture) : instant;

    private void SetModerationAction(ModerationAction action)
    {
        if (_modAction == action) return;
        _modAction = action;
        Changed(nameof(ModerationKick)); Changed(nameof(ModerationStrike)); Changed(nameof(ModerationBan));
        Changed(nameof(ShowModerationCooldown)); Changed(nameof(ShowModerationBanForm));
        ModerationFormChanged();
    }

    /// <summary>A changed body is a new action: a new key on the next press (the same key with another body is refused).</summary>
    private void ModerationFormChanged()
    {
        _modKey = null;
        ChangedModeration();
    }

    private void SetModerationStatus(string? text)
    {
        _modStatus = text;
        Changed(nameof(ModerationStatus));
        Changed(nameof(HasModerationStatus));
    }

    private void ChangedModeration()
    {
        Changed(nameof(ModerationSelected));
        Changed(nameof(ModerationTargetText));
        Changed(nameof(ModerationApplyText));
        Changed(nameof(CanApplyModeration));
        Changed(nameof(CanFindModerationTarget));
        Changed(nameof(ModerationFormProblem));
        Changed(nameof(ModerationBanTypeHint));
    }
}
