// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Update;

// Local workflow state, never an HTTP request or account attestation.
public enum UpdatePhase { Checking, Current, Required, Unavailable, Downloading, Verifying, Applying, Failed, RecoveryRequired }
public sealed record UpdateDecision(UpdatePhase Phase, string? TargetVersion);
public sealed record UpdateProgress(UpdatePhase Phase, long ReceivedBytes, long TotalBytes);
