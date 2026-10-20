// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>The name part of a gamertag as the server accepts it: 1-20 ASCII letters or digits, starting with a letter (ACCOUNTS.md §2).</summary>
public static class ProfileName
{
    public const int MaxLength = 20;

    /// <summary>Checked before sending so the player sees the rule at once; the server still decides (<c>ACCOUNTS.INVALID_NAME</c>).</summary>
    public static bool IsValid(string? name) =>
        name is { Length: > 0 and <= MaxLength } && char.IsAsciiLetter(name[0]) && name.All(char.IsAsciiLetterOrDigit);
}
