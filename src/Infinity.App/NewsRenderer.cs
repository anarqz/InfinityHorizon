// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Documents;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Markdig;
using Markdig.Syntax;
using Markdig.Syntax.Inlines;
using MdBlock = Markdig.Syntax.Block;
using MdInline = Markdig.Syntax.Inlines.Inline;

namespace Infinity.App;

/// <summary>
/// OD-119: a news item's markdown as a WPF document (NEWS_FEED.md §3). CommonMark through Markdig; HTML is shown as its text; images
/// come from the item's folder through <c>image</c> (the alt text stays when one cannot be read); links open the system browser,
/// http(s) only, so nothing local is ever opened by accident. Emoji draw with the bundled font (the document uses InfinityFont).
/// </summary>
public static class NewsRenderer
{
    private static readonly MarkdownPipeline Pipeline = new MarkdownPipelineBuilder().Build();

    public static FlowDocument Render(string markdown, Func<string, Task<byte[]?>> image, Action<Uri> open, ResourceDictionary resources)
    {
        var document = new FlowDocument
        {
            // T3b (Alex): reading on black: near-white body at a comfortable size and line height, white headings, quiet links.
            FontFamily = (FontFamily)resources["InfinityFont"], FontSize = 17, LineHeight = 28, PagePadding = new Thickness(0), TextAlignment = TextAlignment.Left,
            Foreground = (Brush)resources["NewsReadingText"], Background = Brushes.Transparent, MaxPageWidth = 760,
        };
        MarkdownDocument parsed;
        try { parsed = Markdown.Parse(markdown, Pipeline); }
        catch (Exception error) when (error is ArgumentException or InvalidOperationException or IndexOutOfRangeException)
        {
            document.Blocks.Add(new Paragraph(new Run(markdown))); // never a crash on a file we could not parse
            return document;
        }
        var context = new Context(image, open, resources);
        foreach (var block in parsed) if (Block(block, context) is { } converted) document.Blocks.Add(converted);
        return document;
    }

    private sealed record Context(Func<string, Task<byte[]?>> Image, Action<Uri> Open, ResourceDictionary Resources)
    {
        public Brush Muted => (Brush)Resources["NewsReadingMuted"];
        public Brush Heading => (Brush)Resources["NewsReadingHeading"];
        public Brush LinkBrush => (Brush)Resources["NewsReadingLink"];
        public Brush Rule => (Brush)Resources["NewsReadingRule"];
        public Brush CodeSurface => (Brush)Resources["NewsReadingCode"];
    }

    private static FontFamily Mono => new("Consolas, Courier New");

    private static System.Windows.Documents.Block? Block(MdBlock block, Context context)
    {
        switch (block)
        {
            case HeadingBlock heading:
                var title = Inlines(new Paragraph { FontWeight = FontWeights.SemiBold, Foreground = context.Heading, FontSize = heading.Level switch { 1 => 28, 2 => 22, 3 => 19, _ => 17 },
                    LineHeight = heading.Level switch { 1 => 36, 2 => 30, _ => 26 }, Margin = new Thickness(0, heading.Level == 1 ? 8 : 22, 0, 8) }, heading.Inline, context);
                return title;
            case ParagraphBlock paragraph:
                return Inlines(new Paragraph { Margin = new Thickness(0, 0, 0, 16) }, paragraph.Inline, context);
            case ListBlock list:
                var result = new List { MarkerStyle = list.IsOrdered ? TextMarkerStyle.Decimal : TextMarkerStyle.Disc, Margin = new Thickness(0, 0, 0, 16), Padding = new Thickness(34, 0, 0, 0), MarkerOffset = 10 };
                if (list.IsOrdered && int.TryParse(list.OrderedStart, out var start) && start > 0) result.StartIndex = start;
                foreach (var entry in list.OfType<ListItemBlock>())
                {
                    var item = new ListItem();
                    foreach (var child in entry) if (Block(child, context) is { } converted) item.Blocks.Add(Tight(converted));
                    result.ListItems.Add(item);
                }
                return result;
            case QuoteBlock quote:
                var section = new Section { BorderBrush = context.Rule, BorderThickness = new Thickness(3, 0, 0, 0), Padding = new Thickness(16, 2, 0, 2), Margin = new Thickness(0, 0, 0, 16), Foreground = context.Muted };
                foreach (var child in quote) if (Block(child, context) is { } converted) section.Blocks.Add(converted);
                return section;
            case FencedCodeBlock or CodeBlock:
                var code = (LeafBlock)block;
                return new Paragraph(new Run(code.Lines.ToString())) { FontFamily = Mono, FontSize = 14, LineHeight = 21, Background = context.CodeSurface, Padding = new Thickness(14, 10, 14, 10), Margin = new Thickness(0, 0, 0, 16) };
            case ThematicBreakBlock:
                return new BlockUIContainer(new Border { Height = 1, Background = context.Rule, Margin = new Thickness(0, 12, 0, 12) });
            case HtmlBlock html:
                return new Paragraph(new Run(html.Lines.ToString())) { Foreground = context.Muted, Margin = new Thickness(0, 0, 0, 10) }; // shown as text
            case LeafBlock leaf when leaf.Inline is not null:
                return Inlines(new Paragraph(), leaf.Inline, context);
            case LeafBlock leaf:
                return new Paragraph(new Run(leaf.Lines.ToString()));
            case ContainerBlock container:
                var group = new Section();
                foreach (var child in container) if (Block(child, context) is { } converted) group.Blocks.Add(converted);
                return group;
            default:
                return null;
        }
    }

