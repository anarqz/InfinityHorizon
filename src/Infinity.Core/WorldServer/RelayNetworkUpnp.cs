// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Xml;
using System.Xml.Linq;

namespace Infinity.Core.WorldServer;

internal sealed record UpnpGateway(Uri Control, string Service, IPAddress Local);
internal sealed record UpnpMapping(RelayUpnpState State, IPEndPoint? Address = null, uint LeaseSeconds = 0);

/// <summary>Bounded SSDP/IGD adapter. Router traffic only reaches local addresses; XML cannot resolve entities.</summary>
internal static class RelayNetworkUpnp
{
    internal const uint LeaseSeconds = 1800;
    internal const int MaxHttpBytes = 256 * 1024;
    private static readonly TimeSpan RequestTimeout = TimeSpan.FromSeconds(3);
    private static readonly string[] Services = ["urn:schemas-upnp-org:service:WANIPConnection:2", "urn:schemas-upnp-org:service:WANIPConnection:1", "urn:schemas-upnp-org:service:WANPPPConnection:1"];
    private static readonly string[] SearchFor = ["urn:schemas-upnp-org:device:InternetGatewayDevice:1", Services[1], Services[2]];

    internal static string? SsdpLocation(string answer)
    {
        var lines = answer.Split('\n');
        if (lines.Length == 0 || !lines[0].StartsWith("HTTP/1.", StringComparison.OrdinalIgnoreCase)
            || !lines[0].Contains(" 200", StringComparison.Ordinal)) return null;
        foreach (var line in lines.Skip(1))
        {
            var split = line.IndexOf(':');
            if (split > 0 && line[..split].Trim().Equals("location", StringComparison.OrdinalIgnoreCase))
                return line[(split + 1)..].Trim();
        }
        return null;
    }

    private static XDocument Xml(string xml)
    {
        using var text = new StringReader(xml);
        using var reader = XmlReader.Create(text, new XmlReaderSettings { DtdProcessing = DtdProcessing.Prohibit, XmlResolver = null, MaxCharactersInDocument = MaxHttpBytes });
        return XDocument.Load(reader);
    }

    internal static (string Service, Uri Control)? FindService(string description, Uri location)
    {
        try
        {
            var xml = Xml(description);
            var baseText = xml.Descendants().FirstOrDefault(e => e.Name.LocalName == "URLBase")?.Value.Trim();
            var root = Uri.TryCreate(baseText, UriKind.Absolute, out var specified) && ValidUri(specified) ? specified : location;
            foreach (var service in Services)
                foreach (var entry in xml.Descendants().Where(e => e.Name.LocalName == "service"))
                {
                    var kind = entry.Elements().FirstOrDefault(e => e.Name.LocalName == "serviceType")?.Value.Trim();
                    var control = entry.Elements().FirstOrDefault(e => e.Name.LocalName == "controlURL")?.Value.Trim();
                    if (!string.Equals(kind, service, StringComparison.OrdinalIgnoreCase) || string.IsNullOrEmpty(control)) continue;
                    // Rust treats a relative control path as root-relative.
                    var path = Uri.TryCreate(control, UriKind.Absolute, out var absolute) ? absolute : new Uri(root, control.StartsWith('/') ? control : "/" + control);
                    if (ValidUri(path)) return (service, path);
                }
        }
        catch (Exception exception) when (exception is XmlException or UriFormatException) { }
        return null;
    }

    internal static string? SoapValue(string xml, string name)
    {
        try { return Xml(xml).Descendants().FirstOrDefault(e => e.Name.LocalName == name)?.Value.Trim(); }
        catch (XmlException) { return null; }
    }

