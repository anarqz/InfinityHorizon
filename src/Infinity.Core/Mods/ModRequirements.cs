// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>A setting a required mod must have (a catalog entry's <c>required.settings</c>).</summary>
public sealed record ModRequiredSetting(string OptionId, JsonElement Value);

/// <summary>OD-115: a mod the signed catalog requires before every Play (offline and online, modded or vanilla).</summary>
public sealed record ModRequirement(string Id, IReadOnlyList<ModRequiredSetting> Settings);

/// <summary>What Play needs to do about the required mods; only <see cref="Ready"/> lets it start.</summary>
public enum ModRequirementState
{
    /// <summary>Every required mod is managed, enabled and has its required settings (or nothing is required, or a dedicated host).</summary>
    Ready,
    /// <summary>No signed catalog was ever loaded, so what is required is unknown: Play waits for the catalog.</summary>
    CatalogUnavailable,
    /// <summary>Not installed: install it from the signed catalog.</summary>
    Install,
    /// <summary>Installed by hand or by an older launcher, exactly as its manifest: adopt it (the existing adoption flow).</summary>
    Adopt,
    /// <summary>Managed but disabled: enable it.</summary>
    Enable,
    /// <summary>Managed and enabled, but a required setting differs (restored by the launch's locks, or shown).</summary>
    Settings,
    /// <summary>Not installable now (its manifest is not cached, or its installation needs attention on the Mods page).</summary>
    Unavailable,
}

/// <summary>The first unmet requirement (<see cref="Id"/>, and the manifest to act on when known).</summary>
public sealed record ModRequirementStatus(ModRequirementState State, string? Id = null, ModManifest? Manifest = null)
{
    public static ModRequirementStatus Satisfied { get; } = new(ModRequirementState.Ready);
    public bool Blocks => State != ModRequirementState.Ready;
}

/// <summary>
/// OD-115: the required-mods check before Play, generic over the catalog's declarations (OD-111: no mod is named in code). A
/// dedicated host (OD-109) runs no game and needs none.
/// </summary>
public static class ModRequirements
{
    /// <param name="required">The catalog's requirements; <c>null</c> when no catalog was ever loaded.</param>
    /// <param name="installed">The installed mods (<see cref="ModHealth.Installed"/>).</param>
    /// <param name="catalog">The catalog manifests (install source).</param>
    /// <param name="canAdopt">Whether an existing installation matches a manifest exactly (<see cref="ManagedModFiles.CanAdopt"/>).</param>
    /// <param name="settings">The current values of an installed mod's settings, or <c>null</c> to skip the settings check.</param>
    public static ModRequirementStatus Evaluate(IReadOnlyList<ModRequirement>? required, IReadOnlyList<ModInstallation> installed,
        IReadOnlyList<ModManifest> catalog, Func<ModManifest, bool> canAdopt, bool dedicated = false,
        Func<ModManifest, IReadOnlyDictionary<string, JsonElement>?>? settings = null)
    {
        ArgumentNullException.ThrowIfNull(installed); ArgumentNullException.ThrowIfNull(catalog); ArgumentNullException.ThrowIfNull(canAdopt);
        if (dedicated) return ModRequirementStatus.Satisfied;
        if (required is null) return new(ModRequirementState.CatalogUnavailable);
        foreach (var requirement in required)
        {
            var install = installed.FirstOrDefault(i => i.Id == requirement.Id);
            var offered = catalog.FirstOrDefault(m => m.Id == requirement.Id);
            if (install is { Problem: not null }) return new(ModRequirementState.Unavailable, requirement.Id, install.Manifest ?? offered);
            if (install?.Manifest is not { } manifest)
            {
                if (offered is null) return new(ModRequirementState.Unavailable, requirement.Id);
                return new(canAdopt(offered) ? ModRequirementState.Adopt : ModRequirementState.Install, requirement.Id, offered);
            }
            if (!install.Enabled) return new(ModRequirementState.Enable, requirement.Id, manifest);
            if (settings is not null && requirement.Settings.Count > 0)
            {
                var values = settings(manifest);
                if (values is null || requirement.Settings.Any(s => !values.TryGetValue(s.OptionId, out var value) || !JsonElement.DeepEquals(value, s.Value)))
                    return new(ModRequirementState.Settings, requirement.Id, manifest);
            }
        }
        return ModRequirementStatus.Satisfied;
    }
}
