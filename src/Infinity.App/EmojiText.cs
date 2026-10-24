// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.IO.Compression;
using System.Text;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Data;
using System.Windows.Documents;
using System.Windows.Media;

namespace Infinity.App;

/// <summary>
/// OD-119 (T3c): emoji in news text, drawn as vector shapes from Noto Emoji's monochrome outlines (OFL-1.1), pre-extracted at build
/// time into Assets/Emoji/noto-emoji-paths.txt.gz (tools/emoji). No font fallback and no font parsing at runtime: under Wine, WPF
/// left emoji blank through any font route. Everything that is not an emoji stays text in the launcher's Selawik.
/// </summary>
public static class EmojiText
{
    private sealed record Glyph(double Advance, Geometry Shape);
    private sealed record Table(double Units, double Ascent, IReadOnlyDictionary<int, Glyph> Glyphs);

    private static readonly Lazy<Table> Data = new(Load);

    private static Table Load()
    {
        var glyphs = new Dictionary<int, Glyph>();
        double units = 512, ascent = 475;
        try
        {
            var resource = Application.GetResourceStream(new Uri("pack://application:,,,/Assets/Emoji/noto-emoji-paths.txt.gz"));
            if (resource is null) return new(units, ascent, glyphs);
            using var reader = new StreamReader(new GZipStream(resource.Stream, CompressionMode.Decompress), Encoding.UTF8);
            var header = reader.ReadLine()?.Split(' ');
            if (header is ["units", var u, "ascent", var a, ..])
            {
                units = double.Parse(u, CultureInfo.InvariantCulture); ascent = double.Parse(a, CultureInfo.InvariantCulture);
            }
            while (reader.ReadLine() is { } line)
            {
                var parts = line.Split('\t');
                if (parts.Length != 3 || !int.TryParse(parts[0], NumberStyles.HexNumber, CultureInfo.InvariantCulture, out var codePoint)) continue;
                try
                {
                    var shape = Geometry.Parse("F1 " + parts[2]); // NonZero, as font outlines are drawn (the parser's default is EvenOdd)
                    shape.Freeze();
                    glyphs[codePoint] = new(double.Parse(parts[1], CultureInfo.InvariantCulture), shape);
                }
                catch (FormatException) { }
            }
        }
        catch (Exception error) when (error is IOException or InvalidDataException or UnauthorizedAccessException or InvalidOperationException) { }
        return new(units, ascent, glyphs);
    }

    /// <summary>Parts of an emoji sequence that draw nothing here: variation selectors, the joiner, skin tones, keycap and tags.</summary>
    private static bool Invisible(int codePoint) => codePoint is 0xFE0E or 0xFE0F or 0x200D or 0x20E3 or (>= 0x1F3FB and <= 0x1F3FF) or (>= 0xE0020 and <= 0xE007F);

    /// <summary>The text as inlines: runs of text, and each emoji as a shape the size of <paramref name="fontSize"/>, in the text's colour.</summary>
    public static IEnumerable<Inline> Inlines(string text, double fontSize)
    {
        var table = Data.Value;
        var buffer = new StringBuilder();
        foreach (var rune in text.EnumerateRunes())
        {
            if (Invisible(rune.Value)) continue;
            if (!table.Glyphs.TryGetValue(rune.Value, out var glyph)) { buffer.Append(rune.ToString()); continue; }
            if (buffer.Length > 0) { yield return new Run(buffer.ToString()); buffer.Clear(); }
            yield return Shape(glyph, table, fontSize);
        }
        if (buffer.Length > 0) yield return new Run(buffer.ToString());
    }

    private static InlineUIContainer Shape(Glyph glyph, Table table, double fontSize)
    {
        var scale = fontSize / table.Units;
        // The box covers the part above the baseline (so the line keeps its height); the descender draws below it, unclipped.
        var shape = glyph.Shape.Clone();
        shape.Transform = new ScaleTransform(scale, scale);
        shape.Freeze();
        var path = new System.Windows.Shapes.Path { Data = shape, Width = glyph.Advance * scale, Height = table.Ascent * scale, Stretch = Stretch.None, Margin = new Thickness(1, 0, 1, 0) };
        path.SetBinding(System.Windows.Shapes.Shape.FillProperty, new Binding { Path = new PropertyPath(TextElement.ForegroundProperty), RelativeSource = RelativeSource.Self });
        return new InlineUIContainer(path) { BaselineAlignment = BaselineAlignment.Baseline };
    }

    // A TextBlock's text with emoji drawn (news cards and titles): <TextBlock local:EmojiText.Text="{Binding Title}" />.
    public static readonly DependencyProperty TextProperty = DependencyProperty.RegisterAttached("Text", typeof(string), typeof(EmojiText),
        new PropertyMetadata(null, (target, e) =>
        {
            if (target is not TextBlock block) return;
            block.Inlines.Clear();
            if (e.NewValue is string text) block.Inlines.AddRange(Inlines(text, block.FontSize).ToArray());
        }));

    public static string? GetText(DependencyObject element) => (string?)element.GetValue(TextProperty);
    public static void SetText(DependencyObject element, string? value) => element.SetValue(TextProperty, value);
}
