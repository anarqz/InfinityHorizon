// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>
/// Where the launcher talks to: the server (core and gate origins come from its <c>/info</c>) and the website used for account
/// self-service. <see cref="Custom"/> (CR-L39) is a server the player chose besides the official one, in every build.
/// </summary>
public sealed record ServerConfig(Uri Server, Uri Portal, bool Production = false, bool Custom = false)
{
    /// <summary>A player-chosen server (CR-L39): its own origin as server and portal; validated by <see cref="CustomServer"/>.</summary>
    public static ServerConfig ForCustom(Uri origin)
    {
        var server = CustomServer.ParseOrigin(origin?.ToString() ?? throw new ArgumentNullException(nameof(origin)));
        CustomOrigins.Allow(server);
        return new ServerConfig(server, server, Production: false, Custom: true);
    }

    /// <summary>A custom server's key pins (<c>custom-trust.json</c> in its own state folder); required for <see cref="Custom"/>.</summary>
    public TrustPins? Pins { get; init; }

    /// <summary>Official builds use production only; development builds keep their explicit local selection (<see cref="ServerSelection"/>).</summary>
    public static ServerConfig Default { get; } = new(ServerSelection.DefaultServer, ServerSelection.DefaultPortal, ServerSelection.DefaultProduction);

    /// <summary><c>--server &lt;url&gt;</c> / <c>--portal &lt;url&gt;</c>, then <c>FI_SERVER</c> / <c>FI_PORTAL</c>, then <see cref="Default"/>.</summary>
    public static ServerConfig From(IReadOnlyList<string> args, Func<string, string?> env)
    {
        var (server, portal, production) = ServerSelection.Resolve(args, env);
        var config = new ServerConfig(server, portal, production);
        config.Validate();
        return config;
    }

    public void Validate()
    {
        if (Custom)
        {
            if (Production || !Server.Equals(CustomServer.ParseOrigin(Server.ToString())) || Portal != Server)
                throw new ArgumentException("A custom server is its own origin");
            CustomOrigins.Allow(Server);
            return;
        }
        ServerSelection.Validate(Server, Portal, Production);
    }

    /// <summary>The official server in <c>production</c>, a custom one in <c>custom\&lt;scheme&gt;-&lt;host&gt;-&lt;port&gt;</c>, development in the root.</summary>
    public string StateDirectory(string root) =>
        Production ? Path.Combine(root, "production")
        : Custom ? Path.Combine(root, "custom", $"{Server.Scheme}-{Server.IdnHost.ToLowerInvariant()}-{Server.Port.ToString(System.Globalization.CultureInfo.InvariantCulture)}")
        : root;
}

/// <summary>
/// A signed-in account: the API client, its <see cref="TokenManager"/> and the last <c>/me</c>.
/// Everything here is async and never touches the UI thread's state.
/// </summary>
public sealed class AccountSession : IDisposable
{
    private bool _deletionKnown;
    private Deletion? _deletion;
    private AccountSession(ApiClient api, TokenManager tokens, Device device, ServerInfo info, string? email)
    {
        Api = api;
        Tokens = tokens;
        Device = device;
        Info = info;
        Email = email;
    }

    public ApiClient Api { get; }

    public TokenManager Tokens { get; }

    public Device Device { get; }

    public ServerInfo Info { get; }

    /// <summary>The identity's e-mail when the server said it (a <c>needs_account</c> answer).</summary>
    public string? Email { get; }

    public Me? Me { get; private set; }

    /// <summary>A successful deletion mutation remains authoritative if a subsequent /me read fails.</summary>
    public Deletion? Deletion => _deletionKnown ? _deletion : Me?.Deletion;

    /// <summary><c>/info</c>: the origins and the Identity Platform config of the native sign-in.</summary>
    public static async Task<(ServerInfo Info, ApiClient Api)> ConnectAsync(HttpClient http, ServerConfig config, string clientHeader, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(config);
        config.Validate();
        var info = await ApiClient.InfoAsync(http, config.Server, clientHeader, cancellation).ConfigureAwait(false);
        if (config.Production) ProductionDeployment.ValidateInfo(info);
        else if (config.Custom)
        {
            CustomServer.ValidateInfo(info, config.Server);
            var pins = config.Pins ?? throw new InvalidOperationException("A custom server needs its key pins");
            switch (pins.Check(config.Server, info.Realm, info.Trust!))
            {
                case TrustPin.FirstUse: pins.Pin(config.Server, info.Realm, info.Trust!); break;
                case TrustPin.Changed: throw new TrustChangedException(config.Server, info.Realm, info.Trust!);
            }
            foreach (var origin in CustomServer.Origins(info, config.Server)) CustomOrigins.Allow(origin);
        }
        else ProductionDeployment.ValidateDevelopmentInfo(info);
        var (core, gate) = ApiClient.Origins(info, config.Server);
        return (info, new ApiClient(http, core, gate, clientHeader));
    }

