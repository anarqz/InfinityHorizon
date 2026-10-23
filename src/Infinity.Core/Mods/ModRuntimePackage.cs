// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Process;

namespace Infinity.Core.Mods;

/// <summary>
/// The native mod runtime shipped next to the launcher (<c>mod\infinity_mod.dll</c> and <c>mod\infinity-mod-prepare.exe</c>,
/// each with its detached <c>.sig</c>). Native (<c>infinity-native</c>) mods are available only when both files are present
/// and verify with the build's key (OD-49: the release key in production; the development key against a development server).
/// A package without them (today's production layout) leaves native mods unavailable, never half-started; file-only (data)
/// mods need no runtime (MODS.md §6).
/// </summary>
public sealed record ModRuntimePackage(string Runtime, string Helper)
{
    public const string Folder = "mod";
    public const string RuntimeFile = "infinity_mod.dll";
    public const string HelperFile = "infinity-mod-prepare.exe";

    /// <summary>The verified package under <paramref name="launcherDirectory"/>, or null (missing file or signature, wrong key, unreadable).</summary>
    public static ModRuntimePackage? Locate(string launcherDirectory, ReadOnlySpan<byte> key)
    {
        ArgumentNullException.ThrowIfNull(launcherDirectory);
        if (key.Length != 32) return null;
        var folder = Path.Combine(launcherDirectory, Folder);
        var runtime = Path.Combine(folder, RuntimeFile);
        var helper = Path.Combine(folder, HelperFile);
        foreach (var path in new[] { runtime, helper })
            if (!File.Exists(path) || !File.Exists(path + ".sig") || !ReleaseSignature.VerifyFile(path, path + ".sig", key)) return null;
        return new(runtime, helper);
    }
}
