// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

/// <summary>ONLINE_CONTROL.md: the public operator-controlled availability snapshot.</summary>
public sealed record OnlineStatus(string State, ulong Revision, string? Reason, string? ClosedAt, uint RetryAfterSeconds);
