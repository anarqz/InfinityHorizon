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
    // Sent: what the overlay shows of it now. None: nothing (INVITE_UX: held back while this player races, Holding; or its accept could not
    // be served yet); Accept: the hold-Y toast; Notice (OD-223 revised): the same toast with "Go into Multiplayer to accept it", while the
    // game is off the screens that take invites. Sync moves every pending one to the form the game's screen asks for.
    private sealed record Shown(UiFrame.InvitePrompt Prompt, long Deadline, bool Answered, string WorldId = "", string? Activity = null,
        GameSessionReference? Session = null, long ReofferAt = 0, bool FromRequest = false, Form Sent = Form.None);

    private enum Form
    {
        None,
        Accept,
        Notice,
    }

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
    /// MULTIPLAYER hoppers screen). OD-223 revised (owner 2026-11-08): nothing is ever declined automatically (FM5's own UI never hears of
    /// it, so the inviter could not invite again). A pending invite is HELD: the overlay shows it as a notice ("Go into Multiplayer to accept
    /// it", no hold), it becomes the hold-Y accept toast as soon as the game is there (<see cref="SyncScreenAsync"/>, every poll), back to the
    /// notice when the game leaves, and it goes silently when it lapses. An accept that comes off those screens is never served: the invite
    /// stays held. Null: always (FH2).
    /// </summary>
    public Func<bool>? Acceptable { get; set; }

    private bool AcceptableNow() => Acceptable?.Invoke() != false;

    /// <summary>
    /// OD-223 revised: every pending prompt to the form the game's screen asks for now (<see cref="Acceptable"/>: the accept toast, else the
    /// notice); one held back (a race, an accept not served yet) is sent when it may be. Called when the game's screen changes and after an
    /// accept. Never declines.
    /// </summary>
    public async Task SyncScreenAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        var outgoing = new List<UiFrame>();
        var holding = Holding?.Invoke() == true;
        var acceptable = AcceptableNow();
        lock (_lock) Sync(outgoing, _clock.GetTimestamp(), holding, acceptable);
        LogSent(outgoing);
        foreach (var frame in outgoing) await send(frame, cancellation).ConfigureAwait(false);
    }

    // under _lock: each unanswered prompt still in time whose shown form is not the screen's is sent in that form (with the time it has
    // left); one the overlay does not show waits while this player races (Holding) or until its re-offer time (T-L5)
    private void Sync(List<UiFrame> outgoing, long now, bool holding, bool acceptable)
    {
        var form = acceptable ? Form.Accept : Form.Notice;
        foreach (var (id, shown) in _shown.ToList())
        {
            if (shown.Answered || shown.Deadline <= now || shown.Sent == form) continue;
            if (shown.Sent == Form.None && (holding || shown.ReofferAt > now)) continue;
            _shown[id] = shown with { Sent = form, ReofferAt = 0 };
            outgoing.Add(Left(shown, now) with { Notice = form == Form.Notice });
        }
    }

    private void LogSent(List<UiFrame> outgoing)
    {
        var accepts = outgoing.Count(f => f is UiFrame.InvitePrompt { ExpiresInSec: > 0, Notice: false });
        var notices = outgoing.Count(f => f is UiFrame.InvitePrompt { ExpiresInSec: > 0, Notice: true });
        var gone = outgoing.Count(f => f is UiFrame.InvitePrompt { ExpiresInSec: 0 });
        if (accepts > 0) _log($"invites: {accepts} private lobby invite(s) prompted");
        if (notices > 0) _log($"invites: {notices} private lobby invite(s) shown as a notice (the game is not on a screen that takes invites, OD-223)");
        if (gone > 0) _log($"invites: {gone} prompt(s) withdrawn (answered elsewhere, gone or expired)");
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
        var held = 0;
        lock (_lock)
        {
            foreach (var invite in list.Items)
            {
                if (Prompt(invite, serverNow) is not { } prompt) continue;
                listed.Add(invite.Id);
                if (_shown.ContainsKey(invite.Id)) continue;
                // a new one: Sync below sends it as the screen asks (INVITE_UX: not while racing; OD-223 revised: a notice off the screen)
                _shown[invite.Id] = new Shown(prompt, now + (long)(prompt.ExpiresInSec * (double)_clock.TimestampFrequency), false,
                    invite.World.Id, invite.World.Activity, invite.Session);
                if (holding) held++;
            }
            foreach (var (id, shown) in _shown.ToList())
            {
                // OD-223 revised: one the server no longer lists, or past its expiresAt, goes silently (its toast withdrawn, nothing sent)
                if ((listed.Contains(id) || shown.FromRequest) && shown.Deadline > now) continue;
                _shown.Remove(id);
                if (!shown.Answered && shown.Sent != Form.None) outgoing.Add(UiFrame.InvitePrompt.Withdraw(id));
            }
            Sync(outgoing, now, holding, acceptable);
        }
        if (held > 0) _log($"invites: {held} new private lobby invite(s) held back while racing");
        LogSent(outgoing);
        foreach (var frame in outgoing) await send(frame, cancellation).ConfigureAwait(false);
    }

    /// <summary>T-L3's open item: every pending, unanswered prompt again, with the seconds it has left (an overlay's hello).</summary>
    public async Task ResendAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        var now = _clock.GetTimestamp();
        var form = AcceptableNow() ? Form.Accept : Form.Notice; // OD-223 revised: off the screens that take invites, as notices
        var again = new List<UiFrame.InvitePrompt>();
        lock (_lock)
        {
            foreach (var (id, s) in _shown.ToList())
            {
                if (s.Answered || s.Sent == Form.None || s.Deadline <= now) continue;
                _shown[id] = s with { Sent = form };
                again.Add(Left(s, now) with { Notice = form == Form.Notice });
            }
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
            // OD-223 revised: the game left the screens that take invites before the accept came: never served, never declined; it stays
            // held (SyncScreenAsync, which the supervisor runs after an accept, shows it again as a notice)
            if (!AcceptableNow())
            {
                lock (_lock)
                {
                    if (_shown.TryGetValue(inviteId, out var still) && still.Answered)
                        _shown[inviteId] = still with { Answered = false, Sent = Form.None, ReofferAt = 0 };
                }
                _log("invites: accepted off the screens that take invites; held, not served (OD-223)");
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
                        _shown[inviteId] = still with { Answered = false, Sent = Form.None, ReofferAt = _clock.GetTimestamp() + (long)(ReofferAfter.TotalSeconds * _clock.TimestampFrequency) };
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
        var form = acceptable ? Form.Accept : Form.Notice;
        lock (_lock)
        {
            if (_shown.ContainsKey(requestId)) return;
            // OD-223 revised: the same hold rule as an invite's: off the screens that take invites a notice, the accept toast once there
            _shown[requestId] = new Shown(prompt, _clock.GetTimestamp() + (long)(AcceptedRequestPrompt.TotalSeconds * _clock.TimestampFrequency), false,
                worldId, activity, null, FromRequest: true, Sent: form);
        }
        _log(acceptable ? "invites: an accepted request to join is prompted as an invite"
            : "invites: an accepted request to join is shown as a notice (the game is not on a screen that takes invites, OD-223)");
        await send(prompt with { Notice = !acceptable }, cancellation).ConfigureAwait(false);
    }

    private async Task WithdrawAllAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        List<string> ids;
        lock (_lock)
        {
            ids = _shown.Where(s => !s.Value.Answered && s.Value.Sent != Form.None).Select(s => s.Key).ToList();
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