    private static System.Windows.Documents.Block Tight(System.Windows.Documents.Block block) { if (block is Paragraph p) p.Margin = new Thickness(0, 0, 0, 4); return block; }

    private static Paragraph Inlines(Paragraph paragraph, ContainerInline? inlines, Context context)
    {
        if (inlines is not null) foreach (var inline in inlines) if (Inline(inline, context) is { } converted) paragraph.Inlines.Add(converted);
        return paragraph;
    }

    private static Span Children(Span span, ContainerInline container, Context context)
    {
        foreach (var inline in container) if (Inline(inline, context) is { } converted) span.Inlines.Add(converted);
        return span;
    }

    private static System.Windows.Documents.Inline? Inline(MdInline inline, Context context)
    {
        switch (inline)
        {
            case LiteralInline literal: return new Run(literal.Content.ToString());
            case EmphasisInline emphasis: return Children(emphasis.DelimiterCount >= 2 ? new Bold() : new Italic(), emphasis, context);
            case CodeInline code: return new Run(code.Content) { FontFamily = Mono, FontSize = 15, Background = context.CodeSurface };
            case LineBreakInline lineBreak: return lineBreak.IsHard ? new LineBreak() : new Run(" ");
            case HtmlEntityInline entity: return new Run(entity.Transcoded.ToString());
            case HtmlInline html: return new Run(html.Tag); // shown as text
            case AutolinkInline auto: return Link(auto.Url, new Span(new Run(auto.Url)), context);
            case LinkInline { IsImage: true } picture: return Image(picture, context);
            case LinkInline link: return Link(link.Url, Children(new Span(), link, context), context);
            case ContainerInline container: return Children(new Span(), container, context);
            default: return null;
        }
    }

    private static System.Windows.Documents.Inline Link(string? url, Span content, Context context)
    {
        if (!Uri.TryCreate(url, UriKind.Absolute, out var target) || target.Scheme is not ("https" or "http")) return content; // text only
        var link = new Hyperlink { Foreground = context.LinkBrush, ToolTip = target.AbsoluteUri, NavigateUri = target };
        link.Inlines.Add(content);
        link.RequestNavigate += (_, e) => { context.Open(e.Uri); e.Handled = true; };
        return link;
    }

    private static InlineUIContainer Image(LinkInline picture, Context context)
    {
        var alt = string.Concat(picture.Descendants<LiteralInline>().Select(l => l.Content.ToString()));
        var host = new ContentControl { Content = new TextBlock { Text = alt, Foreground = context.Muted, TextWrapping = TextWrapping.Wrap }, Focusable = false };
        _ = LoadAsync(host, picture.Url, context);
        return new InlineUIContainer(host) { BaselineAlignment = BaselineAlignment.Bottom };
    }

    private static async Task LoadAsync(ContentControl host, string? path, Context context)
    {
        if (string.IsNullOrWhiteSpace(path)) return;
        try
        {
            if (await context.Image(path) is not { Length: > 0 } bytes) return;
            var bitmap = new BitmapImage();
            using (var stream = new MemoryStream(bytes))
            {
                bitmap.BeginInit(); bitmap.CacheOption = BitmapCacheOption.OnLoad; bitmap.StreamSource = stream; bitmap.EndInit();
            }
            bitmap.Freeze();
            host.Content = new System.Windows.Controls.Image { Source = bitmap, Stretch = Stretch.Uniform, MaxWidth = Math.Min(bitmap.PixelWidth, 720), StretchDirection = StretchDirection.DownOnly };
        }
        catch (Exception error) when (error is IOException or NotSupportedException or InvalidOperationException or ArgumentException or FileFormatException or UnauthorizedAccessException)
        { } // the alt text stays
    }

    /// <summary>Opens an http(s) address in the system browser (the launcher never follows it itself).</summary>
    public static void OpenInBrowser(Uri url)
    {
        if (url.Scheme is not ("https" or "http")) return;
        try { Process.Start(new ProcessStartInfo(url.AbsoluteUri) { UseShellExecute = true }); }
        catch (Exception error) when (error is System.ComponentModel.Win32Exception or InvalidOperationException) { }
    }
}
