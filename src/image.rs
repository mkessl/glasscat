//! Images a model can be shown: which kind a file is, its size in
//! pixels, and the base64 text it is sent as.

/// The largest image file sent: as base64 it stays under the 5 MB that
/// Anthropic takes; OpenAI and Google take more.
pub const MAX: usize = 3_750_000;

/// The media type of PNG, JPEG, GIF and WebP files, the kinds providers
/// take, by the bytes they start with.
pub fn kind(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Width and height in pixels, when the header says.
pub fn size(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    let le16 = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let le24 = |i: usize| {
        let s = b.get(i..i + 3)?;
        Some(s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16)
    };
    match kind(b)? {
        "image/png" => Some((be32(16)?, be32(20)?)),
        "image/gif" => Some((le16(6)?, le16(8)?)),
        "image/webp" => match b.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let s = b.get(21..25)?;
                let w = 1 + (s[0] as u32 | (s[1] as u32 & 0x3f) << 8);
                let h = 1 + (s[1] as u32 >> 6 | (s[2] as u32) << 2 | (s[3] as u32 & 0xf) << 10);
                Some((w, h))
            }
            b"VP8X" => Some((1 + le24(24)?, 1 + le24(27)?)),
            _ => None,
        },
        _ => {
            // JPEG: walk the segments to the frame header.
            let mut i = 2;
            loop {
                if *b.get(i)? != 0xff {
                    return None;
                }
                let m = *b.get(i + 1)?;
                if m == 0xff {
                    i += 1;
                    continue;
                }
                if matches!(m, 0xc0..=0xcf) && !matches!(m, 0xc4 | 0xc8 | 0xcc) {
                    return Some((be16(i + 7)?, be16(i + 5)?));
                }
                i += 2 + be16(i + 2)? as usize;
            }
        }
    }
}

/// What the model is told about an image file: "PNG image, 800×600, 120 KB".
pub fn describe(b: &[u8], mime: &str) -> String {
    let name = match mime {
        "image/jpeg" => "JPEG",
        "image/webp" => "WebP",
        m => &m[6..].to_uppercase(),
    };
    let px = size(b)
        .map(|(w, h)| format!(", {w}×{h}"))
        .unwrap_or_default();
    let kb = b.len().div_ceil(1024);
    let amount = if kb < 1024 {
        format!("{kb} KB")
    } else {
        format!("{:.1} MB", b.len() as f64 / (1 << 20) as f64)
    };
    format!("{name} image{px}, {amount}")
}

pub fn base64(b: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                out.push(ABC[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe]), "//4=");
    }

    #[test]
    fn reads_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend([0, 0, 3, 32, 0, 0, 2, 88]);
        assert_eq!(size(&png), Some((800, 600)));
        assert_eq!(describe(&png, "image/png"), "PNG image, 800×600, 1 KB");
        assert_eq!(size(b"GIF89a\x10\x00\x20\x00"), Some((16, 32)));
        // SOI, an APP0 segment of 4 bytes, then SOF0 with 40 high, 30 wide.
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 17, 8, 0, 40, 0, 30,
        ];
        assert_eq!(kind(&jpeg), Some("image/jpeg"));
        assert_eq!(size(&jpeg), Some((30, 40)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend([0; 8]);
        webp.extend([99, 0, 0, 49, 0, 0]);
        assert_eq!(size(&webp), Some((100, 50)));
        assert_eq!(kind(b"plain text"), None);
    }
}
