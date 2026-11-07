// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Security.Cryptography;
using Infinity.Core.Overlay;
using Infinity.Core.Process;
using Infinity.Core.Supervisor;

namespace Infinity.App;

/// <summary>
/// OD-50: what the in-game overlay shows, decided here (the overlay holds no tokens and no strings
/// of its own for these): the status line and the Xbox-style toasts, localised, sent through the
/// supervisor's UI pipe. Every player gets these notifications and every sidebar feature
/// (CR-S69, OD-129); the tier is only the header's badge: Infinity Racer, or Infinity Supporter.
/// </summary>
public sealed partial class ShellViewModel
{
    private const string OverlayFile = "infinity-overlay.dll";
    private bool _overlaySupporter;
    private bool _overlayWarned;
    private DateTimeOffset _overlayLastWarning = DateTimeOffset.MinValue;

    /// <summary>
    /// The overlay next to the launcher (<c>overlay\infinity-overlay.dll</c> + <c>.sig</c>, or
    /// <c>FI_OVERLAY_PACKAGE</c> against a development server), only when its signature verifies
    /// like the hook's (OD-47): the release key, or the development key on a development server.
    /// </summary>
    private OverlayPackage? LoadOverlayPackage()
    {
        _overlayOfficial = false;
        try
        {
            var folder = (DevServer ? Environment.GetEnvironmentVariable("FI_OVERLAY_PACKAGE") : null) ?? Path.Combine(AppContext.BaseDirectory, "overlay");
            var dll = Path.Combine(folder, OverlayFile);
            if (!File.Exists(dll) || !File.Exists(dll + ".sig") || new FileInfo(dll).Length > HookInstaller.MaxHookBytes)
            {
                Log("overlay: no signed package next to the launcher");
                return null;
            }
            var sha = SHA256.HashData(File.ReadAllBytes(dll));
            var signer = HookInstaller.SignerOf(Convert.ToHexStringLower(sha), File.ReadAllBytes(dll + ".sig"), DevHookKey);
            _overlayOfficial = signer == HookSigner.Official;
            if (signer == HookSigner.None)
            {
                Log("overlay: the package's signature does not verify; not loaded");
                return null;
            }
            Log($"overlay: package signed ({signer})");
            return new OverlayPackage(Path.GetFullPath(dll), sha);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log("overlay: " + e.Message);
            return null;
        }
        finally
        {
            Changed(nameof(ShowBuildOriginWarning));
        }
    }

