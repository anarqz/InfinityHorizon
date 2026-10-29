// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Infinity.Core.Presence;

/// <summary>Discord's local RPC opcodes (the first header field).</summary>
public static class DiscordOp
{
    public const int Handshake = 0;
    public const int Frame = 1;
    public const int Close = 2;
    public const int Ping = 3;
    public const int Pong = 4;
}

/// <summary>A refusal or a broken exchange with the local Discord app: the connection is dropped and retried later.</summary>
public sealed class DiscordIpcException(string message) : IOException(message);

/// <summary>
/// One frame of Discord's local IPC (OD-136, DISCORD_PRESENCE.md §3): an 8-byte little-endian header
/// <c>{op: u32, length: u32}</c>, then <c>length</c> bytes of UTF-8 JSON.
/// </summary>
public static class DiscordIpcFrame
{
    /// <summary>Discord's own answers are small; anything larger is a broken or foreign pipe.</summary>
    public const int MaxPayload = 64 * 1024;

    public static byte[] Encode(int op, string json)
    {
        ArgumentNullException.ThrowIfNull(json);
        var payload = Encoding.UTF8.GetBytes(json);
        if (payload.Length > MaxPayload) throw new ArgumentException("frame too large", nameof(json));
        var frame = new byte[8 + payload.Length];
        BinaryPrimitives.WriteInt32LittleEndian(frame, op);
        BinaryPrimitives.WriteInt32LittleEndian(frame.AsSpan(4), payload.Length);
        payload.CopyTo(frame, 8);
        return frame;
    }

    public static async Task WriteAsync(Stream stream, int op, string json, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(stream);
        await stream.WriteAsync(Encode(op, json), cancellation).ConfigureAwait(false);
        await stream.FlushAsync(cancellation).ConfigureAwait(false);
    }

    public static async Task<(int Op, string Json)> ReadAsync(Stream stream, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(stream);
        var header = new byte[8];
        await stream.ReadExactlyAsync(header, cancellation).ConfigureAwait(false);
        var op = BinaryPrimitives.ReadInt32LittleEndian(header);
        var length = BinaryPrimitives.ReadInt32LittleEndian(header.AsSpan(4));
        if (length is < 0 or > MaxPayload) throw new DiscordIpcException("frame length out of range");
        var payload = new byte[length];
        await stream.ReadExactlyAsync(payload, cancellation).ConfigureAwait(false);
        return (op, Encoding.UTF8.GetString(payload));
    }
}

/// <summary>An open, handshaken connection to the local Discord app.</summary>
public interface IDiscordConnection : IAsyncDisposable
{
    /// <summary><c>SET_ACTIVITY</c> for this launcher's pid; <paramref name="activity"/> null clears the card.</summary>
    Task SetActivityAsync(JsonObject? activity, CancellationToken cancellation);
}

/// <summary>
/// The launcher's own small Discord RPC client (no Discord SDK binary): <c>\\.\pipe\discord-ipc-0</c> … <c>-9</c>, the
/// handshake (op 0, <c>{v: 1, client_id}</c>, answered by a <c>READY</c> dispatch), then <c>SET_ACTIVITY</c> (op 1,
/// <c>{cmd, args: {pid, activity}, nonce}</c>). Under Wine/Proton the same pipe names reach the host's Discord through
/// the prefix's bridge. Pings are answered; a close or an <c>ERROR</c> event ends the exchange with
/// <see cref="DiscordIpcException"/>.
/// </summary>
public sealed class DiscordIpcClient : IDiscordConnection
{
    /// <summary>The "Infinity" application (OD-136 follow-up, 2026-10-29).</summary>
    public const string InfinityClientId = "1564739935443877958";

    public static readonly TimeSpan ExchangeTimeout = TimeSpan.FromSeconds(5);

    private readonly Stream _pipe;
    private readonly int _pid;
    private readonly SemaphoreSlim _exchange = new(1, 1);

    private DiscordIpcClient(Stream pipe, int pid)
    {
        _pipe = pipe;
        _pid = pid;
    }

    /// <summary>Discord's pipe name for slot <paramref name="slot"/> (0-9).</summary>
    public static string PipeName(int slot) => "discord-ipc-" + slot.ToString(System.Globalization.CultureInfo.InvariantCulture);

