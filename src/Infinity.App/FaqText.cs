// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Windows;
using System.Windows.Controls;
using System.Windows.Documents;
using System.Windows.Media;
using Infinity.Core.Help;

namespace Infinity.App;

public sealed class FaqText : TextBlock
{
    public static readonly DependencyProperty MarkupProperty = DependencyProperty.Register(
        nameof(Markup), typeof(string), typeof(FaqText), new PropertyMetadata(string.Empty, OnMarkupChanged));

    public string Markup
    {
        get => (string)GetValue(MarkupProperty);
        set => SetValue(MarkupProperty, value);
    }

    private static void OnMarkupChanged(DependencyObject target, DependencyPropertyChangedEventArgs args)
    {
        var block = (FaqText)target;
        block.Inlines.Clear();
        foreach (var span in FaqMarkup.Parse(args.NewValue as string ?? string.Empty))
        {
            var run = new Run(span.Text);
            if (span.Bold) run.FontWeight = FontWeights.SemiBold;
            if (span.Italic) run.FontStyle = FontStyles.Italic;
            if (span.Underline) run.TextDecorations = System.Windows.TextDecorations.Underline;
            if (span.Code) run.FontFamily = new FontFamily("Consolas");
            block.Inlines.Add(run);
        }
    }
}
