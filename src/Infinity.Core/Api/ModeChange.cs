// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
﻿using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary><c>POST /launcher/v1/profiles/{xuid}/mode</c> (ACCOUNTS.md §5 "Mode change", OD-52): Hardcore → Noob only.</summary>
public sealed record ModeChange(string Mode, bool? Confirm);

/// <summary>
/// What a Hardcore → Noob change deletes for good (<c>ACCOUNTS.MODE_CHANGE_CONFIRM</c> <c>details</c>):
/// the newest cloud save revision, which must be on the PC first, and the counts per kind
/// (scores, ghosts, xp, drivatars, onlineProfile, inbox, saveRevisions; unknown kinds are kept as given).
/// A <c>liveries</c> count from an older server is dropped: a mode change keeps the liveries (CR-S59,
/// owner 2026-10-27 "a mode change keeps the liveries"; they follow the profile's current mode).
/// </summary>
public sealed record ModeChangeConfirm(ulong SaveRevision, IReadOnlyDictionary<string, long> Deletes)
{
    public const string Code = "ACCOUNTS.MODE_CHANGE_CONFIRM";

    /// <summary>Kinds a mode change no longer deletes, ignored when a server still counts them.</summary>
    public static IReadOnlySet<string> KeptKinds { get; } = new HashSet<string>(StringComparer.Ordinal) { "liveries" };

    /// <summary>The details of a <see cref="Code"/> refusal; null when they are not that shape.</summary>
    public static ModeChangeConfirm? From(JsonElement? details)
    {
        if (details is not { ValueKind: JsonValueKind.Object } d)
        {
            return null;
        }
        ulong revision = 0;
        if (d.TryGetProperty("saveRevision", out var r) && r.ValueKind == JsonValueKind.Number)
        {
            r.TryGetUInt64(out revision);
        }
        var deletes = new Dictionary<string, long>(StringComparer.Ordinal);
        if (d.TryGetProperty("deletes", out var counts) && counts.ValueKind == JsonValueKind.Object)
        {
            foreach (var kind in counts.EnumerateObject())
            {
                if (kind.Value.ValueKind == JsonValueKind.Number && kind.Value.TryGetInt64(out var n) && !KeptKinds.Contains(kind.Name))
                {
                    deletes[kind.Name] = n;
                }
            }
        }
        return new ModeChangeConfirm(revision, deletes);
    }
}

public sealed partial class ApiClient
{
    /// <summary>
    /// <c>POST /launcher/v1/profiles/{xuid}/mode</c>. Without <c>confirm</c> nothing is written and the
    /// answer is 409 <see cref="ModeChangeConfirm.Code"/>; with <c>confirm: true</c> the profile becomes
    /// Noob. <paramref name="idempotencyKey"/> is reused on every retry of the same step.
    /// </summary>
    public Task<Profile> ChangeModeAsync(string access, string xuid, ModeChange change, string idempotencyKey, string? game = null,
        CancellationToken cancellation = default) =>
        SendAsync<Profile>(HttpMethod.Post, Core, ProfilePath(xuid, "/mode", game), change, access, idempotencyKey, cancellation);

    /// <summary>
    /// <c>POST /launcher/v1/profiles/{xuid}/name</c> (OD-120, ACCOUNTS.md §5 "Plain names and renames"): only while the profile has a
    /// <c>renameNotice</c>; the current name keeps it, another free name takes it, and either clears the notice. CR-S98: it renames the
    /// profile and all its careers; <paramref name="game"/> picks the career the route names.
    /// </summary>
    public Task<Profile> RenameProfileAsync(string access, string xuid, NameChange change, string idempotencyKey, string? game = null,
        CancellationToken cancellation = default) =>
        SendAsync<Profile>(HttpMethod.Post, Core, ProfilePath(xuid, "/name", game), change, access, idempotencyKey, cancellation);

