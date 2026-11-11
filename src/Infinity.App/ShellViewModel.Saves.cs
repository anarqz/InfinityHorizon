// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;

namespace Infinity.App;

/// <summary>
/// OD-239 (T-1111-AZIMUTH): the OD-41 picker that copied one of Azimuth's own saves into a new profile's folder is gone. An Azimuth
/// profile plays Azimuth's own save (the hook points the game at <c>User_9000000000000</c>), and a Hardcore career starts fresh or
/// from the server (OD-60, CR-S52), so the launcher never copies a save into a profile's folder when a profile is made.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The profile's XUID (a decimal string, ACCOUNTS.md §1); null, with a message, when it is not one.</summary>
    private ulong? ParseXuid(Profile profile)
    {
        if (ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid))
        {
            return xuid;
        }
        Message = S["saves.read_failed"] + " (xuid)";
        return null;
    }
}
