// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net.Http;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// Batch 1118 (SAURON, 3.2.1: FH2 ANNA 1.2.0 and Underglow 1.1.0 on, the catalog offering ANNA 1.2.4 and Underglow 1.1.1, which needs
/// ANNA 1.2.4): an update that needs another mod's update offers both in its review and installs them from one confirmation, dependency
/// first (<see cref="ManagedModFiles.UpdateTogetherAsync"/>). A refusal that is real names the mod it is about and why, never a generic failure.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The updates <paramref name="next"/> needs made with it, dependency first (<see cref="ManagedModFiles.UpdatePlan"/>).</summary>
    internal static IReadOnlyList<ModManifest> UpdatePlan(ModManifest next, IReadOnlyList<ModManifest> catalog, ModHealth health) =>
        ManagedModFiles.UpdatePlan(next, catalog, health.Installed);

    /// <summary>A mod's name for a message: the installed manifest's, else the catalog's, else its id.</summary>
    private string NameOf(string id) => (_modHealth.Installed.FirstOrDefault(i => i.Id == id)?.Manifest ?? _manifestCatalog.FirstOrDefault(m => m.Id == id))
        ?.Text("name", S.Locale) ?? id;

    /// <summary>A dependency refusal that names its mod ("Underglow is on and works only with version 1.2.0 of this mod…"), or null.</summary>
    private string? RefusalText(ModException? refused) => refused is not { Code: ModErrors.Dependency, Related: { } related } ? null
        : S.Format((refused.RelatedIsDependent, refused.RelatedProblem) switch
        {
            (true, ModRelated.Settings) => "mods.refused_dependent_settings",
            (true, _) => "mods.refused_dependent",
            // another version than the mod accepts, and none installed or offered by the catalog: nothing the player can install now
            (false, ModRelated.Version) => "mods.refused_dependency_version",
            (false, ModRelated.Settings) => "mods.refused_dependency_settings",
            (false, ModRelated.Changed) => "mods.refused_dependency_changed",
            _ => "mods.refused_dependency",
        }, ("mod", NameOf(related)), ("versions", string.Join(" / ", refused.RelatedVersions)));

    /// <summary>A launch refused for an enabled mod whose dependency is missing, off or another version: both named, and what to do.</summary>
    private string UnmetDependencyText(ModUnmetDependencyException unmet) => S.Format("mods.launch_unmet_dependency",
        ("mod", NameOf(unmet.Mod)), ("dependency", NameOf(unmet.Dependency)), ("versions", string.Join(" / ", unmet.Versions)));

    /// <summary>Why an update card waits: the named refusal, else its category's sentence.</summary>
    private string UpdateBlockerText(ModCardFacts fact) => RefusalText(fact.Refusal) ?? ModErrorText(fact.Blocker!);

    /// <summary>"FH2 ANNA 1.2.4, Underglow 1.1.1": a plan's mods in the order they are updated.</summary>
    private string PlanNames(IEnumerable<ModManifest> plan) => string.Join(", ", plan.Select(m => m.Text("name", S.Locale) + " " + m.Version));

    /// <summary>The review's line for an update made together with others (empty for an update alone).</summary>
    private string TogetherText(ModCardFacts fact) => fact.Together.Count > 1 ? "\n" + S.Format("mods.update_together", ("mods", PlanNames(fact.Together))) : "";

    /// <summary>A failed step of an update made together: which mod, why, what was not changed and what was turned off.</summary>
    private string UpdatePlanFailureText(ModUpdatePlanException failed)
    {
        var reason = failed.InnerException switch
        {
            ModException inner => RefusalText(inner) ?? ModErrorText(inner.Code),
            HttpRequestException or OperationCanceledException => S["mods.download_failed"],
            _ => S["mods.transaction_failed"],
        };
        var text = S.Format("mods.update_plan_failed", ("mod", NameOf(failed.Mod)), ("reason", reason));
        if (failed.NotChanged.Count > 0) text += " " + S.Format("mods.update_plan_not_changed", ("mods", string.Join(", ", failed.NotChanged.Select(NameOf))));
        if (failed.TurnedOff.Count > 0) text += " " + S.Format("mods.update_plan_turned_off", ("mods", string.Join(", ", failed.TurnedOff.Select(NameOf))));
        if (failed.StillOn.Count > 0) text += " " + S.Format("mods.update_plan_still_on", ("mods", string.Join(", ", failed.StillOn.Select(NameOf))));
        return text;
    }

    /// <summary>The confirmed update of <paramref name="card"/> with the others it needs, from one confirmation, under one screen lock.</summary>
    private Task UpdateModsTogetherAsync(ModCard card)
    {
        var plan = card.Together;
        var factory = ModManagerFactory()!; var hosts = CatalogHosts; var hardcore = ProfileHardcore;
        return RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => ModStep(S["mods.progress." + stage]));
            using var timeout = new CancellationTokenSource(TimeSpan.FromMinutes(3 * plan.Count));
            var manager = await Task.Run(factory);
            try { await manager.UpdateTogetherAsync(ModDownloads, plan, hosts, timeout.Token, progress, hardcore); }
            finally
            {
                // what each step installed is from this catalog, whatever a later step did
                foreach (var mod in plan)
                    try { if (manager.ReceiptFor(mod.Id)?.ManifestHash == mod.Digest) RecordModSource(mod.Id); }
                    catch (Exception error) when (ManagedModFiles.IsStateError(error)) { }
            }
        }, S.Format("mods.updated_together", ("mods", PlanNames(plan))), label: S.Format("mods.lock.update_together", ("mods", PlanNames(plan))),
            only: [.. plan.Select(m => m.Id)]);
    }
}