    /// <summary>
    /// <c>POST /launcher/v1/profiles/{xuid}/website</c> (CR-S83, CR-L47; ACCOUNTS.md §5) → 200 <see cref="Profile"/>: the caller's own
    /// profile on the play dashboard or off it. Idempotent (the current value answers 200 again), so no <c>Idempotency-Key</c>.
    /// </summary>
    public Task<Profile> SetWebsiteListedAsync(string access, string xuid, bool listed, string? game = null, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuid);
        return SendAsync<Profile>(HttpMethod.Post, Core, ProfilePath(xuid, "/website", game),
            new WebsiteVisibility(listed), access, idempotencyKey: null, cancellation);
    }

    /// <summary>
    /// <c>DELETE /launcher/v1/profiles/{xuid}</c> (CR-S61, ACCOUNTS.md §5 "Profile deletion"): no body → 204, the profile's cloud data
    /// erased at once and for good. <paramref name="idempotencyKey"/> is required and reused on every retry of the same deletion (a
    /// replay answers the 204 again). CR-S98: with <paramref name="game"/> it deletes that game's career (CAREERS.md §3 "delete
    /// career"); the profile goes with its last live career.
    /// </summary>
    public async Task DeleteProfileAsync(string access, string xuid, string idempotencyKey, string? game = null, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(idempotencyKey);
        await SendAsync<System.Text.Json.JsonElement?>(HttpMethod.Delete, Core, ProfilePath(xuid, "", game), null, access, idempotencyKey, cancellation)
            .ConfigureAwait(false);
    }

    /// <summary>
    /// CR-S98 <c>POST /launcher/v1/players/{xuid}/careers</c> <c>{game}</c> → 201 <see cref="Profile"/> (the new, empty career).
    /// <paramref name="idempotencyKey"/> is one per start of that (profile, game), reused by its retries. 409
    /// <c>ACCOUNTS.CAREER_EXISTS</c> when it exists; 400 <c>profile.mode</c> / <c>profile.game</c>; 403 <c>ACCOUNTS.BANNED</c>; 409
    /// <c>ACCOUNTS.DELETION_PENDING</c> (CAREERS.md §3, §6.12).
    /// </summary>
    public Task<Profile> CreateCareerAsync(string access, string xuid, CareerCreate career, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuid);
        ArgumentNullException.ThrowIfNull(career);
        ArgumentException.ThrowIfNullOrEmpty(idempotencyKey);
        return SendAsync<Profile>(HttpMethod.Post, Core, $"/launcher/v1/players/{Uri.EscapeDataString(xuid)}/careers", career, access, idempotencyKey,
            cancellation);
    }

    /// <summary>
    /// CR-S98 <c>DELETE /launcher/v1/players/{xuid}</c> → 204, "delete profile": every career, then the profile (CAREERS.md §3, §6.5).
    /// Same key, refusals and shared 3-an-hour limit as <see cref="DeleteProfileAsync"/>; refused whole while any career is in use.
    /// </summary>
    public async Task DeletePlayerAsync(string access, string xuid, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuid);
        ArgumentException.ThrowIfNullOrEmpty(idempotencyKey);
        await SendAsync<System.Text.Json.JsonElement?>(HttpMethod.Delete, Core, $"/launcher/v1/players/{Uri.EscapeDataString(xuid)}", null, access,
            idempotencyKey, cancellation).ConfigureAwait(false);
    }

    /// <summary>
    /// <c>/launcher/v1/profiles/{xuid}{suffix}</c>, with <c>?game=</c> when <paramref name="game"/> is set (CAREERS.md §3, §6.1). Null
    /// leaves it off: the bare route means the XUID's only career, what a server before CR-S98 understands.
    /// </summary>
    public static string ProfilePath(string xuid, string suffix, string? game) =>
        $"/launcher/v1/profiles/{Uri.EscapeDataString(xuid)}{suffix}" + GameQuery(game, first: true);

    /// <summary><c>?game=&lt;id&gt;</c> (or <c>&amp;game=</c> after another parameter); empty when <paramref name="game"/> is null.</summary>
    public static string GameQuery(string? game, bool first) =>
        string.IsNullOrEmpty(game) ? "" : (first ? "?" : "&") + "game=" + Uri.EscapeDataString(game);
}
