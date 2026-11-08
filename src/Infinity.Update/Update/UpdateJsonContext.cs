// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.Json.Serialization.Metadata;
using Infinity.Core.Api;

namespace Infinity.Core.Update;

/// <summary>
/// U2: source-generated metadata for every document the update path reads or writes (release descriptor, plan, journal,
/// health and parent hand-shake), so infinity-updater.exe runs under Native AOT without reflection. It is only the
/// <see cref="JsonSerializerOptions.TypeInfoResolver"/> of the existing options: naming, strictness and depth stay those
/// options' own, and the bytes are those the reflection serializer wrote (tests compare both).
/// </summary>
[JsonSerializable(typeof(LauncherReleaseDescriptor))]
[JsonSerializable(typeof(UpdateApplyPlan))]
[JsonSerializable(typeof(UpdateJournal))]
[JsonSerializable(typeof(UpdateHealth))]
[JsonSerializable(typeof(UpdateParentReady))]
internal sealed partial class UpdateJsonContext : JsonSerializerContext
{
    /// <summary>The metadata of <typeparamref name="T"/> bound to <paramref name="options"/> (which must use this context).</summary>
    internal static JsonTypeInfo<T> TypeInfo<T>(JsonSerializerOptions options) => (JsonTypeInfo<T>)options.GetTypeInfo(typeof(T));
}