    /// <summary>The overlay's Worlds tab against this server's matchmaking (OD-54), with the session's game ticket.</summary>
    private Func<Func<string>, Infinity.Core.Matchmaking.WorldsBridge>? WorldsFor(string mode, string xuid, Infinity.Core.Online.OnlineActivity onlineActivity,
        Infinity.Core.Matchmaking.IModSetSource mods)
    {
        if (_session?.Info.Services?.Matchmaking is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out var mm))
        {
            Log("worlds: no matchmaking origin in the server's info");
            return null;
        }
        var session = _session;
        var integrityApplies = IntegrityApplies; // OD-113
        var integrity = integrityApplies ? _integrity : null;
        var relayKeys = IntegratedRelayKeys(mm);
        // OD-193 F-2: this account's own profiles get no friend item in the game's PLAYER ACTIONS (read here, on the shell's thread)
        IReadOnlyCollection<string> ownXuids = Profiles.Select(p => p.Xuid).Append(xuid).Where(x => !string.IsNullOrEmpty(x)).ToHashSet(StringComparer.Ordinal);
        return ticket =>
        {
            var matchmaking = new Infinity.Core.Matchmaking.MatchmakingClient(
                MatchmakingHttp(mm), mm, ClientHeader, ticket);
            // CR-S68: this profile's mutes reach the server (POST /launcher/v1/voice/mutes); a 202 marks the history entry sent
            Infinity.Core.Voice.VoiceRoom? voiceRoom = null;
            var voiceMutes = new Infinity.Core.Voice.VoiceMuteSender(
                async (mute, ct) => await session.Api.VoiceMuteAsync(await session.Tokens.AccessAsync(ct).ConfigureAwait(false), mute, ct).ConfigureAwait(false), Log)
            { Accepted = target => voiceRoom?.MuteSent(target) };
            voiceRoom = new Infinity.Core.Voice.VoiceRoom(new Infinity.Core.Voice.VoiceStore(_stateDirectory), mode, xuid) { MuteReported = voiceMutes.Report };
            Infinity.Core.Matchmaking.WorldsBridge? bridge = null;
            // OD-193 F-2: the game's PLAYER ACTIONS friend and report items; a report names the world only when the roster lists the player
            var playerActions = new Infinity.Core.Matchmaking.PlayerActions(
                new Infinity.Core.Matchmaking.ApiPlayerActions(session.Api, ct => session.Tokens.AccessAsync(ct)),
                () => ownXuids, target => bridge?.RosterWorldOf(target), log: Log);
            return _playWorlds = bridge = new Infinity.Core.Matchmaking.WorldsBridge(
                matchmaking, key => S[key], ProofClock(session), mode,
                () => Infinity.Core.Matchmaking.HostRelay.LanAddresses(mm), xuid, Log,
                checkWorldAccess: ct => CheckWorldAccessAsync(integrityApplies, integrity, ct),
                // CR-S69: invites and requests to join reach every ticket, so no tier skips the background seat read
                onlineActivity: onlineActivity, idleWhenNoWorld: false)
            {
                Social = new Infinity.Core.Matchmaking.WorldSocial(session.Api, ct => session.Tokens.AccessAsync(ct)),
                PlayerActions = playerActions,
                // T-L4 (OD-204): the invite inbox (GET /launcher/v1/invites every 10 s while online) and its decline route
                Inbox = new Infinity.Core.Matchmaking.InviteInbox(
                    async ct => await session.Api.InvitesAsync(await session.Tokens.AccessAsync(ct).ConfigureAwait(false), null, ct).ConfigureAwait(false),
                    async (id, ct) => await session.Api.DeclineInviteAsync(await session.Tokens.AccessAsync(ct).ConfigureAwait(false), id, ct).ConfigureAwait(false),
                    key => S[key], log: Log),
                ModSet = mods,
                ModName = LobbyModName, // OD-181: the catalog's names of the mods players declare
                SocketProofRequired=SocketProofRequired,SocketProofKeys=relayKeys,
                TrustedServerNow=session.Api.TrustedServerNow,
                // OD-154: voice chat's settings and this profile's mute history (voice.json), shown by the overlay
                Voice = voiceRoom,
                // CR-S68: the voice_blocked notice's end (GET /core/v1/me/notices), told once per block in the player's language
                AccountNotices = async ct => (await session.Api.AccountNoticesAsync(await session.Tokens.AccessAsync(ct).ConfigureAwait(false), ct).ConfigureAwait(false)).Items,
                // NR-6: the parts rule judges the other players' cars against this PC's own game database (read-only)
                GameDatabase = ContentLocation?.Mount is { } gameMount ? Infinity.Core.WorldServer.IntegratedRelayOptions.FindGameDatabase(gameMount) : null,
            };
        };
    }

    /// <summary>
    /// OD-181 (CR-L48): a declared mod's name in the player's language, from the loaded catalog by its multiplayer id
    /// (<c>compatibilityId</c>); null when the catalog does not know it (the overlay then shows the id).
    /// </summary>
    private string? LobbyModName(string compatibilityId)
    {
        var catalog = _manifestCatalog;
        var mod = catalog.FirstOrDefault(m => string.Equals(m.Multiplayer?.CompatibilityId, compatibilityId, StringComparison.Ordinal));
        return mod?.Text("name", S.Locale);
    }

    /// <summary>The official server and a custom one (CR-L39) prove the relay socket against the server's clock and relay keys.</summary>
    private bool SocketProofRequired => _config.Production || _config.Custom;

    private TimeProvider? ProofClock(Infinity.Core.Auth.AccountSession session) => SocketProofRequired ? session.Api.TrustedTimeProvider() : null;

    private Infinity.Core.Matchmaking.RelayGrantVerifyKeys IntegratedRelayKeys(Uri matchmaking)
    {
        if (_config.Production)
        {
            Infinity.Core.Auth.ProductionDeployment.RequireOrigin(matchmaking, Infinity.Core.Auth.ProductionDeployment.Matchmaking);
            return Infinity.Core.Auth.ProductionDeployment.RelayKeys();
        }
        // CR-L39: the relay keys the custom server published and its realm's pin matched (none: worlds stay unavailable).
        if (_config.Custom)
            return _session?.Info.Trust is { } trust ? new([.. Infinity.Core.Auth.CustomServer.Keys(trust.RelayGrant ?? [])]) : new();
#if INFINITY_PRODUCTION_BUILD
        return new();
#else
        if (!DevServer || !Infinity.Core.DevNet.PlainHttpUrl(matchmaking.OriginalString)) return new();
        var configured = Environment.GetEnvironmentVariable("FI_DEV_RELAY_VERIFY_KEYS");
        // a development run needs no extra variable: the local stack's public ring by default
        if (string.IsNullOrWhiteSpace(configured)) configured = Infinity.Core.Matchmaking.RelayGrantVerifyKeys.LocalStack;
        try { return Infinity.Core.Matchmaking.RelayGrantVerifyKeys.Parse(configured); }
        catch (ArgumentException) { Log("worlds: invalid development relay public-key configuration"); return new(); }
#endif
    }

    private void OverlayStatus(byte state, string detail) =>
        _commands?.Writer.TryWrite(new SupervisorCommand.OverlayStatus(
            new UiFrame.Status(_gate.ProfileTag ?? string.Empty, _overlaySupporter ? UiTier.Supporter : UiTier.Free, state, detail)));

    /// <summary>One toast: <paramref name="kind"/> is the overlay's look (0 info, 1 success, 2 warning, 3 error, 4 achievement, 5 update, 6 friend).</summary>
    private void OverlayToast(byte kind, string title, string body)
    {
        var level = kind switch { 3 => UiLevel.Error, 2 => UiLevel.Warn, _ => UiLevel.Info };
        _commands?.Writer.TryWrite(new SupervisorCommand.OverlayToast(new UiFrame.Toast(level, kind, 50, title + "\n" + body)));
    }

    /// <summary>The overlay's side of each supervisor event.</summary>
    private void OverlayOn(SupervisorEvent e)
    {
        switch (e)
        {
            case SupervisorEvent.Admitted a:
                _overlaySupporter = UiTier.Of(a.Pool.Tier) == UiTier.Supporter;
                _overlayWarned = false;
                break;
            case SupervisorEvent.Running:
                OverlayStatus(UiState.Running, string.Empty);
                // OD-125 follow-up: the first notice also says how to open the overlay (LB+RB; View is Azimuth's)
                OverlayToast(1, S["overlay.connected"], (_overlaySupporter ? S["overlay.connected_supporter"] : S["overlay.connected_racer"]) + "\n" + S["overlay.open_hint"]);
                break;
            case SupervisorEvent.Refused r:
                OverlayStatus(UiState.Offline, r.Reason);
                OverlayToast(3, S["overlay.offline"], S["overlay.offline_body"] + "\n" + S["overlay.open_hint"]);
                break;
            case SupervisorEvent.Error err when err.Message.StartsWith("heartbeat", StringComparison.Ordinal):
                if (DateTimeOffset.UtcNow - _overlayLastWarning > TimeSpan.FromMinutes(1))
                {
                    _overlayLastWarning = DateTimeOffset.UtcNow;
                    _overlayWarned = true;
                    OverlayStatus(UiState.Exiting, S["overlay.unstable_body"]);
                    OverlayToast(2, S["overlay.unstable"], S["overlay.unstable_body"]);
                }
                break;
            case SupervisorEvent.TokenPushed when _overlayWarned:
                _overlayWarned = false;
                OverlayStatus(UiState.Running, string.Empty);
                OverlayToast(1, S["overlay.reconnected"], _overlaySupporter ? S["overlay.connected_supporter"] : S["overlay.connected_racer"]);
                break;
            case SupervisorEvent.OverlayConnected c:
                Log("overlay: connected " + c.Version);
                break;
        }
    }
}