    internal static string SoapBody(string service, string action, IEnumerable<(string Name, string Value)> arguments)
    {
        XNamespace envelope = "http://schemas.xmlsoap.org/soap/envelope/";
        XNamespace kind = service;
        var content = new XElement(kind + action, arguments.Select(pair => new XElement(pair.Name, pair.Value)));
        content.Add(new XAttribute(XNamespace.Xmlns + "u", service));
        return new XElement(envelope + "Envelope", new XAttribute(XNamespace.Xmlns + "s", envelope),
            new XAttribute(envelope + "encodingStyle", "http://schemas.xmlsoap.org/soap/encoding/"), new XElement(envelope + "Body", content)).ToString(SaveOptions.DisableFormatting);
    }

    internal static bool ValidUri(Uri uri) => uri.IsAbsoluteUri && uri.Scheme == Uri.UriSchemeHttp
        && string.IsNullOrEmpty(uri.UserInfo) && uri.Port is > 0 and <= 65535 && string.IsNullOrEmpty(uri.Fragment);

    internal static (int Status, byte[] Body)? HttpSplit(ReadOnlySpan<byte> raw)
    {
        var headerEnd = raw.IndexOf("\r\n\r\n"u8);
        if (headerEnd < 0 || raw.Length > MaxHttpBytes) return null;
        var header = Encoding.ASCII.GetString(raw[..headerEnd]);
        var lines = header.Split("\r\n", StringSplitOptions.None);
        var first = lines[0].Split(' ');
        if (first.Length < 2 || !first[0].StartsWith("HTTP/1.", StringComparison.Ordinal) || !int.TryParse(first[1], CultureInfo.InvariantCulture, out var status)) return null;
        var body = raw[(headerEnd + 4)..];
        var chunked = false;
        int? contentLength = null;
        foreach (var line in lines.Skip(1))
        {
            var at = line.IndexOf(':');
            if (at <= 0) continue;
            var name = line[..at].Trim(); var value = line[(at + 1)..].Trim();
            if (name.Equals("Transfer-Encoding", StringComparison.OrdinalIgnoreCase))
            {
                if (!value.Equals("chunked", StringComparison.OrdinalIgnoreCase)) return null;
                chunked = true;
            }
            if (name.Equals("Content-Length", StringComparison.OrdinalIgnoreCase))
            {
                if (!int.TryParse(value, CultureInfo.InvariantCulture, out var length) || length is < 0 or > MaxHttpBytes) return null;
                if (contentLength is { } previous && previous != length) return null;
                contentLength = length;
            }
        }
        if (!chunked) return contentLength is { } expected && body.Length < expected ? null : (status, body[..(contentLength ?? body.Length)].ToArray());
        using var decoded = new MemoryStream();
        var offset = 0;
        while (offset < body.Length)
        {
            var end = body[offset..].IndexOf("\r\n"u8);
            if (end < 0) return null;
            var size = Encoding.ASCII.GetString(body.Slice(offset, end)).Split(';')[0];
            if (!int.TryParse(size, NumberStyles.HexNumber, CultureInfo.InvariantCulture, out var count) || count < 0 || count > MaxHttpBytes - decoded.Length) return null;
            offset += end + 2;
            if (count == 0) return body.Length - offset >= 2 && body.Slice(offset, 2).SequenceEqual("\r\n"u8) ? (status, decoded.ToArray()) : null;
            if (count > body.Length - offset - 2 || !body.Slice(offset + count, 2).SequenceEqual("\r\n"u8)) return null;
            decoded.Write(body.Slice(offset, count)); offset += count + 2;
        }
        return null;
    }

