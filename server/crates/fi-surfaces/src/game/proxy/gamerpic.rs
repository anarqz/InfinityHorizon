//! Player pictures for the Xbox Live profile settings (`GameDisplayPicRaw`): a generated
//! "identicon" per XUID, stable and never fetched from anywhere (ported from the Beta's
//! `server/src/gamerpic.rs`, without its admin-dropped files).
//!
//! The URL must be absolute: XSAPI turns it into a Uri, and an empty one crashes the game. Its
//! host is one the hook redirects like any other, so the picture comes back through the proxy.

/// The host the game is told to load pictures from (the Beta's; the hook forwards it).
pub const HOST: &str = "gamerpic.fh2online";

/// Side of the picture in pixels.
pub const SIZE: usize = 208;

/// The picture URL of `xuid`.
pub fn url_for(xuid: u64) -> String {
    format!("http://{HOST}/pic?xuid={xuid}")
}

/// A 5x5 mirrored pattern on a gradient, colours from the XUID, as a PNG.
pub fn identicon(xuid: u64, size: usize) -> Vec<u8> {
    let mut h = xuid ^ 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h
    };
    let hue = (next() % 360) as f32;
    let fg = hsl(hue, 0.75, 0.62);
    let bg_top = hsl((hue + 200.0) % 360.0, 0.45, 0.20);
    let bg_bottom = hsl((hue + 230.0) % 360.0, 0.50, 0.10);
    let bits = next();
    let cell = |x: usize, y: usize| {
        let col = if x < 3 { x } else { 4 - x };
        (bits >> (y * 3 + col)) & 1 == 1
    };
    let margin = size / 8;
    let grid = ((size - 2 * margin) / 5).max(1);
    let mut rgb = Vec::with_capacity(size * size * 3);
    for y in 0..size {
        let t = y as f32 / size as f32;
        let bg = [0, 1, 2].map(|i| (bg_top[i] as f32 * (1.0 - t) + bg_bottom[i] as f32 * t) as u8);
        for x in 0..size {
            let (gx, gy) = (x.wrapping_sub(margin) / grid, y.wrapping_sub(margin) / grid);
            let on = x >= margin && y >= margin && gx < 5 && gy < 5 && cell(gx, gy);
            rgb.extend_from_slice(if on { &fg } else { &bg });
        }
    }
    png_rgb(size, size, &rgb)
}

fn hsl(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r, g, b].map(|v| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// A minimal PNG: 8-bit RGB, zlib with stored (uncompressed) deflate blocks.
fn png_rgb(width: usize, height: usize, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width * 3 + 1) * height);
    for row in rgb.chunks(width * 3) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = u16::try_from(block.len()).unwrap_or(u16::MAX);
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&u32::try_from(width).unwrap_or(0).to_be_bytes());
    ihdr.extend_from_slice(&u32::try_from(height).unwrap_or(0).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr), (b"IDAT", &z), (b"IEND", &Vec::new())] {
        png.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }
    png
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_reference() {
        // The check value of CRC-32/ISO-HDLC.
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn the_picture_is_a_png_stable_per_xuid() {
        let png = identicon(2_535_100_000_000_001, 64);
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..24], &[0, 0, 0, 64, 0, 0, 0, 64]);
        assert!(png.ends_with(&[0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]));
        assert_eq!(png, identicon(2_535_100_000_000_001, 64));
        assert_ne!(png, identicon(2_535_100_000_000_002, 64));
        assert_eq!(
            url_for(2_535_100_000_000_001),
            "http://gamerpic.fh2online/pic?xuid=2535100000000001"
        );
    }
}
