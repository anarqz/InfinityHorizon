// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Help;

public sealed record FaqSpan(string Text, bool Bold, bool Italic, bool Underline, bool Code);

/// <summary>Inert BBCode subset. Unsupported or mismatched tags remain literal text.</summary>
public static class FaqMarkup
{
    public static IReadOnlyList<FaqSpan> Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        var matched = new HashSet<int>();
        var openings = new Stack<(string Name, int Position)>();
        for (var position = 0; position < text.Length; position++)
        {
            if (text[position] != '[') continue;
            var end = text.IndexOf(']', position);
            if (end < 0) break;
            var token = text[(position + 1)..end].ToLowerInvariant();
            if (openings.Count > 0 && openings.Peek().Name == "code" && token != "/code") continue;
            if (token is "b" or "i" or "u" or "code") openings.Push((token, position));
            else if (token.StartsWith('/') && openings.Count > 0 && openings.Peek().Name == token[1..])
            {
                matched.Add(openings.Pop().Position);
                matched.Add(position);
            }
            position = end;
        }
        var spans = new List<FaqSpan>();
        var tags = new Stack<string>();
        var start = 0;
        void Add(string value)
        {
            if (value.Length > 0) spans.Add(new(value, tags.Contains("b"), tags.Contains("i"), tags.Contains("u"), tags.Contains("code")));
        }
        for (var i = 0; i < text.Length; i++)
        {
            if (text[i] != '[') continue;
            var end = text.IndexOf(']', i);
            if (end < 0) break;
            var tag = text[(i + 1)..end].ToLowerInvariant();
            var close = tag.StartsWith('/');
            var name = close ? tag[1..] : tag;
            if (tags.Contains("code") && tag != "/code") continue;
            if (tag != "br" && !matched.Contains(i)) continue;
            Add(text[start..i]);
            if (tag == "br") Add("\n");
            else if (close) tags.Pop();
            else tags.Push(name);
            i = end;
            start = end + 1;
        }
        Add(text[start..]);
        return spans.AsReadOnly();
    }
}
