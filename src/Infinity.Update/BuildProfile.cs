// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core;

/// <summary>Official production binaries cannot inherit development endpoints or trust.</summary>
public static class BuildProfile
{
    public static bool ProductionOnly =>
#if INFINITY_PRODUCTION_BUILD
        true;
#else
        false;
#endif
}
