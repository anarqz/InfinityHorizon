// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

/// <summary>V1 never distributes remote paint blobs. Resource schema approval is a separate activation gate.</summary>
public sealed class RelayContentPolicy
{
    private RelayContentPolicy() { }
    public static RelayContentPolicy StockOnly { get; } = new();
}
