// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;

namespace Infinity.Core.Shell;

/// <summary>Queue formatting shared by the WPF bindings and their tests.</summary>
public static class QueueText
{
    public static string Estimate(Strings strings, QueuePlace place)
    {
        ArgumentNullException.ThrowIfNull(strings);
        ArgumentNullException.ThrowIfNull(place);
        return place.EtaSeconds is { } seconds
            ? strings.Format("queue.eta", ("m", Math.Max(1, Math.Ceiling(seconds / 60.0)).ToString(CultureInfo.InvariantCulture)))
            : strings["queue.eta_unknown"];
    }
}
