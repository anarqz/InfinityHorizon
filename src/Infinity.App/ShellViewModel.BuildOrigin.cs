// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Process;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    private bool _buildOriginChecked;
    private bool _launcherOfficial;
    private bool _overlayOfficial;

    public bool ShowBuildOriginWarning => _buildOriginChecked &&
        (!_launcherOfficial || !_overlayOfficial || _hookPackage?.Signer != HookSigner.Official ||
         (_hook is { Kind: HookKind.Ours or HookKind.Unknown, Signer: not HookSigner.Official }));

    /// <summary>Launcher hashing runs on a worker; development signatures never count as official.</summary>
    public async Task VerifyBuildOriginAsync()
    {
        var launcher = Environment.ProcessPath;
        _launcherOfficial = await Task.Run(() => ReleaseSignature.IsOfficialFile(launcher));
        _buildOriginChecked = true;
        // owner 2026-11-10: the overlay's and the hook's hashes and signatures on workers too (the start's 316 ms stall before
        // "overlay: package signed" in 3.1.1)
        await LoadOverlayPackageAsync();
        await RefreshHookAsync();
        Changed(nameof(ShowBuildOriginWarning));
    }
}
