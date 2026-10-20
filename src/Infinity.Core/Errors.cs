// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core;

/// <summary>A malformed or unexpected frame, environment value or pipe event. The receiver closes the pipe.</summary>
public sealed class ProtocolException(string message) : Exception(message);

/// <summary>A launch ticket that fails a claim check (kid, version, nonce, lifetime, server).</summary>
public sealed class TicketException(string message) : Exception(message);

/// <summary>A launch ticket whose Ed25519 signature does not verify.</summary>
public sealed class SignatureException() : Exception("ticket signature does not verify");
