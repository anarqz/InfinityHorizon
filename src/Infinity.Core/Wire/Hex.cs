// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Wire;

/// <summary>Lowercase hex, as fi-launcher-core <c>fsx::hex</c> / <c>unhex</c>.</summary>
public static class Hex
{
    public static string Encode(ReadOnlySpan<byte> bytes) => Convert.ToHexStringLower(bytes);

    /// <summary>Decodes an even-length hex string (either case); null when it is not one.</summary>
    public static byte[]? Decode(string text)
    {
        if (text.Length % 2 != 0)
        {
            return null;
        }
        foreach (var c in text)
        {
            if (!char.IsAsciiHexDigit(c))
            {
                return null;
            }
        }
        return Convert.FromHexString(text);
    }
}
