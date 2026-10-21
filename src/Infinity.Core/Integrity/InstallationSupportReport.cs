// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Integrity;

/// <summary>Explicit-copy support projection: relative findings, never local roots, identities or memory.</summary>
public static class InstallationSupportReport
{
    public const int MaxCharacters = 24 * 1024;

    public static string Format(InstallationState state, string reason, IReadOnlyList<InstallationFinding> findings)
    {
        ArgumentNullException.ThrowIfNull(findings);
        var text = new StringBuilder("Infinity installation check v1\n");
        text.Append("State: ").Append(state).Append('\n');
        text.Append("Reason: ").Append(Code(reason)).Append('\n');
        text.Append("This local check does not establish intent or identify an offender.\n");
        foreach (var finding in findings.Take(CriticalFileReader.MaxFiles))
        {
            var root = finding.Root is "azimuth" or "game_content" or "infinity_package" ? finding.Root : "unknown";
            var path = CriticalFileReader.RelativePath(finding.Path) ? Clean(finding.Path, 240) : "[location omitted]";
            var line = new StringBuilder().Append(root).Append('/').Append(path).Append(": ").Append(Code(finding.Reason));
            if (finding.ByteLength is { } length && length >= 0) line.Append(" bytes=").Append(length);
            if (CriticalFileReader.Digest(finding.Sha256)) line.Append(" sha256=").Append(finding.Sha256);
            line.Append('\n');
            if (text.Length + line.Length > MaxCharacters)
            {
                const string omitted = "Additional findings omitted.\n";
                if (text.Length > MaxCharacters - omitted.Length) text.Length = MaxCharacters - omitted.Length;
                text.Append(omitted); break;
            }
            text.Append(line);
        }
        return text.ToString();
    }

    private static string Code(string? value) => value is { Length: > 0 and <= 80 } && value.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '-' or '.') ? value : "unknown";
    private static string Clean(string value, int length) => new(value.Take(length).Select(c => char.IsControl(c) || c is '\u2028' or '\u2029' or '\u202a' or '\u202b' or '\u202c' or '\u202d' or '\u202e' or '\u2066' or '\u2067' or '\u2068' or '\u2069' ? '?' : c).ToArray());
}
