// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

// The account's support links (CORE_API.md §3, register CR-L52). Mirrors of fi-protocol's core.rs / packages/contracts/src/core.ts
// (ContractParityTests).

/// <summary>Support links (<c>SupportLinks</c> in the contract; named apart from <see cref="Shell.SupportLinks"/>): the Ko-fi page, the help
/// centre, where to report a problem.</summary>
public sealed record AccountSupportLinks(string Kofi, string Help, string Report);

/// <summary><c>GET /core/v1/me/support</c> → 200. <see cref="ReportToken"/> is null until the report bundle upload exists.</summary>
public sealed record SupportInfo(AccountSupportLinks Links, string? ReportToken);
