// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
﻿using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary><c>POST /launcher/v1/profiles/{xuid}/mode</c> (ACCOUNTS.md §5 "Mode change", OD-52): Hardcore → Noob only.</summary>
public sealed record ModeChange(string Mode, bool? Confirm);

/// <summary>
/// What a Hardcore → Noob change deletes for good (<c>ACCOUNTS.MODE_CHANGE_CONFIRM</c> <c>details</c>):
/// the newest cloud save revision, which must be on the PC first, and the counts per kind
/// (scores, ghosts, xp, drivatars, onlineProfile, inbox, saveRevisions; unknown kinds are kept as given).
/// </summary>
public sealed record ModeChangeConfirm(ulong SaveRevision, IReadOnlyDictionary<string, long> Deletes)
{
    public const string Code = "ACCOUNTS.MODE_CHANGE_CONFIRM";

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
                if (kind.Value.ValueKind == JsonValueKind.Number && kind.Value.TryGetInt64(out var n))
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
    public Task<Profile> ChangeModeAsync(string access, string xuid, ModeChange change, string idempotencyKey, CancellationToken cancellation = default) =>
        SendAsync<Profile>(HttpMethod.Post, Core, $"/launcher/v1/profiles/{Uri.EscapeDataString(xuid)}/mode", change, access, idempotencyKey, cancellation);

    /// <summary>
    /// <c>POST /launcher/v1/profiles/{xuid}/name</c> (OD-120, ACCOUNTS.md §5 "Plain names and renames"): only while the profile has a
    /// <c>renameNotice</c>; the current name keeps it, another free name takes it, and either clears the notice.
    /// </summary>
    public Task<Profile> RenameProfileAsync(string access, string xuid, NameChange change, string idempotencyKey, CancellationToken cancellation = default) =>
        SendAsync<Profile>(HttpMethod.Post, Core, $"/launcher/v1/profiles/{Uri.EscapeDataString(xuid)}/name", change, access, idempotencyKey, cancellation);
}
