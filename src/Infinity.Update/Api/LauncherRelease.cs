// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

// Exact third mirror of fi_protocol::update / LAUNCHER_UPDATE.md (78d35c5).
public sealed record LauncherReleaseDescriptor(byte SchemaVersion, string Product, string Repository, string Tag,
    string Channel, string Platform, string Version, ulong ReleaseSequence, string SourceCommit, string IssuedAt,
    string ExpiresAt, LauncherReleasePackage Package, IReadOnlyList<LauncherReleaseFile> Files);
public sealed record LauncherReleasePackage(string Name, ulong ByteLength, string Sha256);
public sealed record LauncherReleaseFile(string Path, ulong ByteLength, string Sha256);
