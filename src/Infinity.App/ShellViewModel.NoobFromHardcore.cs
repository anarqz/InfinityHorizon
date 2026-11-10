// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// Owner 2026-10-28: "When creating a NOOB profile, choose to start from a hardcore one (same way we can create a hardcore from a noob)",
/// and SAVE_MANAGER_UX §2.6 "From my HARDCORE profile" on a Noob's Start screen. A Noob career (a new profile, or a new career of the chosen
/// game) starts from the account's Hardcore career of the same game (<see cref="NoobStart"/>): that career's newest sealed save on this PC,
/// else its newest cloud revision (downloaded, as History's copy does). Whatever the Noob's folder held is archived in History first
/// (§7.1); the Hardcore folder and line are only read. No check: a Noob line takes any save (B2), and its first Play uploads the copy as a
/// normal upload (SAVES_API §4.3; the §4.10 seed is Hardcore-only). Fix 2026-11-08: never a silent fallback; a copy that cannot happen says
/// why (en + pt-BR), is logged, and nothing is created or written.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _newNoobFromHardcore;
    private SeedSourceRow? _newNoobSource;

    /// <summary>The cloud can be read for a Hardcore career's newest revision (signed in, the game keeps cloud saves, a saves service).</summary>
    private bool NoobCloudReachable => SignedIn && !OfflineMode && CurrentGame.FullFeatures && CloudSaveClient() is not null;

    /// <summary>The account's (or, offline, this PC's) Hardcore careers of the chosen game.</summary>
    private IReadOnlyList<Profile> NoobCandidates => NoobStart.Candidates(Profiles, CurrentGameId ?? "");

    /// <summary>What this PC knows of <paramref name="hardcore"/>: its local save (and which), its cloud revision from <c>/me</c>.</summary>
    private (HardcoreSourceFacts Facts, SaveProfile? Save) HardcoreFacts(Profile hardcore)
    {
        SaveProfile? save = null;
        if (_install is not null)
        {
            // its own folder; a Hardcore account profile kept on this PC plays its local identity's folder (OD-120)
            var folders = new List<string>();
            if (HardcoreSave(hardcore) is { } own) folders.Add(own);
            try
            {
                if (LocalProfiles.Read().FirstOrDefault(p => p.Cloud?.ProfileId == hardcore.ProfileId && p.Game == hardcore.Game) is { } local)
                    folders.Add(AzimuthSaves.FolderFor(local.OfflinePlayXuid));
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("saves: local profiles not read for a Hardcore source: " + e.GetType().Name); }
            try
            {
                var saves = SavesList(AzimuthSaves.SavesRoot(_install.Root, CurrentGame));
                save = folders.Select(f => saves.FirstOrDefault(s => s.Folder == f)).OfType<SaveProfile>().FirstOrDefault();
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("saves: Hardcore career not read: " + e.GetType().Name); }
        }
        var cloud = _session?.Me?.Players?.SelectMany(p => p.Careers).FirstOrDefault(c => c.CareerId == hardcore.ProfileId)?.Cloud?.Revision;
        return (new HardcoreSourceFacts(hardcore, save is not null, cloud), save);
    }

    private static HardcoreSourceFacts? FirstFact(IReadOnlyList<HardcoreSourceFacts> facts) => facts.Count > 0 ? facts[0] : null;

    private IReadOnlyList<HardcoreSourceFacts> NoobFacts(IEnumerable<Profile>? pool = null) =>
        [.. (pool is null ? NoobCandidates : NoobStart.Candidates(pool, CurrentGameId ?? "")).Select(p => HardcoreFacts(p).Facts)];

    /// <summary>The usable plans among <paramref name="pool"/>'s Hardcore careers of the chosen game (the picker's rows when null).</summary>
    private IReadOnlyList<NoobStartPlan> UsableNoobSources(IEnumerable<Profile>? pool = null) =>
        NoobStart.Usable(CurrentGame, NoobFacts(pool), NoobCloudReachable);

    /// <summary>The Hardcore careers of the chosen game a Noob can start from (a save on this PC, or in the cloud), in /me order.</summary>
    public IReadOnlyList<SeedSourceRow> NoobSources => [.. UsableNoobSources().Select(p => new SeedSourceRow(p.Hardcore!))];

    /// <summary>The XUIDs whose own folder of the chosen game holds a sealed save on this PC.</summary>
    private HashSet<string> LocalSaveXuids()
    {
        var set = new HashSet<string>(StringComparer.Ordinal);
        if (_install is null) return set;
        try
        {
            foreach (var save in SavesList(AzimuthSaves.SavesRoot(_install.Root, CurrentGame)))
                if (save.Xuid is { } xuid) set.Add(xuid.ToString(CultureInfo.InvariantCulture));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("saves: this PC's saves not listed for the board: " + e.GetType().Name); }
        return set;
    }

    /// <summary>The folder a Hardcore profile row plays (its XUID's <c>User_</c> folder), or null.</summary>
    private static string? HardcoreSave(Profile profile) =>
        ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)
            ? AzimuthSaves.FolderFor(xuid) : null;

    /// <summary>The form's "Start from one of my Hardcore profiles": shown for a Noob whenever the account has a Hardcore career of the game.</summary>
    public bool ShowNoobSeedOption => NewNoob && NoobCandidates.Count > 0;

    public bool NewNoobFromHardcore
    {
        get => _newNoobFromHardcore && ShowNoobSeedOption;
        set
        {
            _newNoobFromHardcore = value;
            if (value && _newNoobSource is null && NoobSources is [var only]) _newNoobSource = only;
            ChangedNoobSeedForm();
        }
    }

    public SeedSourceRow? NewNoobSource
    {
        get => _newNoobSource is { } chosen && NoobSources.Contains(chosen) ? chosen : null;
        set
        {
            // the list is rebuilt on every read: an equal row re-selected is no change (see NewSeedSource)
            if (Equals(_newNoobSource, value)) return;
            _newNoobSource = value;
            Changed(nameof(NewNoobSource));
            Changed(nameof(NoobSeedPlanText));
        }
    }

    /// <summary>What will actually happen with the chosen source (or why no Hardcore career can be used), in the player's words.</summary>
    public string NoobSeedPlanText
    {
        get
        {
            if (!ShowNoobSeedOption) return string.Empty;
            var facts = NoobFacts();
            if (NewNoobSource?.Profile is { } chosen && facts.FirstOrDefault(f => f.Hardcore.ProfileId == chosen.ProfileId) is { } picked)
                return NoobPlanText(NoobStart.Plan(CurrentGame, picked, NoobCloudReachable), picked, NewName.Trim());
            if (NoobSources.Count > 0) return S["seed.noob_option_hint"];
            return NoobPlanText(NoobStart.Plan(CurrentGame, FirstFact(facts), NoobCloudReachable), FirstFact(facts), NewName.Trim());
        }
    }

    private string NoobPlanText(NoobStartPlan plan, HardcoreSourceFacts? facts, string noobName) => S.Format(plan.TextKey,
        ("hardcore", ProfileName.Plain(plan.Hardcore?.Gamertag ?? facts?.Hardcore.Gamertag)), ("game", GameLabel),
        ("name", string.IsNullOrWhiteSpace(noobName) ? S["noob_hc.new_profile"] : ProfileName.Plain(noobName)),
        ("rev", (facts?.CloudRevision ?? 0).ToString(CultureInfo.CurrentCulture)));

    private void ChangedNoobSeedForm()
    {
        Changed(nameof(ShowNoobSeedOption));
        Changed(nameof(NewNoobFromHardcore));
        Changed(nameof(NewNoobSource));
        Changed(nameof(NoobSources));
        Changed(nameof(NoobSeedPlanText));
    }

    /// <summary>The form's pre-check: the plan for <paramref name="source"/>; null (with the reason shown and logged) when it cannot start.</summary>
    private NoobStartPlan? NoobPlanBeforeCreate(Profile source, string noobName)
    {
        var (facts, _) = HardcoreFacts(source);
        var plan = NoobStart.Plan(CurrentGame, facts, NoobCloudReachable);
        if (plan.Possible) return plan;
        Log($"saves: Noob from Hardcore refused before creating ({plan.TextKey}, local={facts.LocalSave}, cloud=r{facts.CloudRevision ?? 0})");
        Message = S.Format("noob_hc.not_created", ("reason", NoobPlanText(plan, facts, noobName)));
        return null;
    }

    /// <summary>
    /// Copies <paramref name="source"/>'s newest save (this PC's, else its newest cloud revision) into the Noob career
    /// <paramref name="target"/>'s folder, archiving what that folder held first. False, with the reason shown and logged, when it could not:
    /// nothing was written, and the career stays empty (Fresh still offered).
    /// </summary>
    private async Task<bool> StartNoobFromHardcoreAsync(Profile target, Profile source)
    {
        var name = ProfileName.Plain(target.Gamertag);
        var hardcoreName = ProfileName.Plain(source.Gamertag);
        bool? done = null;
        var (facts, _) = HardcoreFacts(source);
        var plan = NoobStart.Plan(CurrentGame, facts, NoobCloudReachable);
        var label = S.Format(plan.Source == NoobStartSource.Cloud ? "busy.downloading_from" : "busy.copying_from", ("mode", BusyMode("hardcore")));
        var ran = false;
        await RunOrStep(label, async () => { ran = true; done = await CopyHardcoreIntoNoobAsync(target, source, name, hardcoreName); });
        if (ran && done is null) Log("saves: Noob from Hardcore failed: a network or API error (shown to the player)");
        else if (done is null)
        {
            // another action held the launcher: nothing ran, and the player is told so (never a silent fallback)
            Log("saves: Noob from Hardcore not started: the launcher was busy");
            Message = S.Format("noob_hc.failed", ("name", name), ("hardcore", hardcoreName), ("reason", S["noob_hc.busy"]));
        }
        FillProfiles();
        return done == true;
    }

    private async Task<bool> CopyHardcoreIntoNoobAsync(Profile target, Profile source, string name, string hardcoreName)
    {
        void Failed(string reason, string log)
        {
            Log("saves: Noob from Hardcore failed: " + log);
            Message = S.Format("noob_hc.failed", ("name", name), ("hardcore", hardcoreName), ("reason", reason));
        }
        if (_install is null || target.Mode == "hardcore" || !ulong.TryParse(target.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var targetXuid))
        {
            Failed(S["play.setup_needed"], "no install or not a Noob target");
            return false;
        }
        if (Playing || System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0) { Failed(S["saves.game_running"], "the game is running"); return false; }
        var (facts, save) = HardcoreFacts(source);
        var plan = NoobStart.Plan(CurrentGame, facts, NoobCloudReachable);
        if (!plan.Possible) { Failed(NoobPlanText(plan, facts, name), plan.TextKey); return false; }
        var root = AzimuthSaves.SavesRoot(_install.Root, CurrentGame);
        var targetFolder = CloudSaveFiles.ProfileDir(root, targetXuid);
        // §7.1: what the Noob's folder holds goes to History first; a failed copy there stops here (ArchiveFirst says so)
        if (!ArchiveFirst("copy", targetFolder, "sm.reason.copy", [("from", "HARDCORE · " + hardcoreName), ("to", "NOOB · " + name)], "hardcore", "noob"))
        {
            Log("saves: Noob from Hardcore stopped: History not written");
            return false;
        }
        if (plan.Source == NoobStartSource.ThisPc && save is not null)
        {
            BusyStep(S.Format("busy.copying_from", ("mode", BusyMode("hardcore"))));
            CloneResult result;
            try { result = await Task.Run(() => AzimuthSaves.Clone(root, save, targetXuid, DateTime.UtcNow)); }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
            {
                Failed(S["saves.clone_failed"] + " (" + e.Message + ")", "clone " + e.GetType().Name);
                return false;
            }
            Log($"saves: Noob from Hardcore (this PC) {save.Folder}/{save.Revision.Number:x16} -> {result.Target}/{result.Number:x16}");
            try { new SaveLinks(_stateDirectory, CurrentGame.Id).Set(target.Xuid, new SaveLink(save.Folder, save.Revision.Number, result.Number, DateTime.UtcNow)); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("save link not written: " + e.Message); }
        }
        else
        {
            BusyStep(S.Format("busy.downloading_from", ("mode", BusyMode("hardcore"))));
            if (_session is not { } session || CloudSaveClient() is not { } client || !ulong.TryParse(source.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var sourceXuid))
            {
                Failed(S["careers.download_failed"], "no saves service");
                return false;
            }
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var (head, failure) = await client.Saves.HeadAsync(access, sourceXuid);
            if (head is null || failure is not null || head.Revision == 0 || head.IsCloudExpired())
            {
                Failed(S["careers.download_failed"], "head " + (failure?.Code ?? (head is null ? "none" : head.Revision == 0 ? "empty" : "expired")));
                return false;
            }
            var outcome = await client.Saves.InstallRevisionOntoAsync(access, root, sourceXuid, head.Revision, head.ContentSha256 ?? string.Empty, targetXuid, "before-noob-from-hardcore");
            if (outcome.Failure is { } refused)
            {
                Failed(S["careers.download_failed"] + " (" + refused.Message + ")", $"cloud r{head.Revision} → {refused.Code}");
                return false;
            }
            Log($"saves: Noob from Hardcore (cloud r{head.Revision}) -> {AzimuthSaves.FolderFor(targetXuid)}");
        }
        Message = S.Format("noob_hc.done", ("name", name), ("hardcore", hardcoreName));
        return true;
    }
}
