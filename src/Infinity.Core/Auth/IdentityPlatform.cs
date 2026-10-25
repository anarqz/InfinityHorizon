// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net.Http.Json;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>What an Identity Platform refusal means for the player (ACCOUNTS.md §5, native sign-in).</summary>
public enum IdpProblem
{
    /// <summary>INVALID_LOGIN_CREDENTIALS, EMAIL_NOT_FOUND, INVALID_PASSWORD: one message, never which one.</summary>
    Credentials,
    TooManyAttempts,
    Disabled,
    RegistrationDisabled,
    EmailExists,
    WeakPassword,
    InvalidEmail,
    Other,
}

/// <summary>An Identity Platform error answer; <see cref="Code"/> is its <c>error.message</c> head (never the password).</summary>
public sealed class IdentityPlatformException(IdpProblem problem, string code) : Exception(code)
{
    public IdpProblem Problem { get; } = problem;

    public string Code { get; } = code;
}

/// <summary>
/// The launcher's own e-mail/password sign-in (OD-43, ACCOUNTS.md §4.1 and §5): Identity Platform's
/// public REST API, called directly. The password goes only there, never to our services, a log
/// or a file. No Identity Platform refresh token is kept: the ID token is exchanged right away
/// (<c>grantType: id_token</c>) and dropped.
/// <para>
/// A <c>local</c> instance (CR-L37, development builds only until M5) is its own identity provider: the password goes to its
/// <c>/launcher/v1/auth/sign-up</c> and <c>/auth/sign-in</c> instead, the opaque ID token they answer is exchanged the same way,
/// and it sends no mail (no verification, no reset: the server's operator resets a password).
/// </para>
/// </summary>
public sealed class IdentityPlatform
{
    private const string Production = "https://identitytoolkit.googleapis.com/v1";
    private readonly HttpClient? _http;
    private readonly ApiClient? _local;
    private readonly string _base;
    private readonly string _key = string.Empty;
    private readonly string _locale = "en";

    private IdentityPlatform(ApiClient local)
    {
        _local = local;
        _base = new Uri(local.Core, "/launcher/v1/auth").ToString();
    }

    /// <summary>A <c>local</c> instance's built-in provider, on the core origin of <paramref name="api"/>.</summary>
    public static IdentityPlatform Local(ApiClient api) =>
        BuildProfile.ProductionOnly
            ? throw new InvalidOperationException("This build signs in with Identity Platform only")
            : new(api ?? throw new ArgumentNullException(nameof(api)));

    /// <summary>The provider <paramref name="info"/>'s <c>idp</c> names: <c>local</c>, or Identity Platform (<c>kind</c> absent or <c>firebase</c>).</summary>
    public static IdentityPlatform For(HttpClient http, ServerInfo info, ApiClient api, string locale)
    {
        ArgumentNullException.ThrowIfNull(info);
        if (info.Idp.IsLocal) return Local(api);
        return new(http, info.Idp.ApiKey ?? throw new InvalidDataException("idp.apiKey is missing"), info.Idp.EmulatorUrl, locale);
    }

    /// <summary>A <c>local</c> instance: no verification or reset mail, and the password can be changed in the launcher.</summary>
    public bool IsLocal => _local is not null;

    /// <param name="emulatorUrl"><c>idp.emulatorUrl</c> from <c>/launcher/v1/info</c> (local stacks only): plain http only on loopback or a private LAN address.</param>
    /// <param name="locale"><c>en</c> or <c>pt-BR</c>, for the e-mails Identity Platform sends.</param>
    public IdentityPlatform(HttpClient http, string apiKey, string? emulatorUrl, string locale)
    {
        _http = http ?? throw new ArgumentNullException(nameof(http));
        _key = Uri.EscapeDataString(apiKey ?? throw new ArgumentNullException(nameof(apiKey)));
        _locale = locale == "pt-BR" ? "pt-BR" : "en";
        if (string.IsNullOrEmpty(emulatorUrl))
        {
            _base = Production;
        }
        else
        {
            var origin = new Uri(emulatorUrl).GetLeftPart(UriPartial.Authority);
            if (!DevNet.PlainHttpUrl(origin) && !origin.StartsWith("https://", StringComparison.Ordinal))
            {
                throw new ArgumentException($"{origin} is not https, loopback or a private LAN address", nameof(emulatorUrl));
            }
            _base = origin + "/identitytoolkit.googleapis.com/v1";
        }
    }

