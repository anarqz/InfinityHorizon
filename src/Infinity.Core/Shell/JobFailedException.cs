// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>A job that failed with a reason already in the player's words (its toast shows <see cref="Exception.Message"/> as is).</summary>
public sealed class JobFailedException : Exception
{
    public JobFailedException() { }

    public JobFailedException(string message) : base(message) { }

    public JobFailedException(string message, Exception inner) : base(message, inner) { }
}
