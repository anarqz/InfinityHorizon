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
    // CR-S102: FromRequest, a prompt made by this launcher for this player's own request that the leader accepted (the server lists it in
    // no inbox): it ends at its deadline only, and its decline sends nothing (the seat the accept held lapses on its own)
    // INVITE_UX: Held, a prompt not sent yet because this player raced when it came (Holding); it is sent, with the time it has left, at the
    // first poll after the race
    private sealed record Shown(UiFrame.InvitePrompt Prompt, long Deadline, bool Answered, string WorldId = "", string? Activity = null,
        GameSessionReference? Session = null, long ReofferAt = 0, bool FromRequest = false, bool Held = false);

    /// <summary>CR-S102: how long the prompt of an accepted request to join stays (the seat it holds is the server's to keep or lapse).</summary>
    public static readonly TimeSpan AcceptedRequestPrompt = TimeSpan.FromSeconds(60);

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
    /// W15 (OD-211): sees every good answer before it is applied (the friends sidebar's join requests and online toasts); its failures
    /// never stop the inbox.
    /// </summary>
    public Func<InviteList, Func<UiFrame, CancellationToken, Task>, CancellationToken, Task>? Observer { get; set; }

    /// <summary>
    /// INVITE_UX (owner 2026-11-08): true while this player races (the bridge's <see cref="WorldsBridge.IsRacingNow"/>). A new invite then
    /// shows no toast: it waits, and the first poll after the race sends it with the time it has left (one that lapsed meanwhile is gone).
    /// </summary>
    public Func<bool>? Holding { get; set; }

    /// <summary>
    /// OD-223 (owner 2026-11-08, T-1108-M): false while the game is not on a screen where it may take an invite (the bridge's
    /// <see cref="WorldsBridge.InviteScreenAllows"/>, from the game's <see cref="Process.GameDefinition.InviteAcceptScreens"/>; FM5: its
    /// MULTIPLAYER hoppers screen). Then every pending invite is declined at once (<c>POST /launcher/v1/invites/{id}/decline</c>) with no
    /// prompt and no toast, one already shown is withdrawn and declined, and an accept is never served. Null: always (FH2).
    /// </summary>
    public Func<bool>? Acceptable { get; set; }

    private bool AcceptableNow() => Acceptable?.Invoke() != false;

    /// <summary>
    /// OD-223: when the game is not where it may take an invite (<see cref="Acceptable"/>), every unanswered prompt is withdrawn (one that
    /// showed) and declined (one the server lists; an accepted request to join is only dropped). Called when the game's screen changes.
    /// </summary>
    public async Task EnforceAcceptableAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        if (AcceptableNow()) return;
        var outgoing = new List<UiFrame>();
        var declines = new List<string>();
        lock (_lock) DropOffScreen(outgoing, declines);
        foreach (var frame in outgoing) await send(frame, cancellation).ConfigureAwait(false);
        await DeclineOffScreenAsync(declines, cancellation).ConfigureAwait(false);
    }

    // OD-223, under _lock: every unanswered prompt is answered here; a shown one is withdrawn, a server-listed one is to be declined
    private void DropOffScreen(List<UiFrame> outgoing, List<string> declines)
    {
        foreach (var (id, shown) in _shown.ToList())
        {
            if (shown.Answered) continue;
            _shown[id] = shown with { Answered = true, ReofferAt = 0 };
            if (!shown.Held) outgoing.Add(UiFrame.InvitePrompt.Withdraw(id));
            if (!shown.FromRequest) declines.Add(id);
        }
    }

    // OD-223: the automatic declines (one retry of a transport failure); never a toast, a failure only lapses on its own
    private async Task DeclineOffScreenAsync(List<string> ids, CancellationToken cancellation)
    {
        foreach (var id in ids)
        {
            try
            {
                try { await _decline(id, cancellation).ConfigureAwait(false); }
                catch (HttpRequestException) when (!cancellation.IsCancellationRequested) { await _decline(id, cancellation).ConfigureAwait(false); }
                _log("invites: an invite was declined automatically (the game is not on a screen that takes invites, OD-223)");
            }
            catch (Exception error) when (!cancellation.IsCancellationRequested && error is ApiException or HttpRequestException
                or OperationCanceledException or IOException or InvalidOperationException or TimeoutException)
            {
                _log($"invites: an automatic decline failed ({(error is ApiException api ? $"HTTP {(int)api.Status} {api.Code}" : error.GetType().Name)}); it lapses on its own");
            }
        }
    }

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
                    if (Observer is { } observer)
                    {
                        try { await observer(list, send, cancellation).ConfigureAwait(false); }
                        catch (Exception error) when (!cancellation.IsCancellationRequested && error is ApiException or HttpRequestException or IOException
                            or InvalidOperationException or ProtocolException or System.Text.Json.JsonException)
                        {
                            _log($"invites: the friends observer failed ({error.GetType().Name})");
                        }
                    }
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
        var holding = Holding?.Invoke() == true;
        var acceptable = AcceptableNow();
        var declines = new List<string>();
        var held = 0;
        lock (_lock)
        {
            foreach (var invite in list.Items)
            {
                if (Prompt(invite, serverNow) is not { } prompt) continue;
                listed.Add(invite.Id);
                if (_shown.TryGetValue(invite.Id, out var known))
                {
                    if (!acceptable) continue; // OD-223: nothing shows again off those screens (DropOffScreen answers it below)
                    // INVITE_UX: one held back during a race comes once the race is over, with what is left of its time
                    if (known.Held)
                    {
                        if (!holding && !known.Answered && known.Deadline > now) // OD-223: one declined off the screen never comes
                        {
                            _shown[invite.Id] = known with { Held = false };
                            outgoing.Add(Left(known, now));
                        }
                        continue;
                    }
                    // T-L5: an accept that could not be served then is shown again once its wait is over (with the time it has left)
                    if (!holding && !known.Answered && known.ReofferAt > 0 && known.ReofferAt <= now && known.Deadline > now)
                    {
                        _shown[invite.Id] = known with { ReofferAt = 0 };
                        outgoing.Add(Left(known, now));
                    }
                    continue;
                }
                // OD-223: off the screens that take invites, a new one never shows (DropOffScreen declines it below)
                _shown[invite.Id] = new Shown(prompt, now + (long)(prompt.ExpiresInSec * (double)_clock.TimestampFrequency), false,
                    invite.World.Id, invite.World.Activity, invite.Session, Held: holding || !acceptable);
                if (!acceptable) continue;
                if (holding) held++;
                else outgoing.Add(prompt);
            }
            foreach (var (id, shown) in _shown.ToList())
            {
                if ((listed.Contains(id) || shown.FromRequest) && shown.Deadline > now) continue;
                _shown.Remove(id);
                if (!shown.Answered && !shown.Held) outgoing.Add(UiFrame.InvitePrompt.Withdraw(id));
            }
            if (!acceptable) DropOffScreen(outgoing, declines);
        }
        if (held > 0) _log($"invites: {held} new private lobby invite(s) held back while racing");
        var added = outgoing.Count(f => f is UiFrame.InvitePrompt { ExpiresInSec: > 0 });
        if (added > 0) _log($"invites: {added} new private lobby invite(s) prompted");
        if (outgoing.Count > added) _log($"invites: {outgoing.Count - added} prompt(s) withdrawn (answered elsewhere, gone or expired)");
        foreach (var frame in outgoing) await send(frame, cancellation).ConfigureAwait(false);
        await DeclineOffScreenAsync(declines, cancellation).ConfigureAwait(false);
    }

    /// <summary>T-L3's open item: every pending, unanswered prompt again, with the seconds it has left (an overlay's hello).</summary>
    public async Task ResendAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        if (!AcceptableNow()) return; // OD-223: off the screens that take invites nothing shows (EnforceAcceptableAsync declines them)
        var now = _clock.GetTimestamp();
        List<UiFrame.InvitePrompt> again;
        lock (_lock)
        {
            again = _shown.Values.Where(s => !s.Answered && !s.Held && s.Deadline > now && s.ReofferAt <= now).Select(s => Left(s, now)).ToList();
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
            // OD-223: the game left the screens that take invites before the accept came: it is declined, never served, and nothing shows
            if (!AcceptableNow())
            {
                _log("invites: accepted off the screens that take invites; declined instead (OD-223)");
                if (!answered.FromRequest) await DeclineOffScreenAsync([inviteId], cancellation).ConfigureAwait(false);
                return null;
            }
            var left = TimeSpan.FromSeconds((answered.Deadline - now) / (double)_clock.TimestampFrequency);
            InviteAcceptAnswer result;
            try
            {
                result = await join(new InviteAccept(inviteId, answered.WorldId, answered.Activity, left, answered.Session, answered.FromRequest), cancellation).ConfigureAwait(false);
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
            // INVITE_UX (owner 2026-11-08): after a hold accepted, nothing more is shown while the game moves in (it shows its own join);
            // a refusal or a "not now" still says why
            return result.Outcome == InviteAcceptOutcome.Moving ? null : result.Notice;
        }
        if (answered.FromRequest)
        {
            _log("invites: the accepted request to join was declined here; nothing is sent (its seat lapses on its own)");
            return null;
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

    /// <summary>
    /// CR-S102 (MATCHMAKING_API §2.10, §3.4): the leader accepted this player's request to join <paramref name="worldId"/> (the seat read
    /// said so). It shows as an invite (0x23, from <paramref name="leader"/>, for <see cref="AcceptedRequestPrompt"/>), then the invite's
    /// own accept flow (<see cref="AnswerAsync"/>). Shown once per request id.
    /// </summary>
    public async Task OfferAcceptedRequestAsync(string requestId, string worldId, string leader, string? activity, Func<UiFrame, CancellationToken, Task> send,
        CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        var from = Cut(leader ?? string.Empty, 64);
        if (!PromptId(requestId) || string.IsNullOrEmpty(worldId) || from.Length == 0)
        {
            _log("invites: an accepted request the pipe cannot carry; not shown");
            return;
        }
        var shownActivity = activity is { } a && Encoding.UTF8.GetByteCount(a) <= UiFrame.MaxInviteActivity ? a : string.Empty;
        var prompt = new UiFrame.InvitePrompt(requestId, from, shownActivity, (ushort)AcceptedRequestPrompt.TotalSeconds);
        var acceptable = AcceptableNow();
        lock (_lock)
        {
            if (_shown.ContainsKey(requestId)) return;
            // OD-223: off the screens that take invites it is never shown (answered at once; its seat lapses on its own, as a decline's)
            _shown[requestId] = new Shown(prompt, _clock.GetTimestamp() + (long)(AcceptedRequestPrompt.TotalSeconds * _clock.TimestampFrequency), !acceptable,
                worldId, activity, null, FromRequest: true);
        }
        if (!acceptable)
        {
            _log("invites: an accepted request to join came off the screens that take invites; not shown (OD-223)");
            return;
        }
        _log("invites: an accepted request to join is prompted as an invite");
        await send(prompt, cancellation).ConfigureAwait(false);
    }

    private async Task WithdrawAllAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        List<string> ids;
        lock (_lock)
        {
            ids = _shown.Where(s => !s.Value.Answered && !s.Value.Held).Select(s => s.Key).ToList();
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
