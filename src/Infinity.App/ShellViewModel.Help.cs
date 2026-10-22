// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using Infinity.Core.Help;

namespace Infinity.App;

public sealed record FaqRow(string Title, string Description, string? DiscordLink, string DiscordLabel)
{
    public bool HasDiscordLink => DiscordLink is not null;
}

public sealed partial class ShellViewModel
{
    private bool _onHelp;
    private string _helpQuery = string.Empty;
    public bool ShowHelp => SignedIn && _onHelp && !ShowPickSave;
    public ObservableCollection<FaqRow> HelpItems { get; } = [];
    public bool HelpEmpty => HelpItems.Count == 0;
    public string HelpResultCount => S.Format("help.results", ("count", HelpItems.Count.ToString(System.Globalization.CultureInfo.InvariantCulture)));
    public string HelpQuery
    {
        get => _helpQuery;
        set
        {
            if (_helpQuery == value) return;
            _helpQuery = value;
            RefreshHelp();
            Changed();
        }
    }

    public void OpenHelp()
    {
        if (!SignedIn) return;
        LeaveAccount();
        _onMods = _onSetup = _onBugReport = _crashOpen = false;
        _onHelp = true;
        RefreshHelp();
        Changed(string.Empty);
    }

    private void RefreshHelp()
    {
        HelpItems.Clear();
        foreach (var article in FaqCatalog.Search(HelpQuery))
            HelpItems.Add(new(article.Title(S.Locale), article.Description(S.Locale), article.DiscordLink, S["help.discord"]));
        Changed(nameof(HelpEmpty));
        Changed(nameof(HelpResultCount));
    }
}
