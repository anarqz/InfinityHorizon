// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Mods;

/// <summary>
/// FIMOD004 data-mod SQL as the launcher checks it before install and launch (the runtime's authorizer is the real boundary):
/// UTF-8 without BOM, statements separated by <c>;</c>, at most 8 MiB, 50,000 statements and 64 KiB per statement, each an
/// INSERT or UPDATE. A <c>;</c> inside a quoted literal or identifier does not end a statement; comments are refused.
/// </summary>
public static class DataModSql
{
    public const int MaxBytes = 8 * 1024 * 1024;
    public const int MaxStatements = 50_000;
    public const int MaxStatementBytes = 64 * 1024;

    /// <summary>The statements (trimmed), or <see cref="ModException"/> (<see cref="ModErrors.Unsupported"/>).</summary>
    public static IReadOnlyList<string> Statements(byte[] bytes)
    {
        ArgumentNullException.ThrowIfNull(bytes);
        if (bytes.Length is < 1 or > MaxBytes || bytes.AsSpan().StartsWith(new byte[] { 0xef, 0xbb, 0xbf })) throw Invalid("size or byte order mark");
        string text;
        try { text = new UTF8Encoding(false, throwOnInvalidBytes: true).GetString(bytes); }
        catch (DecoderFallbackException) { throw Invalid("not UTF-8"); }
        if (text.Contains('\0', StringComparison.Ordinal)) throw Invalid("NUL");
        var statements = new List<string>();
        var start = 0; char? quote = null;
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (quote is { } open) { if (c == open) quote = null; continue; } // a doubled quote reopens at once: still inside
            if (c is '\'' or '"' or '`') { quote = c; continue; }
            if (c == '[') { var close = text.IndexOf(']', i); if (close < 0) throw Invalid("unterminated identifier"); i = close; continue; }
            if (c == '-' && i + 1 < text.Length && text[i + 1] == '-' || c == '/' && i + 1 < text.Length && text[i + 1] == '*') throw Invalid("comment");
            if (c != ';') continue;
            Add(statements, text[start..i]); start = i + 1;
        }
        if (quote is not null) throw Invalid("unterminated literal");
        if (text[start..].Trim().Length > 0) Add(statements, text[start..]);
        if (statements.Count == 0) throw Invalid("no statements");
        return statements;
    }

    private static void Add(List<string> statements, string raw)
    {
        var statement = raw.Trim();
        if (statement.Length == 0) return;
        if (statements.Count >= MaxStatements) throw Invalid("too many statements");
        if (Encoding.UTF8.GetByteCount(statement) > MaxStatementBytes) throw Invalid("statement too long");
        if (!(statement.StartsWith("INSERT", StringComparison.OrdinalIgnoreCase) || statement.StartsWith("UPDATE", StringComparison.OrdinalIgnoreCase))
            || statement.Length > 6 && !char.IsWhiteSpace(statement[6])) throw Invalid("only INSERT and UPDATE");
        statements.Add(statement);
    }

    private static ModException Invalid(string reason) => new("invalid data mod SQL: " + reason, ModErrors.Unsupported);
}
