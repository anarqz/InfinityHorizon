// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Text;
using Infinity.Core.Api;
using Infinity.Core.Online;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// T-L4 (OD-204, WORLDS_OFFICIAL §12.5; MATCHMAKING_API §2.10, §3.4; CORE_API §4.2): the invitee's inbox. While the game session is
/// online it polls <c>GET /launcher/v1/invites</c> every <see cref="PollEvery"/> (backing off on errors, up to <see cref="MaxBackoff"/>; a
/// 429 waits its <c>Retry-After</c>), and stops for good when signed out or refused (401, 403, an online pause, or signed out) or when its
/// cancellation (the session's online lifetime) ends.
/// <list type="bullet">
/// <item>Each new pending invite to a private lobby (<c>kind: invite</c>, <c>private: true</c>) becomes one modal prompt
/// (<see cref="UiFrame.InvitePrompt"/>, 0x23; never a toast) with the seconds the server leaves it (<c>expiresAt</c> −
/// <c>generatedAt</c>, so a skewed PC clock does not matter).</item>
/// <item>One the server no longer lists (answered elsewhere, accepted, withdrawn, expired) or past its time is withdrawn
/// (<see cref="UiFrame.InvitePrompt.Withdraw"/>).</item>
/// <item><see cref="ResendAsync"/> sends every pending, unanswered prompt again (an overlay's hello: a prompt sent before it was
/// dropped, T-L3's open item).</item>
/// <item><see cref="AnswerAsync"/>: Decline calls <c>POST /launcher/v1/invites/{id}/decline</c> (one retry of a transport failure; a
/// failure is one notice, the invite then lapses on its own); Accept is T-L5's (the join, the proof, FollowHost): it is only logged and
/// the prompt stays answered.</item>
/// </list>
/// Never logs a gamertag, a XUID or an invite id.
/// </summary>
public sealed class InviteInbox(Func<CancellationToken, Task<InviteList>> read, Func<string, CancellationToken, Task> decline,
    Func<string, string> text, TimeProvider? clock = null, Action<string>? log = null)
{
    /// <summary>The poll's cadence (CORE_API §4.2, WORLDS_OFFICIAL §12.5: every 10 s while online).</summary>
    public static readonly TimeSpan PollEvery = ApiClient.InvitesPollEvery;

    /// <summary>The longest wait between two polls after errors (each failure doubles the wait from <see cref="PollEvery"/>).</summary>
    public static readonly TimeSpan MaxBackoff = TimeSpan.FromMinutes(2);

    private readonly Func<CancellationToken, Task<InviteList>> _read = read ?? throw new ArgumentNullException(nameof(read));
    private readonly Func<string, CancellationToken, Task> _decline = decline ?? throw new ArgumentNullException(nameof(decline));
    private readonly Func<string, string> _text = text ?? throw new ArgumentNullException(nameof(text));
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly Action<string> _log = log ?? (_ => { });
    private readonly object _lock = new();
    private readonly Dictionary<string, Shown> _shown = new(StringComparer.Ordinal);

    // a prompt this inbox sent: its invite, its local deadline (the clock's), whether the player answered it; T-L5: the invite's world and
    // game session (the accept's join), and ReofferAt (the clock's): an accept the launcher could not serve now (the game in an event, another
    // lobby request in flight) is shown again from then while the server still lists it; 0 none
    private sealed record Shown(UiFrame.InvitePrompt Prompt, long Deadline, bool Answered, string WorldId = "", string? Activity = null,
        GameSessionReference? Session = null, long ReofferAt = 0);

    /// <summary>T-L5: how long an accept the launcher could not serve now waits before its prompt shows again (while still pending).</summary>
    public static readonly TimeSpan ReofferAfter = TimeSpan.FromSeconds(30);

    /// <summary>The ids of the prompts pending now, unanswered (tests).</summary>
    public IReadOnlyCollection<string> Pending
    {
        get { lock (_lock) return _shown.Where(s => !s.Value.Answered).Select(s => s.Key).ToList(); }
    }

    /// <summary>Whether polling stopped for good (signed out, refused, paused).</summary>
    public bool Stopped { get; private set; }

    /// <summary>
    /// Polls until <paramref name="cancellation"/> ends or the server refuses the caller; <paramref name="send"/> takes each prompt and
    /// withdrawal (the overlay channel, which drops what its overlay's protocol does not have).
    /// </summary>
    public async Task RunAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        var failures = 0;
        var wait = TimeSpan.Zero;
        try
        {
            while (!cancellation.IsCancellationRequested)
            {
                if (wait > TimeSpan.Zero) await Task.Delay(wait, _clock, cancellation).ConfigureAwait(false);
                try
                {
                    var list = await _read(cancellation).ConfigureAwait(false);
                    if (failures > 0) _log($"invites: the inbox answers again after {failures} failed polls");
                    failures = 0;
                    await ApplyAsync(list, send, cancellation).ConfigureAwait(false);
                    wait = PollEvery;
                }
                catch (ApiException error) when (error.Status is HttpStatusCode.Unauthorized or HttpStatusCode.Forbidden || OnlinePause.From(error) is not null)
                {
                    _log($"invites: the inbox stopped (HTTP {(int)error.Status} {error.Code})");
                    Stopped = true;
                    await WithdrawAllAsync(send, cancellation).ConfigureAwait(false);
                    return;
                }
                catch (InvalidOperationException) when (!cancellation.IsCancellationRequested)
                {
                    // review L5: the token source says signed out (no access token to read with): stop for good
                    _log("invites: the inbox stopped (signed out)");
                    Stopped = true;
                    await WithdrawAllAsync(send, cancellation).ConfigureAwait(false);
                    return;
                }
                catch (ApiException error) when (error.Status == HttpStatusCode.TooManyRequests)
                {
                    wait = Max(error.RetryAfter ?? TimeSpan.Zero, Backoff(++failures));
                    _log($"invites: the inbox is rate limited; next poll in {(int)wait.TotalSeconds} s");
                }
                catch (Exception error) when (!cancellation.IsCancellationRequested && error is ApiException or HttpRequestException
                    or OperationCanceledException or IOException or System.Text.Json.JsonException or TimeoutException)
                {
                    wait = Backoff(++failures);
                    _log($"invites: the inbox poll failed ({(error is ApiException api ? $"HTTP {(int)api.Status} {api.Code}" : error.GetType().Name)}); next poll in {(int)wait.TotalSeconds} s");
                }
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
        }
    }

    /// <summary>The wait after <paramref name="failures"/> failed polls in a row: 20 s, 40 s, 80 s, then <see cref="MaxBackoff"/>.</summary>
    public static TimeSpan Backoff(int failures) =>
        failures <= 0 ? PollEvery : Min(TimeSpan.FromTicks(PollEvery.Ticks << Math.Min(failures, 8)), MaxBackoff);

    /// <summary>One answer of the inbox: new private invites are prompted, gone or lapsed ones withdrawn.</summary>
    public async Task ApplyAsync(InviteList list, Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(list);
        ArgumentNullException.ThrowIfNull(send);
        var now = _clock.GetTimestamp();
        var serverNow = Parse(list.GeneratedAt);
        var listed = new HashSet<string>(StringComparer.Ordinal);
        var outgoing = new List<UiFrame>();
        lock (_lock)
        {
            foreach (var invite in list.Items)
            {
                if (Prompt(invite, serverNow) is not { } prompt) continue;
                listed.Add(invite.Id);
                if (_shown.TryGetValue(invite.Id, out var known))
                {
                    // T-L5: an accept that could not be served then is shown again once its wait is over (with the time it has left)
                    if (!known.Answered && known.ReofferAt > 0 && known.ReofferAt <= now && known.Deadline > now)
                    {
                        _shown[invite.Id] = known with { ReofferAt = 0 };
                        outgoing.Add(Left(known, now));
                    }
                    continue;
                }
                _shown[invite.Id] = new Shown(prompt, now + (long)(prompt.ExpiresInSec * (double)_clock.TimestampFrequency), false,
                    invite.World.Id, invite.World.Activity, invite.Session);
                outgoing.Add(prompt);
            }
            foreach (var (id, shown) in _shown.ToList())
            {
                if (listed.Contains(id) && shown.Deadline > now) continue;
                _shown.Remove(id);
                if (!shown.Answered) outgoing.Add(UiFrame.InvitePrompt.Withdraw(id));
            }
        }
        var added = outgoing.Count(f => f is UiFrame.InvitePrompt { ExpiresInSec: > 0 });
        if (added > 0) _log($"invites: {added} new private lobby invite(s) prompted");
        if (outgoing.Count > added) _log($"invites: {outgoing.Count - added} prompt(s) withdrawn (answered elsewhere, gone or expired)");
        foreach (var frame in outgoing) await send(frame, cancellation).ConfigureAwait(false);
    }

    /// <summary>T-L3's open item: every pending, unanswered prompt again, with the seconds it has left (an overlay's hello).</summary>
    public async Task ResendAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        var now = _clock.GetTimestamp();
        List<UiFrame.InvitePrompt> again;
        lock (_lock)
        {
            again = _shown.Values.Where(s => !s.Answered && s.Deadline > now && s.ReofferAt <= now).Select(s => Left(s, now)).ToList();
        }
        if (again.Count > 0) _log($"invites: {again.Count} pending prompt(s) sent again (the overlay said hello)");
        foreach (var prompt in again) await send(prompt, cancellation).ConfigureAwait(false);
    }

    // a shown prompt with the seconds its local deadline leaves (at least 1)
    private UiFrame.InvitePrompt Left(Shown shown, long now) => shown.Prompt with
    {
        ExpiresInSec = (ushort)Math.Clamp(Math.Ceiling((shown.Deadline - now) / (double)_clock.TimestampFrequency), 1, UiFrame.MaxInviteSeconds),
    };

    /// <summary>
    /// The player's answer to a prompt (0x24): Decline calls the decline route; Accept (T-L5) hands the invite's world to
    /// <paramref name="join"/> (the bridge's <see cref="WorldsBridge.AcceptInviteAsync"/>: the join, the proof, FollowHost). An accept it
    /// could not serve now (<see cref="InviteAcceptOutcome.NotNow"/>) shows its prompt again after <see cref="ReofferAfter"/> while the
    /// server still lists it; every other outcome leaves it answered. A second answer to the same prompt is ignored. Returns one notice
    /// to show, or null (nothing to say).
    /// </summary>
    public async Task<UiFrame.Toast?> AnswerAsync(string inviteId, bool accept, CancellationToken cancellation,
        Func<InviteAccept, CancellationToken, Task<InviteAcceptAnswer>>? join = null)
    {
        ArgumentException.ThrowIfNullOrEmpty(inviteId);
        Shown answered;
        long now;
        lock (_lock)
        {
            if (!_shown.TryGetValue(inviteId, out var shown) || shown.Answered)
            {
                _log("invites: an answer for no pending prompt; ignored");
                return null;
            }
            now = _clock.GetTimestamp();
            _shown[inviteId] = answered = shown with { Answered = true, ReofferAt = 0 };
        }
        if (accept)
        {
            if (join is null)
            {
                _log("invites: accepted with no lobby bridge in this session; nothing is sent");
                return null;
            }
            var left = TimeSpan.FromSeconds((answered.Deadline - now) / (double)_clock.TimestampFrequency);
            InviteAcceptAnswer result;
            try
            {
                result = await join(new InviteAccept(inviteId, answered.WorldId, answered.Activity, left, answered.Session), cancellation).ConfigureAwait(false);
            }
            catch (Exception error) when (!cancellation.IsCancellationRequested && error is ApiException or HttpRequestException
                or OperationCanceledException or IOException or InvalidOperationException or TimeoutException)
            {
                _log($"invites: accept failed ({error.GetType().Name})");
                return PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.InviteJoinFailed);
            }
            if (result.Outcome == InviteAcceptOutcome.NotNow)
            {
                lock (_lock)
                {
                    if (_shown.TryGetValue(inviteId, out var still) && still.Answered)
                        _shown[inviteId] = still with { Answered = false, ReofferAt = _clock.GetTimestamp() + (long)(ReofferAfter.TotalSeconds * _clock.TimestampFrequency) };
                }
                _log($"invites: accepted, but not now; the prompt shows again in {(int)ReofferAfter.TotalSeconds} s while the invite is pending");
            }
            else _log($"invites: accepted ({result.Outcome.ToString().ToLowerInvariant()})");
            return result.Notice;
        }
        try
        {
            try { await _decline(inviteId, cancellation).ConfigureAwait(false); }
            catch (HttpRequestException) when (!cancellation.IsCancellationRequested) { await _decline(inviteId, cancellation).ConfigureAwait(false); }
            _log("invites: declined");
            return null;
        }
        catch (Exception error) when (!cancellation.IsCancellationRequested && error is ApiException or HttpRequestException
            or OperationCanceledException or IOException or InvalidOperationException or TimeoutException)
        {
            _log($"invites: decline failed ({(error is ApiException api ? $"HTTP {(int)api.Status} {api.Code}" : error.GetType().Name)}); it lapses on its own");
            return PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.DeclineFailed);
        }
    }

    private async Task WithdrawAllAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        List<string> ids;
        lock (_lock)
        {
            ids = _shown.Where(s => !s.Value.Answered).Select(s => s.Key).ToList();
            _shown.Clear();
        }
        foreach (var id in ids) await send(UiFrame.InvitePrompt.Withdraw(id), cancellation).ConfigureAwait(false);
    }

    /// <summary>
    /// The prompt of <paramref name="invite"/>, or null when it is none of this inbox's: not an invite (a request to join), not to a
    /// private lobby, already past its time, or with an id or inviter the pipe cannot carry.
    /// </summary>
    public static UiFrame.InvitePrompt? Prompt(InviteReceived invite, DateTimeOffset? serverNow)
    {
        ArgumentNullException.ThrowIfNull(invite);
        if (invite.Kind != OfferKinds.Invite || invite.Private != true || !PromptId(invite.Id)) return null;
        var from = Cut(invite.From.Gamertag, 64);
        if (from.Length == 0 || Parse(invite.ExpiresAt) is not { } expires || serverNow is not { } now) return null;
        var left = Math.Ceiling((expires - now).TotalSeconds);
        if (left < 1) return null;
        var activity = invite.World.Activity is { } a && Encoding.UTF8.GetByteCount(a) <= UiFrame.MaxInviteActivity ? a : string.Empty;
        return new UiFrame.InvitePrompt(invite.Id, from, activity, (ushort)Math.Min(left, UiFrame.MaxInviteSeconds));
    }

    private static bool PromptId(string id) => id.Length is > 0 and <= UiFrame.MaxInviteId
        && id.All(c => char.IsAsciiLetterOrDigit(c) || c is ':' or '.' or '_' or '-');

    private static DateTimeOffset? Parse(string? at) => DateTimeOffset.TryParse(at, CultureInfo.InvariantCulture,
        DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out var parsed) ? parsed : null;

    private static TimeSpan Min(TimeSpan a, TimeSpan b) => a < b ? a : b;

    private static TimeSpan Max(TimeSpan a, TimeSpan b) => a > b ? a : b;

    private static string Cut(string text, int maxBytes)
    {
        if (Encoding.UTF8.GetByteCount(text) <= maxBytes) return text;
        var cut = new StringBuilder();
        var used = 0;
        foreach (var rune in text.EnumerateRunes())
        {
            if (used + rune.Utf8SequenceLength > maxBytes) break;
            used += rune.Utf8SequenceLength;
            cut.Append(rune.ToString());
        }
        return cut.ToString();
    }
}