    /// <summary>
    /// The native sign-in's exchange (OD-43): the ID token Identity Platform just minted, on
    /// <c>grantType: id_token</c>, with one Idempotency-Key per ID token. <c>AUTH.EMAIL_UNVERIFIED</c>
    /// and the other refusals surface as <see cref="ApiException"/>.
    /// </summary>
    public static async Task<(AccountSession? Session, PendingAccount? Pending)> SignInWithIdTokenAsync(
        ApiClient api, ServerInfo info, Device device, string idToken, CancellationToken cancellation, string? email = null)
    {
        ArgumentNullException.ThrowIfNull(api);
        var result = await api.ExchangeIdTokenAsync(new IdTokenGrant(idToken, device), Guid.CreateVersion7().ToString(), cancellation).ConfigureAwait(false);
        return result switch
        {
            // The e-mail the player signed in with (signed_in carries none) labels the top bar.
            SignedIn signedIn => (new AccountSession(api, TokenManager.From(api, signedIn), device, info, email), null),
            NeedsAccount needs => (null, new PendingAccount(api, device, info, needs)),
            _ => throw new InvalidOperationException("unknown sign-in answer"),
        };
    }

    /// <summary>Re-reads <c>/me</c> (profiles come from the server only).</summary>
    public async Task<Me> RefreshMeAsync(CancellationToken cancellation = default)
    {
        Me = await Tokens.WithAccessAsync(access => Api.MeAsync(access, cancellation), cancellation).ConfigureAwait(false);
        _deletionKnown = false;
        return Me;
    }

    /// <summary>Password goes only to Identity Platform. The FI request carries a fresh ID token, never a cached token or password.</summary>
    public async Task<Deletion> RequestDeletionAsync(IdentityPlatform idp, string email, string password, string confirmGamertag, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(idp);
        var idToken = await idp.SignInAsync(email, password, cancellation).ConfigureAwait(false);
        var request = new LauncherDeletionRequest(confirmGamertag, new Reauth(idToken));
        var key = Guid.CreateVersion7().ToString();
        var deletion = await Tokens.WithAccessAsync(access => Api.RequestDeletionAsync(access, request, key, cancellation), cancellation).ConfigureAwait(false);
        _deletion = deletion;
        _deletionKnown = true;
        return deletion;
    }

    public async Task CancelDeletionAsync(CancellationToken cancellation = default)
    {
        try
        {
            await Tokens.WithAccessAsync(async access =>
            {
                await Api.CancelDeletionAsync(access, cancellation).ConfigureAwait(false);
                return true;
            }, cancellation).ConfigureAwait(false);
        }
        catch (ApiException e) when (e.Code == ErrorCodes.AccountsNotPendingDeletion)
        {
            // Another client may already have cancelled it; the server confirms the same state.
        }
        _deletion = null;
        _deletionKnown = true;
    }

    /// <summary>
    /// Creates a profile on the server: <c>noob</c>, or <c>hardcore</c> (every account's, OD-140),
    /// which starts empty: the game seeds it on the first launch and that career is its first cloud
    /// revision (OD-60; no starter car).
    /// </summary>
    public Task<Profile> CreateProfileAsync(string game, string name, string mode = "noob", CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.CreateProfileAsync(access, new NewProfile(game, name, mode), cancellation), cancellation);

