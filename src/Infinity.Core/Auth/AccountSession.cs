// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>Where the launcher talks to: the server (core and gate origins come from its <c>/info</c>) and the website used for account self-service.</summary>
public sealed record ServerConfig(Uri Server, Uri Portal)
{
    /// <summary>Local development defaults; --server/--portal or FI_SERVER/FI_PORTAL select a deployment.</summary>
    public static ServerConfig Default { get; } = new(new Uri("http://127.0.0.1:8480"), new Uri("http://127.0.0.1:3120"));

    /// <summary><c>--server &lt;url&gt;</c> / <c>--portal &lt;url&gt;</c>, then <c>FI_SERVER</c> / <c>FI_PORTAL</c>, then <see cref="Default"/>.</summary>
    public static ServerConfig From(IReadOnlyList<string> args, Func<string, string?> env)
    {
        ArgumentNullException.ThrowIfNull(args);
        ArgumentNullException.ThrowIfNull(env);
        string? Arg(string name)
        {
            var at = args.ToList().IndexOf(name);
            return at >= 0 && at + 1 < args.Count ? args[at + 1] : null;
        }
        var server = Arg("--server") ?? env("FI_SERVER");
        var portal = Arg("--portal") ?? env("FI_PORTAL");
        return new ServerConfig(
            ApiClient.CheckOrigin(server is null ? Default.Server : new Uri(server)),
            ApiClient.CheckOrigin(portal is null ? Default.Portal : new Uri(portal)));
    }
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
        var info = await ApiClient.InfoAsync(http, config.Server, clientHeader, cancellation).ConfigureAwait(false);
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
    /// Creates a profile on the server: <c>noob</c>, or <c>hardcore</c> (an Infinity Supporter's, OD-52),
    /// which starts empty: the game seeds it on the first launch and that career is its first cloud
    /// revision (OD-60; no starter car).
    /// </summary>
    public Task<Profile> CreateProfileAsync(string game, string name, string mode = "noob", CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.CreateProfileAsync(access, new NewProfile(game, name, mode), cancellation), cancellation);

    /// <summary>
    /// Hardcore → Noob, step 1 (OD-52): what the change would delete. Nothing is written; null when
    /// the profile is already Noob (the server answers 200).
    /// </summary>
    public async Task<ModeChangeConfirm?> AskNoobAsync(string xuid, CancellationToken cancellation = default)
    {
        try
        {
            await Tokens.WithAccessAsync(access => Api.ChangeModeAsync(access, xuid, new ModeChange("noob", null), Guid.CreateVersion7().ToString(), cancellation), cancellation).ConfigureAwait(false);
            return null;
        }
        catch (ApiException e) when (e.Code == ModeChangeConfirm.Code)
        {
            return ModeChangeConfirm.From(e.Details) ?? new ModeChangeConfirm(0, new Dictionary<string, long>());
        }
    }

    /// <summary>Hardcore → Noob, step 2: confirmed. The profile's cloud data is deleted for good; friends stay.</summary>
    public Task<Profile> ConfirmNoobAsync(string xuid, string idempotencyKey, CancellationToken cancellation = default) =>
        Tokens.WithAccessAsync(access => Api.ChangeModeAsync(access, xuid, new ModeChange("noob", true), idempotencyKey, cancellation), cancellation);

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
}
