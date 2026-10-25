// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// A gamertag as the server accepts it: 1-20 ASCII letters or digits, starting with a letter (ACCOUNTS.md §3). Since OD-120 the
/// gamertag is that plain name; a server or cache from before it may still carry the retired <c>Name#12345</c>.
/// </summary>
public static class ProfileName
{
    public const int MaxLength = 20;

    /// <summary>Checked before sending so the player sees the rule at once; the server still decides (<c>ACCOUNTS.INVALID_NAME</c>).</summary>
    public static bool IsValid(string? name) =>
        name is { Length: > 0 and <= MaxLength } && char.IsAsciiLetter(name[0]) && name.All(char.IsAsciiLetterOrDigit);

    /// <summary>
    /// The name a player sees (OD-120: never the <c>#NNNNN</c> number): a legacy <c>Name#12345</c> without its number, a plain name
    /// as it is. Display only; what is sent back to the server (friend requests, the deletion confirmation) stays the server's text.
    /// </summary>
    public static string Plain(string? gamertag)
    {
        if (string.IsNullOrEmpty(gamertag)) return string.Empty;
        var hash = gamertag.IndexOf('#', StringComparison.Ordinal);
        return hash > 0 ? gamertag[..hash] : gamertag;
    }

    /// <summary>
    /// The name a profile plays under offline (FI_OFFLINE_GAMERTAG): the plain name, kept to the rule the hook checks (1-20 letters
    /// or digits, a letter first); "Player" when nothing of it is left.
    /// </summary>
    public static string Playable(string? gamertag)
    {
        var letters = new string(Plain(gamertag).Where(char.IsAsciiLetterOrDigit).Take(MaxLength).ToArray());
        var name = letters.TrimStart("0123456789".ToCharArray());
        return IsValid(name) ? name : "Player";
    }
}
