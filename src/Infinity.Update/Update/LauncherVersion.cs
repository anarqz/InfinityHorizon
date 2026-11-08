// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics.CodeAnalysis;

namespace Infinity.Core.Update;

/// <summary>Bounded SemVer 2 precedence; informational build metadata never changes ordering.</summary>
public sealed class LauncherVersion : IComparable<LauncherVersion>, IEquatable<LauncherVersion>
{
    public const int MaxLength = 64;
    private readonly string[] _core, _pre;
    private readonly string _precedence;
    public string Value { get; }
    public string? Prerelease => _pre.Length == 0 ? null : string.Join('.', _pre);

    private LauncherVersion(string value, string[] core, string[] pre)
    {
        Value = value; _core = core; _pre = pre;
        _precedence = string.Join('.', core) + (pre.Length == 0 ? "" : "-" + string.Join('.', pre));
    }

    public static bool TryParse(string? value, [NotNullWhen(true)] out LauncherVersion? version)
    {
        version = null;
        if (value is not { Length: > 0 and <= MaxLength }) return false;
        var plus = value.IndexOf('+');
        var main = plus < 0 ? value : value[..plus];
        if (plus >= 0 && !Identifiers(value[(plus + 1)..], numericLeadingZeros: true)) return false;
        var dash = main.IndexOf('-');
        var core = (dash < 0 ? main : main[..dash]).Split('.');
        if (core.Length != 3 || core.Any(number => !Numeric(number) || number.Length > 1 && number[0] == '0'
            || Number(number, "18446744073709551615") > 0)) return false;
        var pre = dash < 0 ? [] : main[(dash + 1)..].Split('.');
        if (dash >= 0 && !Identifiers(main[(dash + 1)..], numericLeadingZeros: false)) return false;
        version = new(value, core, pre);
        return true;
    }

    private static bool Identifiers(string value, bool numericLeadingZeros) => value.Split('.').All(identifier => identifier.Length > 0
        && identifier.All(c => char.IsAsciiLetterOrDigit(c) || c == '-')
        && (numericLeadingZeros || !Numeric(identifier) || identifier.Length == 1 || identifier[0] != '0'));
    private static bool Numeric(string value) => value.Length > 0 && value.All(char.IsAsciiDigit);
    private static int Number(string left, string right) => left.Length != right.Length
        ? left.Length.CompareTo(right.Length) : string.Compare(left, right, StringComparison.Ordinal);

    public int CompareTo(LauncherVersion? other)
    {
        if (other is null) return 1;
        for (var i = 0; i < _core.Length; i++)
        {
            var order = Number(_core[i], other._core[i]);
            if (order != 0) return order;
        }
        if (_pre.Length == 0 || other._pre.Length == 0)
            return _pre.Length == other._pre.Length ? 0 : _pre.Length == 0 ? 1 : -1;
        for (var i = 0; i < Math.Min(_pre.Length, other._pre.Length); i++)
        {
            var leftNumeric = Numeric(_pre[i]); var rightNumeric = Numeric(other._pre[i]);
            var order = leftNumeric && rightNumeric ? Number(_pre[i], other._pre[i])
                : leftNumeric != rightNumeric ? leftNumeric ? -1 : 1 : string.Compare(_pre[i], other._pre[i], StringComparison.Ordinal);
            if (order != 0) return order;
        }
        return _pre.Length.CompareTo(other._pre.Length);
    }

    public bool Equals(LauncherVersion? other) => other is not null && CompareTo(other) == 0;
    public static bool operator ==(LauncherVersion? left, LauncherVersion? right) => object.Equals(left, right);
    public static bool operator !=(LauncherVersion? left, LauncherVersion? right) => !object.Equals(left, right);
    public static bool operator <(LauncherVersion? left, LauncherVersion? right) => Compare(left, right) < 0;
    public static bool operator <=(LauncherVersion? left, LauncherVersion? right) => Compare(left, right) <= 0;
    public static bool operator >(LauncherVersion? left, LauncherVersion? right) => Compare(left, right) > 0;
    public static bool operator >=(LauncherVersion? left, LauncherVersion? right) => Compare(left, right) >= 0;
    private static int Compare(LauncherVersion? left, LauncherVersion? right) => left is null ? right is null ? 0 : -1 : left.CompareTo(right);
    public override bool Equals(object? obj) => obj is LauncherVersion other && Equals(other);
    public override int GetHashCode() => StringComparer.Ordinal.GetHashCode(_precedence);
    public override string ToString() => Value;
}