    /// <summary>
    /// The first slot whose pipe opens and whose Discord answers the handshake with <c>READY</c>, or null (Discord not
    /// running): never throws for an absent or refusing Discord.
    /// </summary>
    public static async Task<DiscordIpcClient?> ConnectAsync(string clientId, CancellationToken cancellation, Func<int, string>? pipeName = null,
        int? pid = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(clientId);
        pipeName ??= PipeName;
        for (var slot = 0; slot < 10; ++slot)
        {
            cancellation.ThrowIfCancellationRequested();
            var pipe = new NamedPipeClientStream(".", pipeName(slot), PipeDirection.InOut, PipeOptions.Asynchronous);
            try
            {
                await pipe.ConnectAsync(TimeSpan.FromMilliseconds(250), cancellation).ConfigureAwait(false);
            }
            catch (Exception error) when (error is TimeoutException or IOException or UnauthorizedAccessException)
            {
                await pipe.DisposeAsync().ConfigureAwait(false);
                continue;
            }
            var client = new DiscordIpcClient(pipe, pid ?? Environment.ProcessId);
            try
            {
                await client.HandshakeAsync(clientId, cancellation).ConfigureAwait(false);
                return client;
            }
            catch (Exception error) when (error is IOException or JsonException or TimeoutException or OperationCanceledException && !cancellation.IsCancellationRequested)
            {
                await client.DisposeAsync().ConfigureAwait(false);
            }
        }
        return null;
    }

    /// <summary>Wraps an already open stream (tests; never a second handshake).</summary>
    internal static async Task<DiscordIpcClient> OverAsync(Stream stream, string clientId, int pid, CancellationToken cancellation)
    {
        var client = new DiscordIpcClient(stream, pid);
        await client.HandshakeAsync(clientId, cancellation).ConfigureAwait(false);
        return client;
    }

    private async Task HandshakeAsync(string clientId, CancellationToken cancellation)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(ExchangeTimeout);
        var hello = new JsonObject { ["v"] = 1, ["client_id"] = clientId };
        await DiscordIpcFrame.WriteAsync(_pipe, DiscordOp.Handshake, hello.ToJsonString(), bound.Token).ConfigureAwait(false);
        while (true)
        {
            var (op, json) = await ReadAnswerAsync(bound.Token).ConfigureAwait(false);
            if (op != DiscordOp.Frame) continue;
            using var answer = JsonDocument.Parse(json);
            if (Text(answer.RootElement, "evt") == "READY") return;
            if (Text(answer.RootElement, "evt") == "ERROR") throw new DiscordIpcException("handshake refused");
        }
    }

    public async Task SetActivityAsync(JsonObject? activity, CancellationToken cancellation)
    {
        await _exchange.WaitAsync(cancellation).ConfigureAwait(false);
        try
        {
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            bound.CancelAfter(ExchangeTimeout);
            var nonce = Guid.NewGuid().ToString("N");
            var command = new JsonObject
            {
                ["cmd"] = "SET_ACTIVITY",
                ["args"] = new JsonObject { ["pid"] = _pid, ["activity"] = activity?.DeepClone() },
                ["nonce"] = nonce,
            };
            await DiscordIpcFrame.WriteAsync(_pipe, DiscordOp.Frame, command.ToJsonString(), bound.Token).ConfigureAwait(false);
            while (true)
            {
                var (op, json) = await ReadAnswerAsync(bound.Token).ConfigureAwait(false);
                if (op != DiscordOp.Frame) continue;
                using var answer = JsonDocument.Parse(json);
                if (Text(answer.RootElement, "nonce") != nonce) continue;
                if (Text(answer.RootElement, "evt") == "ERROR") throw new DiscordIpcException("SET_ACTIVITY refused");
                return;
            }
        }
        finally
        {
            _exchange.Release();
        }
    }

    /// <summary>The next frame that is not a ping (answered with a pong); a close ends the connection.</summary>
    private async Task<(int Op, string Json)> ReadAnswerAsync(CancellationToken cancellation)
    {
        while (true)
        {
            var (op, json) = await DiscordIpcFrame.ReadAsync(_pipe, cancellation).ConfigureAwait(false);
            switch (op)
            {
                case DiscordOp.Ping:
                    await DiscordIpcFrame.WriteAsync(_pipe, DiscordOp.Pong, json, cancellation).ConfigureAwait(false);
                    continue;
                case DiscordOp.Close:
                    throw new DiscordIpcException("Discord closed the connection");
                default:
                    return (op, json);
            }
        }
    }

    private static string? Text(JsonElement root, string name) =>
        root.ValueKind == JsonValueKind.Object && root.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    public async ValueTask DisposeAsync()
    {
        await _pipe.DisposeAsync().ConfigureAwait(false);
        _exchange.Dispose();
    }
}