    private static async Task<(int Status, string Body, IPAddress Local)> HttpAsync(Uri uri, string method, string? soapAction, string body, CancellationToken cancellation)
    {
        if (!ValidUri(uri)) throw new IOException("Unsupported router URL");
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        timeout.CancelAfter(RequestTimeout);
        var addresses = await Dns.GetHostAddressesAsync(uri.DnsSafeHost, timeout.Token).ConfigureAwait(false);
        var address = addresses.FirstOrDefault(RelayNetwork.RouterAddress) ?? throw new IOException("Router URL has no local address");
        using var client = new TcpClient(address.AddressFamily);
        await client.ConnectAsync(address, uri.Port, timeout.Token).ConfigureAwait(false);
        var local = ((IPEndPoint)client.Client.LocalEndPoint!).Address;
        var bytes = Encoding.UTF8.GetBytes(body);
        var extra = soapAction is null ? string.Empty : "Content-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"" + soapAction + "\"\r\n";
        var header = Encoding.ASCII.GetBytes(string.Create(CultureInfo.InvariantCulture,
            $"{method} {uri.PathAndQuery} HTTP/1.1\r\nHost: {uri.Authority}\r\nConnection: close\r\nUser-Agent: InfinityLauncher\r\n{extra}Content-Length: {bytes.Length}\r\n\r\n"));
        var stream = client.GetStream();
        await stream.WriteAsync(header, timeout.Token).ConfigureAwait(false);
        await stream.WriteAsync(bytes, timeout.Token).ConfigureAwait(false);
        using var response = new MemoryStream();
        var buffer = new byte[8192];
        while (true)
        {
            var length = await stream.ReadAsync(buffer, timeout.Token).ConfigureAwait(false);
            if (length == 0) break;
            if (response.Length + length > MaxHttpBytes) throw new IOException("Router response limit");
            response.Write(buffer, 0, length);
            // Some IGDs keep the TCP connection alive despite Connection: close. Honor a complete
            // framed response immediately instead of discarding it at the request deadline.
            var raw = response.GetBuffer().AsSpan(0, checked((int)response.Length));
            var headerEnd = raw.IndexOf("\r\n\r\n"u8);
            if (headerEnd >= 0)
            {
                var responseHeader = Encoding.ASCII.GetString(raw[..headerEnd]);
                var framed = responseHeader.Split("\r\n", StringSplitOptions.None).Any(line =>
                    line.StartsWith("Content-Length:", StringComparison.OrdinalIgnoreCase)
                    || line.StartsWith("Transfer-Encoding:", StringComparison.OrdinalIgnoreCase));
                if (framed && HttpSplit(raw) is { } complete) return (complete.Status, Encoding.UTF8.GetString(complete.Body), local);
            }
        }
        var parsed = HttpSplit(response.ToArray()) ?? throw new IOException("Malformed router response");
        return (parsed.Status, Encoding.UTF8.GetString(parsed.Body), local);
    }

    internal static async Task<UpnpGateway?> FindGatewayAsync(TimeSpan wait, CancellationToken cancellation)
    {
        using var socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        socket.Bind(new IPEndPoint(IPAddress.Any, 0));
        socket.SetSocketOption(SocketOptionLevel.IP, SocketOptionName.MulticastTimeToLive, 2);
        var ssdp = new IPEndPoint(IPAddress.Parse("239.255.255.250"), 1900);
        foreach (var service in SearchFor)
        {
            var search = Encoding.ASCII.GetBytes($"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {service}\r\n\r\n");
            await socket.SendToAsync(search, SocketFlags.None, ssdp, cancellation).ConfigureAwait(false);
        }
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        timeout.CancelAfter(wait);
        var found = new HashSet<string>(StringComparer.Ordinal);
        var buffer = new byte[2048];
        try
        {
            while (found.Count < 8)
            {
                var packet = await socket.ReceiveFromAsync(buffer, SocketFlags.None, new IPEndPoint(IPAddress.Any, 0), timeout.Token).ConfigureAwait(false);
                if (!RelayNetwork.RouterAddress(((IPEndPoint)packet.RemoteEndPoint).Address)) continue;
                if (SsdpLocation(Encoding.ASCII.GetString(buffer, 0, packet.ReceivedBytes)) is { } location) found.Add(location);
            }
        }
        catch (OperationCanceledException) when (!cancellation.IsCancellationRequested) { }
        foreach (var location in found)
        {
            if (!Uri.TryCreate(location, UriKind.Absolute, out var uri) || !ValidUri(uri)) continue;
            try
            {
                var response = await HttpAsync(uri, "GET", null, string.Empty, cancellation).ConfigureAwait(false);
                if (response.Status == 200 && FindService(response.Body, uri) is { } service)
                    return new(service.Control, service.Service, response.Local);
            }
            catch (Exception exception) when (exception is IOException or SocketException or OperationCanceledException)
            { cancellation.ThrowIfCancellationRequested(); }
        }
        return null;
    }

