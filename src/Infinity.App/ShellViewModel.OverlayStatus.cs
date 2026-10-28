// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Infinity.Core.Overlay;

namespace Infinity.App;

/// <summary>
/// CR-L40 §1.3: the guide's bottom block, fed from here. The connected server's public status (its core
/// origin's <c>/core/v1/status</c>, read through the session's API client) and the linked Discord avatar,
/// decoded and scaled here to <see cref="UiFrame.AvatarSide"/> straight RGBA: the overlay never fetches anything.
/// </summary>
public sealed partial class ShellViewModel
{
    private OverlayServerStatusFeed? _overlayStatusFeed;

    /// <summary>A play session's feed (<see cref="_overlayStatusFeed"/>): the server's host and status read, and the avatar as it is now.</summary>
    private void StartOverlayStatusFeed(Infinity.Core.Auth.AccountSession session)
    {
        var api = session.Api;
        _overlayStatusFeed = new OverlayServerStatusFeed(api.Core.Authority, api.ServerStatusAsync, log: Log)
        {
            Avatar = OverlayAvatar(DiscordAvatar as BitmapSource),
        };
    }

    /// <summary>The Discord link or its avatar changed (or the account signed out): the overlay's avatar follows.</summary>
    private void OverlayAvatarChanged()
    {
        if (_overlayStatusFeed is { } feed) feed.Avatar = OverlayAvatar(DiscordAvatar as BitmapSource);
    }

    /// <summary>
    /// The avatar as the overlay draws it: the image's centre square scaled to <see cref="UiFrame.AvatarSide"/>,
    /// straight-alpha RGBA; <see cref="UiFrame.Avatar.None"/> (the initials) for no image or one that does not decode.
    /// </summary>
    internal static UiFrame.Avatar OverlayAvatar(BitmapSource? image)
    {
        if (image is null || image.PixelWidth <= 0 || image.PixelHeight <= 0) return UiFrame.Avatar.None;
        try
        {
            var side = Math.Min(image.PixelWidth, image.PixelHeight);
            BitmapSource square = new CroppedBitmap(image, new Int32Rect((image.PixelWidth - side) / 2, (image.PixelHeight - side) / 2, side, side));
            if (side != UiFrame.AvatarSide)
            {
                var scale = (double)UiFrame.AvatarSide / side;
                square = new TransformedBitmap(square, new ScaleTransform(scale, scale));
            }
            // Bgra32 is straight alpha (Pbgra32 would be premultiplied)
            var bgra = new FormatConvertedBitmap(square, PixelFormats.Bgra32, null, 0);
            var (width, height) = (bgra.PixelWidth, bgra.PixelHeight);
            if (width != height || width is 0 or > UiFrame.MaxAvatarSide) return UiFrame.Avatar.None;
            var pixels = new byte[width * height * 4];
            bgra.CopyPixels(pixels, width * 4, 0);
            for (var i = 0; i < pixels.Length; i += 4) (pixels[i], pixels[i + 2]) = (pixels[i + 2], pixels[i]);
            return new UiFrame.Avatar((ushort)width, (ushort)height, pixels);
        }
        catch (Exception error) when (error is IOException or NotSupportedException or InvalidOperationException or ArgumentException or FileFormatException)
        {
            return UiFrame.Avatar.None;
        }
    }
}