    /// <summary>
    /// Hardcore → Noob, step 1 (OD-52): what the change would delete. Nothing is written; null when
    /// the profile is already Noob (the server answers 200).
    /// </summary>
    public async Task<ModeChangeConfirm?> AskNoobAsync(string xuid, string? game = null, CancellationToken cancellation = default)
    {
        try
        {
            await Tokens.WithAccessAsync(access => Api.ChangeModeAsync(access, xuid, new ModeChange("noob", null), Guid.CreateVersion7().ToString(),
                CareerGame(game), cancellation), cancellation).ConfigureAwait(false);
            return null;
        }
        catch (ApiException e) when (e.Code == ModeChangeConfirm.Code)
        {
            return ModeChangeConfirm.From(e.Details) ?? new ModeChangeConfirm(0, new Dictionary<string, long>());
        }
    }

    /// <summary>Hardcore → Noob, step 2: confirmed. The profile's cloud data is deleted for good; friends stay.</summary>
    public Task<Profile> ConfirmNoobAsync(string xuid, string idempotencyKey, string? game = null, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.ChangeModeAsync(access, xuid, new ModeChange("noob", true), idempotencyKey, CareerGame(game), cancellation), cancellation);

    /// <summary>
    /// CR-S61: deletes the profile from the cloud at once, for good. <paramref name="idempotencyKey"/> is the caller's, kept for every
    /// retry of this deletion; 204 and 404 are <see cref="Shell.ProfileDeletionResult.Deleted"/>, the contract's refusals outcomes.
    /// </summary>
    public Task<Shell.ProfileDeletionOutcome> DeleteProfileAsync(string xuid, string idempotencyKey, string? game = null,
        CancellationToken cancellation = default) =>
        Shell.ProfileDeletion.SendAsync(ct => Tokens.WithAccessAsync(async access =>
        {
            await Api.DeleteProfileAsync(access, xuid, idempotencyKey, CareerGame(game), ct).ConfigureAwait(false);
            return true;
        }, ct), () => Api.ServerNow(TimeProvider.System), cancellation); // a ticket's expiry is on the server's clock

    /// <summary>
    /// CR-S98 "delete profile" (<c>DELETE /players/{xuid}</c>): every career, then the profile; outcomes as
    /// <see cref="DeleteProfileAsync"/> (same key rules, shared limit, refused whole while a career is in use).
    /// </summary>
    public Task<Shell.ProfileDeletionOutcome> DeletePlayerAsync(string xuid, string idempotencyKey, CancellationToken cancellation = default) =>
        Shell.ProfileDeletion.SendAsync(ct => Tokens.WithAccessAsync(async access =>
        {
            await Api.DeletePlayerAsync(access, xuid, idempotencyKey, ct).ConfigureAwait(false);
            return true;
        }, ct), () => Api.ServerNow(TimeProvider.System), cancellation);

    /// <summary>
    /// CR-S98 <c>POST /players/{xuid}/careers {game}</c>: the profile's empty career of <paramref name="game"/>. Refusals are thrown as
    /// <see cref="ApiException"/> (<see cref="Shell.CareerStart.Classify"/> reads them).
    /// </summary>
    public Task<Profile> CreateCareerAsync(string xuid, string game, string idempotencyKey, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.CreateCareerAsync(access, xuid, new CareerCreate(game), idempotencyKey, cancellation), cancellation);

    /// <summary>
    /// CR-S98: <paramref name="game"/> for a <c>/profiles/{xuid}*</c> route's <c>?game=</c> on a server that knows careers
    /// (<c>players</c> in <c>/me</c>, CAREERS.md §6.11); null on an older one, where the bare route means FH2.
    /// </summary>
    public string? CareerGame(string? game) => Me?.Players is not null && !string.IsNullOrEmpty(game) ? game : null;

    /// <summary>CR-S62: a moderator's kick (the caller's key, kept for the retries of this one action).</summary>
    public Task<KickApplied> KickAsync(NewKick kick, string idempotencyKey, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.KickAsync(access, kick, idempotencyKey, cancellation), cancellation);

    /// <summary>A moderator's manual strike (CORE_API.md §5).</summary>
    public Task<Strike> StrikeAsync(NewStrike strike, string idempotencyKey, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.StrikeAsync(access, strike, idempotencyKey, cancellation), cancellation);

    /// <summary>An admin's ban (CORE_API.md §5).</summary>
    public Task<BanApplied> BanAsync(NewBan ban, string idempotencyKey, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.BanAsync(access, ban, idempotencyKey, cancellation), cancellation);