    private static Task<(int Status, string Body, IPAddress Local)> SoapAsync(UpnpGateway gateway, string action,
        (string Name, string Value)[] arguments, CancellationToken cancellation) => HttpAsync(gateway.Control, "POST", gateway.Service + "#" + action,
            SoapBody(gateway.Service, action, arguments), cancellation);

    internal static async Task<UpnpMapping> MapAsync(UpnpGateway gateway, ushort port, CancellationToken cancellation)
    {
        for (var offset = 0; offset <= 4 && port + offset <= ushort.MaxValue; offset++)
            foreach (var lease in new uint[] { LeaseSeconds, 0 })
            {
                var external = checked((ushort)(port + offset));
                var response = await SoapAsync(gateway, "AddPortMapping", [
                    ("NewRemoteHost", string.Empty), ("NewExternalPort", external.ToString(CultureInfo.InvariantCulture)), ("NewProtocol", "UDP"),
                    ("NewInternalPort", port.ToString(CultureInfo.InvariantCulture)), ("NewInternalClient", gateway.Local.ToString()), ("NewEnabled", "1"),
                    ("NewPortMappingDescription", "Infinity world"), ("NewLeaseDuration", lease.ToString(CultureInfo.InvariantCulture))], cancellation).ConfigureAwait(false);
                if (response.Status == 200)
                {
                    var answer = await SoapAsync(gateway, "GetExternalIPAddress", [], cancellation).ConfigureAwait(false);
                    return answer.Status == 200 && IPAddress.TryParse(SoapValue(answer.Body, "NewExternalIPAddress"), out var ip)
                        ? new(RelayUpnpState.Mapped, new(ip, external), lease) : new(RelayUpnpState.Failed);
                }
                var code = SoapValue(response.Body, "errorCode");
                if (code == "725" && lease != 0) continue;
                if (code == "718") break;
                return new(RelayUpnpState.Failed);
            }
        return new(RelayUpnpState.Failed);
    }

    internal static async Task RunAsync(ushort port, Action<UpnpMapping> report, CancellationToken cancellation,
        Func<CancellationToken, Task<UpnpGateway?>>? discover = null, Func<TimeSpan, CancellationToken, Task>? delay = null)
    {
        discover ??= static token => FindGatewayAsync(TimeSpan.FromSeconds(3), token);
        delay ??= static (duration, token) => Task.Delay(duration, token);
        while (!cancellation.IsCancellationRequested)
        {
            try
            {
                var gateway = await discover(cancellation).ConfigureAwait(false);
                if (gateway is null) report(new(RelayUpnpState.NotFound));
                else
                    while (!cancellation.IsCancellationRequested)
                    {
                        var mapping = await MapAsync(gateway, port, cancellation).ConfigureAwait(false);
                        report(mapping);
                        if (mapping.State != RelayUpnpState.Mapped) break;
                        await delay(TimeSpan.FromSeconds(900), cancellation).ConfigureAwait(false);
                    }
            }
            catch (Exception exception) when (exception is IOException or SocketException or OperationCanceledException)
            { if (cancellation.IsCancellationRequested) return; report(new(RelayUpnpState.Failed)); }
            try { await delay(TimeSpan.FromSeconds(300), cancellation).ConfigureAwait(false); }
            catch (OperationCanceledException) { return; }
        }
    }
}