    /// <summary>The REST base in use (for tests and the log; it holds no secret).</summary>
    public string Base => _base;

    /// <summary><c>accounts:signInWithPassword</c> (<c>local</c>: <c>/auth/sign-in</c>): the ID token (5 minutes to exchange it).</summary>
    public async Task<string> SignInAsync(string email, string password, CancellationToken cancellation = default)
    {
        if (_local is { } local)
            return Opaque(await Local(() => local.LocalSignInAsync(new LocalCredentials(email, password), cancellation)).ConfigureAwait(false));
        return IdToken(await PostAsync("accounts:signInWithPassword", new { email, password, returnSecureToken = true }, cancellation).ConfigureAwait(false));
    }

    /// <summary>
    /// <c>accounts:signUp</c>, then the verification e-mail; NativeSignInFlow continues after confirmation without retyping (OD-74),
    /// so the answer is null. <c>local</c>: <c>/auth/sign-up</c>, whose ID token is the answer (no e-mail to confirm).
    /// </summary>
    public async Task<string?> SignUpAsync(string email, string password, CancellationToken cancellation = default)
    {
        if (_local is { } local)
        {
            var key = Guid.CreateVersion7().ToString();
            return Opaque(await Local(() => local.LocalSignUpAsync(new LocalCredentials(email, password), key, cancellation)).ConfigureAwait(false));
        }
        var idToken = IdToken(await PostAsync("accounts:signUp", new { email, password, returnSecureToken = true }, cancellation).ConfigureAwait(false));
        await SendVerificationAsync(idToken, cancellation).ConfigureAwait(false);
        return null;
    }

    /// <summary><c>local</c> only: a fresh sign-in with the current password, then <c>/auth/password</c>; every refresh family of the account ends.</summary>
    public async Task ChangePasswordAsync(string email, string password, string newPassword, CancellationToken cancellation = default)
    {
        var local = _local ?? throw new InvalidOperationException("Identity Platform changes a password by e-mail");
        var idToken = await SignInAsync(email, password, cancellation).ConfigureAwait(false);
        await Local(async () => { await local.LocalChangePasswordAsync(new LocalPasswordChange(idToken, newPassword), cancellation).ConfigureAwait(false); return true; }).ConfigureAwait(false);
    }

    /// <summary><c>accounts:sendOobCode</c> VERIFY_EMAIL ("send the confirmation e-mail again", <c>AUTH.EMAIL_UNVERIFIED</c>).</summary>
    public Task SendVerificationAsync(string idToken, CancellationToken cancellation = default) =>
        _local is not null
            ? throw new InvalidOperationException("A local instance sends no e-mail")
            : PostAsync("accounts:sendOobCode", new { requestType = "VERIFY_EMAIL", idToken }, cancellation);

    /// <summary>
    /// <c>accounts:sendOobCode</c> PASSWORD_RESET. Every Identity Platform answer, EMAIL_NOT_FOUND
    /// included, reads as sent (the screen never tells whether an e-mail has an account); only a
    /// network failure throws.
    /// </summary>
    public async Task SendPasswordResetAsync(string email, CancellationToken cancellation = default)
    {
        if (_local is not null) throw new InvalidOperationException("A local instance sends no e-mail");
        try
        {
            await PostAsync("accounts:sendOobCode", new { requestType = "PASSWORD_RESET", email }, cancellation).ConfigureAwait(false);
        }
        catch (IdentityPlatformException)
        {
            // The same confirmation whatever the answer.
        }
    }

