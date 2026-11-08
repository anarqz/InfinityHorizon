// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>
/// The command-line server selection of <c>ServerConfig.From</c> (official or development; a custom server is never chosen
/// here), shared by the launcher and by the updater, which applies it to the arguments it restarts the launcher with.
/// </summary>
public static class ServerSelection
{
    /// <summary>Official builds use production only; development builds keep their explicit local selection.</summary>
#if INFINITY_PRODUCTION_BUILD
    public static Uri DefaultServer => ProductionOrigins.Core;
    public static Uri DefaultPortal => ProductionOrigins.Core;
    public const bool DefaultProduction = true;
#else
    public static Uri DefaultServer { get; } = new("http://127.0.0.1:8480");
    public static Uri DefaultPortal { get; } = new("http://127.0.0.1:3120");
    public const bool DefaultProduction = false;
#endif

    /// <summary><c>--server &lt;url&gt;</c> / <c>--portal &lt;url&gt;</c>, then <c>FI_SERVER</c> / <c>FI_PORTAL</c>, then the defaults; validated.</summary>
    public static (Uri Server, Uri Portal, bool Production) Resolve(IReadOnlyList<string> args, Func<string, string?> env)
    {
        ArgumentNullException.ThrowIfNull(args);
        ArgumentNullException.ThrowIfNull(env);
        string? Arg(string name)
        {
            var at = args.ToList().IndexOf(name);
            if (at < 0) return null;
            if (at + 1 >= args.Count || args[at + 1].StartsWith("--", StringComparison.Ordinal) || args.Count(value => value == name) != 1)
                throw new ArgumentException("Invalid configuration argument");
            return args[at + 1];
        }
#if INFINITY_PRODUCTION_BUILD
        // Environment inherited from a development shortcut/prefix must never select LAN.
        var server = Arg("--server");
        var portal = Arg("--portal");
        var environment = Arg("--environment");
        if (environment is not (null or "production")) throw new ArgumentException("This build uses production only");
        const bool production = true;
#else
        var server = Arg("--server") ?? env("FI_SERVER");
        var portal = Arg("--portal") ?? env("FI_PORTAL");
        var environment = Arg("--environment") ?? env("FI_ENVIRONMENT");
        if (environment is not (null or "development" or "production"))
            throw new ArgumentException("Environment must be development or production");
        var production = environment == "production";
#endif
        var resolved = (
            OriginPolicy.Check(server is null ? production ? ProductionOrigins.Core : DefaultServer : new Uri(server)),
            OriginPolicy.Check(portal is null ? production ? ProductionOrigins.Core : DefaultPortal : new Uri(portal)), production);
        Validate(resolved.Item1, resolved.Item2, production);
        return resolved;
    }

    /// <summary>The rules of an official or development (not custom) selection.</summary>
    public static void Validate(Uri server, Uri portal, bool production)
    {
        ArgumentNullException.ThrowIfNull(server);
        ArgumentNullException.ThrowIfNull(portal);
        if (BuildProfile.ProductionOnly && !production) throw new ArgumentException("This build uses production or a custom server only");
        if (production)
        {
            ProductionOrigins.RequireOrigin(server, ProductionOrigins.Core);
            // No hosted production portal is available. Native account operations use core.
            ProductionOrigins.RequireOrigin(portal, ProductionOrigins.Core);
        }
        else if (ProductionOrigins.IsOfficial(server) || ProductionOrigins.IsOfficial(portal))
            throw new ArgumentException("Official endpoints require explicit production selection");
    }

    /// <summary>The default selection's state folder: <c>production</c> in official builds, the root in development (<c>ServerConfig.Default.StateDirectory</c>).</summary>
    public static string DefaultStateDirectory(string root) => DefaultProduction ? Path.Combine(root, "production") : root;
}