    /// <summary>The accounts a gamertag or XUID names (MOD, ADM).</summary>
    public Task<AccountLookupList> LookupAccountsAsync(string xuidOrGamertag, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.LookupAccountsAsync(access, xuidOrGamertag, cancellation), cancellation);

    /// <summary>CR-S63: the account's Discord link and each live profile's announce switch.</summary>
    public Task<DiscordSettings> DiscordSettingsAsync(CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.DiscordSettingsAsync(access, cancellation), cancellation);

    /// <summary>A new one-time link code (the bot's page to open in the browser).</summary>
    public Task<DiscordLinkCode> CreateDiscordLinkAsync(CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.CreateDiscordLinkAsync(access, cancellation), cancellation);

    /// <summary>Clears the account's Discord link (204 also when there was none).</summary>
    public Task UnlinkDiscordAsync(CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(async access =>
        {
            await Api.UnlinkDiscordAsync(access, cancellation).ConfigureAwait(false);
            return true;
        }, cancellation);

    /// <summary>A profile's announce switch.</summary>
    public Task<ProfileAnnounce> SetAnnounceAsync(string xuid, bool announce, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.SetAnnounceAsync(access, xuid, announce, cancellation), cancellation);

    /// <summary>CR-L47: a profile's website switch; the server's answer replaces that profile in <see cref="Me"/>.</summary>
    public async Task<Profile> SetWebsiteListedAsync(string xuid, bool listed, string? game = null, CancellationToken cancellation = default)
    {
        var answer = await Tokens.WithAccessAsync(access => Api.SetWebsiteListedAsync(access, xuid, listed, CareerGame(game), cancellation), cancellation).ConfigureAwait(false);
        // CR-S98: a XUID names a career per game; the answer replaces only its own career's row
        if (Me is { } me) Me = me with { Profiles = [.. me.Profiles.Select(p => p.ProfileId == answer.ProfileId ? answer : p)] };
        return answer;
    }

    /// <summary>The session <see cref="SessionStore"/> kept from the last run; <see cref="RefreshMeAsync"/> proves it.</summary>
    public static AccountSession Resume(ApiClient api, ServerInfo info, Device device, SessionStore.Saved saved)
    {
        ArgumentNullException.ThrowIfNull(saved);
        return new AccountSession(api, TokenManager.Resume(api, saved.AccountId, saved.RefreshToken), device, info, saved.Email);
    }

    internal static AccountSession From(ApiClient api, SignedIn signedIn, Device device, ServerInfo info) =>
        new(api, TokenManager.From(api, signedIn), device, info, null);

    internal static AccountSession Created(ApiClient api, Device device, ServerInfo info, LauncherAccountCreated created, string? email) =>
        new(api, TokenManager.From(api, created), device, info, email);

    public void Dispose() => Tokens.Dispose();
}

/// <summary>A verified identity with no account yet: the identity ticket (15 minutes, single use) waits for the first profile's name.</summary>
public sealed class PendingAccount(ApiClient api, Device device, ServerInfo info, NeedsAccount needs)
{
    public string? Email => needs.Email;

    /// <summary><c>POST /launcher/v1/accounts</c> with the first profile; <c>ACCOUNTS.NAME_UNAVAILABLE</c> means: pick another name.</summary>
    public async Task<AccountSession> CreateAccountAsync(string game, string name, CancellationToken cancellation = default)
    {
        var created = await api.CreateAccountAsync(new LauncherAccountCreate(needs.IdentityTicket, device, new NewProfile(game, name, "noob")), cancellation).ConfigureAwait(false);
        return AccountSession.Created(api, device, info, created, needs.Email);
    }

    /// <summary>
    /// OD-120: the first profile of the account is a profile already on this PC (created signed out): <c>POST /launcher/v1/accounts</c>
    /// with its name and mode under its own upload key, so a retry replays instead of creating a second account. The caller links the
    /// local profile to the answer's <see cref="LauncherAccountCreated.Profile"/>.
    /// </summary>
    public async Task<(AccountSession Session, Profile Profile)> CreateAccountAsync(NewProfile first, string uploadKey, CancellationToken cancellation = default)
    {
        var created = await api.CreateAccountAsync(new LauncherAccountCreate(needs.IdentityTicket, device, first), uploadKey, cancellation).ConfigureAwait(false);
        return (AccountSession.Created(api, device, info, created, needs.Email), created.Profile);
    }
}