    private static string IdToken(JsonElement body) =>
        body.TryGetProperty("idToken", out var token) && token.GetString() is { Length: > 0 } text
            ? text
            : throw new IdentityPlatformException(IdpProblem.Other, "NO_ID_TOKEN");

    private static string Opaque(LocalIdToken? answer) =>
        answer?.IdToken is { Length: > 0 } text ? text : throw new IdentityPlatformException(IdpProblem.Other, "NO_ID_TOKEN");

    /// <summary>
    /// A <c>local</c> refusal the sign-in screen shows like Identity Platform's (ACCOUNTS.md §9); every other one (rate limits,
    /// registration closed, an online pause) stays an <see cref="ApiException"/> for the screen's existing server branches.
    /// </summary>
    private static async Task<T> Local<T>(Func<Task<T>> call)
    {
        try
        {
            return await call().ConfigureAwait(false);
        }
        catch (ApiException e) when (Problem(e) is { } problem)
        {
            throw new IdentityPlatformException(problem, e.Code);
        }
    }

    public static IdpProblem? Problem(ApiException e)
    {
        ArgumentNullException.ThrowIfNull(e);
        return e.Code switch
        {
            ErrorCodes.AuthInvalidCredentials => IdpProblem.Credentials,
            ErrorCodes.AccountsEmailTaken => IdpProblem.EmailExists,
            ErrorCodes.RequestInvalid => Field(e) switch
            {
                "email" => IdpProblem.InvalidEmail,
                "password" or "newPassword" => IdpProblem.WeakPassword,
                _ => null,
            },
            _ => null,
        };
    }

    private static string? Field(ApiException e) =>
        e.Details is { ValueKind: JsonValueKind.Object } details && details.TryGetProperty("field", out var field) && field.ValueKind == JsonValueKind.String
            ? field.GetString()
            : null;

    private async Task<JsonElement> PostAsync(string method, object body, CancellationToken cancellation)
    {
        // A string URL: "accounts:signUp" as a relative Uri would read "accounts:" as a scheme.
        using var request = new HttpRequestMessage(HttpMethod.Post, $"{_base}/{method}?key={_key}");
        request.Headers.TryAddWithoutValidation("X-Firebase-Locale", _locale);
        request.Content = JsonContent.Create(body);
        using var response = await _http!.SendAsync(request, cancellation).ConfigureAwait(false);
        var text = await response.Content.ReadAsStringAsync(cancellation).ConfigureAwait(false);
        JsonElement json = default;
        try
        {
            json = JsonSerializer.Deserialize<JsonElement>(text);
        }
        catch (JsonException)
        {
            // Not Identity Platform's body: classified below.
        }
        if (response.IsSuccessStatusCode && json.ValueKind == JsonValueKind.Object)
        {
            return json;
        }
        var message = json.ValueKind == JsonValueKind.Object && json.TryGetProperty("error", out var error) &&
                      error.TryGetProperty("message", out var m) && m.GetString() is { } s
            ? s
            : $"HTTP.{(int)response.StatusCode}";
        // "WEAK_PASSWORD : Password should be at least 6 characters": the code is the head.
        var code = message.Split(' ', 2)[0].Trim();
        throw new IdentityPlatformException(Classify(code), code);
    }

    public static IdpProblem Classify(string code) => code switch
    {
        "INVALID_LOGIN_CREDENTIALS" or "EMAIL_NOT_FOUND" or "INVALID_PASSWORD" => IdpProblem.Credentials,
        "TOO_MANY_ATTEMPTS_TRY_LATER" => IdpProblem.TooManyAttempts,
        "USER_DISABLED" => IdpProblem.Disabled,
        "ADMIN_ONLY_OPERATION" => IdpProblem.RegistrationDisabled,
        "EMAIL_EXISTS" => IdpProblem.EmailExists,
        "WEAK_PASSWORD" => IdpProblem.WeakPassword,
        "INVALID_EMAIL" or "MISSING_EMAIL" => IdpProblem.InvalidEmail,
        _ => IdpProblem.Other,
    };
}
