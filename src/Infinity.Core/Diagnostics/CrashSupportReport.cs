// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;

namespace Infinity.Core.Diagnostics;

/// <summary>Explicit clipboard/file export. A projection, never a serialization of private local context.</summary>
public static class CrashSupportReport
{
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web) { WriteIndented = true };
    public static string Create(CrashIncident incident, string? serverReportId = null)
    {
        ArgumentNullException.ThrowIfNull(incident);
        static string? Id(string? id) => Guid.TryParseExact(id, "D", out var parsed) && parsed != Guid.Empty ? id : null;
        return JsonSerializer.Serialize(new
        {
            format = "infinity-crash-support-v1",
            localIncidentId = Id(incident.Id),
            reportId = Id(serverReportId),
            occurredAt = incident.OccurredAt.UtcDateTime.ToString(CrashSubmission.InstantFormat, CultureInfo.InvariantCulture),
            build = CrashSubmission.Build(incident),
            crash = CrashSubmission.Describe(incident),
            localDumpAvailable = incident.Dump is not null,
            worldContextAvailable = incident.Context.World is not null,
            observedParticipantCount = Math.Min(incident.Context.World?.Participants.Count ?? 0, 12),
            attribution = "unknown",
        }, Json);
    }
}

/// <summary>Owner-configured Discord channel. Its actual private permissions must be set by moderators.</summary>
public static class PrivateSupportLink
{
    public static Uri? Parse(string? configured)
    {
        if (!Uri.TryCreate(configured, UriKind.Absolute, out var uri) || uri.Scheme != "https" || uri.Host is not ("discord.com" or "discord.gg")
            || !uri.IsDefaultPort || uri.UserInfo.Length != 0 || uri.Query.Length != 0 || uri.Fragment.Length != 0) return null;
        var parts = uri.AbsolutePath.Split('/', StringSplitOptions.RemoveEmptyEntries);
        if (uri.Host == "discord.gg") return parts.Length == 1 && parts[0].Length is >= 2 and <= 64
            && parts[0].All(c => char.IsAsciiLetterOrDigit(c) || c == '-') ? uri : null;
        return parts.Length == 3 && parts[0] == "channels" && Snowflake(parts[1]) && Snowflake(parts[2]) ? uri : null;
    }

    private static bool Snowflake(string value) => value.Length is >= 1 and <= 20 && value.All(char.IsAsciiDigit)
        && ulong.TryParse(value, out var id) && id != 0;
}
