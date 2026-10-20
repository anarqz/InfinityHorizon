// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core;

/// <summary>
/// A bearer value (session token, refresh token). <see cref="ToString"/> never shows it, so a
/// secret that reaches a log or an exception message stays redacted (fi-launcher-core
/// <c>credentials::Secret</c>).
/// </summary>
public sealed class Secret : IEquatable<Secret>
{
    private readonly string _value;

    public Secret(string value) => _value = value ?? throw new ArgumentNullException(nameof(value));

    /// <summary>The raw value, for the one place that must send it.</summary>
    public string Expose() => _value;

    public override string ToString() => "<redacted>";

    public bool Equals(Secret? other) => other is not null && string.Equals(_value, other._value, StringComparison.Ordinal);

    public override bool Equals(object? obj) => Equals(obj as Secret);

    public override int GetHashCode() => StringComparer.Ordinal.GetHashCode(_value);
}
