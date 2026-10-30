//! Platform-independent image handling: wire limits, PNG `IHDR` guard and
//! Windows DIB <-> PNG conversion. Nothing here touches the OS clipboard.

use std::fmt;

/// Cap on the encoded PNG wire payload.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Receiver-side decompression-bomb guard.
pub const MAX_PIXELS: u64 = 64_000_000;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// Encoded PNG is larger than `MAX_IMAGE_BYTES`.
    TooLarge { bytes: usize },
    /// `width * height` exceeds `MAX_PIXELS`.
    TooManyPixels { width: u32, height: u32 },
    /// Not a PNG, or truncated before the IHDR dimensions.
    NotPng,
    /// A native format could not be converted.
    Conversion(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::TooLarge { bytes } => write!(f, "image too large ({bytes} bytes)"),
            ImageError::TooManyPixels { width, height } => {
                write!(f, "image too many pixels ({width}x{height})")
            }
            ImageError::NotPng => write!(f, "not a valid PNG"),
            ImageError::Conversion(m) => write!(f, "image conversion failed: {m}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Width and height from the IHDR chunk (bytes 16-23). Reads 24 bytes only.
pub fn png_dimensions(png: &[u8]) -> Result<(u32, u32), ImageError> {
    if png.len() < 24 || png[..8] != PNG_SIGNATURE || &png[12..16] != b"IHDR" {
        return Err(ImageError::NotPng);
    }
    let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
    Ok((w, h))
}

/// Size cap and IHDR megapixel guard; never decompresses.
pub fn validate_png(png: &[u8]) -> Result<(u32, u32), ImageError> {
    if png.len() > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge { bytes: png.len() });
    }
    let (width, height) = png_dimensions(png)?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ImageError::TooManyPixels { width, height });
    }
    Ok((width, height))
}

fn conv<E: fmt::Display>(e: E) -> ImageError {
    ImageError::Conversion(e.to_string())
}

/// Decode a PNG to 8-bit RGBA after `validate_png`.
pub fn png_to_rgba(png: &[u8]) -> Result<(u32, u32, Vec<u8>), ImageError> {
    let (w, h) = validate_png(png)?;
    let mut dec = png::Decoder::new(std::io::Cursor::new(png));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(conv)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| conv("size overflow"))?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(conv)?;
    let data = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return Err(conv("unexpanded palette")),
    };
    Ok((w, h, rgba))
}

pub fn rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(conv)?;
    w.write_image_data(rgba).map_err(conv)?;
    w.finish().map_err(conv)?;
    Ok(out)
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Convert a packed DIB (`CF_DIB` / `CF_DIBV5` payload) to PNG. Supports
/// 24 and 32 bpp, `BI_RGB` / `BI_BITFIELDS`, top-down and bottom-up. 32 bpp
/// keeps alpha only when an alpha mask is declared (V4/V5 headers).
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, ImageError> {
    let bad = || conv("unsupported or truncated DIB");
    let hdr = le32(dib, 0).ok_or_else(bad)? as usize;
    if hdr < 40 || dib.len() < hdr {
        return Err(bad());
    }
    let width = le32(dib, 4).ok_or_else(bad)? as i32;
    let height = le32(dib, 8).ok_or_else(bad)? as i32;
    let bpp = u16::from_le_bytes(dib.get(14..16).ok_or_else(bad)?.try_into().unwrap());
    let compression = le32(dib, 16).ok_or_else(bad)?;
    if width <= 0 || height == 0 || !(bpp == 24 || bpp == 32) {
        return Err(bad());
    }
    if compression != BI_RGB && compression != BI_BITFIELDS {
        return Err(bad());
    }
    let (w, h, top_down) = (width as u32, height.unsigned_abs(), height < 0);
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(ImageError::TooManyPixels {
            width: w,
            height: h,
        });
    }
    let mut off = hdr;
    let mut masks = [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF, 0];
    if compression == BI_BITFIELDS && bpp == 32 {
        if hdr >= 56 {
            for (i, m) in masks.iter_mut().enumerate() {
                *m = le32(dib, 40 + i * 4).ok_or_else(bad)?;
            }
        } else {
            for (i, m) in masks.iter_mut().take(3).enumerate() {
                *m = le32(dib, hdr + i * 4).ok_or_else(bad)?;
            }
            off += 12;
        }
    } else if hdr >= 56 && bpp == 32 && compression == BI_RGB {
        masks[3] = le32(dib, 52).ok_or_else(bad)?;
    }
    let bytes_pp = (bpp / 8) as usize;
    let stride = (w as usize * bytes_pp + 3) & !3;
    let pixels = dib.get(off..).ok_or_else(bad)?;
    if pixels.len() < stride * h as usize {
        return Err(bad());
    }
    let channel = |px: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 255;
        }
        let shift = mask.trailing_zeros();
        let max = u64::from(mask >> shift);
        ((u64::from((px & mask) >> shift) * 255) / max) as u8
    };
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h as usize {
        let src_row = if top_down { row } else { h as usize - 1 - row };
        let line = &pixels[src_row * stride..src_row * stride + w as usize * bytes_pp];
        for px in line.chunks_exact(bytes_pp) {
            if bpp == 24 {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            } else {
                let v = u32::from_le_bytes(px.try_into().unwrap());
                rgba.extend_from_slice(&[
                    channel(v, masks[0]),
                    channel(v, masks[1]),
                    channel(v, masks[2]),
                    channel(v, masks[3]),
                ]);
            }
        }
    }
    rgba_to_png(w, h, &rgba)
}

fn dib_header(size: u32, w: u32, h: u32, bpp: u16, compression: u32, image_size: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(size as usize);
    v.extend_from_slice(&size.to_le_bytes());
    v.extend_from_slice(&(w as i32).to_le_bytes());
    v.extend_from_slice(&(h as i32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&bpp.to_le_bytes());
    v.extend_from_slice(&compression.to_le_bytes());
    v.extend_from_slice(&image_size.to_le_bytes());
    v.extend_from_slice(&[0u8; 16]); // ppm x/y, colors used/important
    v
}

/// PNG to a `CF_DIBV5` payload: 32 bpp BGRA bottom-up with an alpha mask.
pub fn png_to_dibv5(png: &[u8]) -> Result<Vec<u8>, ImageError> {
    let (w, h, rgba) = png_to_rgba(png)?;
    let mut v = dib_header(124, w, h, 32, BI_BITFIELDS, w * h * 4);
    for m in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000] {
        v.extend_from_slice(&m.to_le_bytes());
    }
    v.extend_from_slice(&0x7352_4742u32.to_le_bytes()); // bV5CSType = LCS_sRGB
    v.resize(124, 0);
    v[108..112].copy_from_slice(&4u32.to_le_bytes()); // bV5Intent = LCS_GM_IMAGES
    for row in (0..h as usize).rev() {
        for px in rgba[row * w as usize * 4..(row + 1) * w as usize * 4].chunks_exact(4) {
            v.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Ok(v)
}

/// PNG to a `CF_DIB` payload: 24 bpp bottom-up, alpha composited on white
/// (legacy consumers ignore alpha; `CF_DIBV5` and `PNG` carry it).
pub fn png_to_dib(png: &[u8]) -> Result<Vec<u8>, ImageError> {
    let (w, h, rgba) = png_to_rgba(png)?;
    let stride = (w as usize * 3 + 3) & !3;
    let mut v = dib_header(40, w, h, 24, BI_RGB, (stride * h as usize) as u32);
    for row in (0..h as usize).rev() {
        let start = v.len();
        for px in rgba[row * w as usize * 4..(row + 1) * w as usize * 4].chunks_exact(4) {
            let a = u32::from(px[3]);
            let blend = |c: u8| ((u32::from(c) * a + 255 * (255 - a)) / 255) as u8;
            v.extend_from_slice(&[blend(px[2]), blend(px[1]), blend(px[0])]);
        }
        v.resize(start + stride, 0);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal PNG prefix: signature + IHDR header declaring `w` x `h`.
    /// Deliberately not a decodable image, to prove the guard never decodes.
    fn fake_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = PNG_SIGNATURE.to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v
    }

    #[test]
    fn reads_ihdr_dimensions() {
        assert_eq!(png_dimensions(&fake_header(1920, 1080)), Ok((1920, 1080)));
    }

    #[test]
    fn rejects_non_png_and_truncated() {
        assert_eq!(
            png_dimensions(b"not a png at all, definitely"),
            Err(ImageError::NotPng)
        );
        assert_eq!(
            png_dimensions(&fake_header(1, 1)[..20]),
            Err(ImageError::NotPng)
        );
        let mut wrong_chunk = fake_header(1, 1);
        wrong_chunk[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_dimensions(&wrong_chunk), Err(ImageError::NotPng));
    }

    #[test]
    fn megapixel_limit_is_inclusive_at_64mp() {
        assert!(validate_png(&fake_header(8000, 8000)).is_ok());
        assert_eq!(
            validate_png(&fake_header(8000, 8001)),
            Err(ImageError::TooManyPixels {
                width: 8000,
                height: 8001
            })
        );
    }

    #[test]
    fn huge_dimensions_do_not_overflow() {
        assert!(matches!(
            validate_png(&fake_header(u32::MAX, u32::MAX)),
            Err(ImageError::TooManyPixels { .. })
        ));
    }

    #[test]
    fn size_cap_is_10_mib_on_encoded_bytes() {
        let mut ok = fake_header(1, 1);
        ok.resize(MAX_IMAGE_BYTES, 0);
        assert!(validate_png(&ok).is_ok());
        ok.push(0);
        assert_eq!(
            validate_png(&ok),
            Err(ImageError::TooLarge {
                bytes: MAX_IMAGE_BYTES + 1
            })
        );
    }

    #[test]
    fn oversize_png_is_refused_before_decoding() {
        assert!(matches!(
            png_to_rgba(&fake_header(9000, 9000)),
            Err(ImageError::TooManyPixels { .. })
        ));
    }

    fn sample_rgba() -> (u32, u32, Vec<u8>) {
        // 2x2: red opaque, green half-alpha / blue opaque, transparent white
        (
            2,
            2,
            vec![
                255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 255, 255, 255, 0,
            ],
        )
    }

    #[test]
    fn png_roundtrip_preserves_pixels() {
        let (w, h, px) = sample_rgba();
        let png = rgba_to_png(w, h, &px).unwrap();
        assert_eq!(png_to_rgba(&png).unwrap(), (w, h, px));
    }

    #[test]
    fn dibv5_roundtrip_preserves_alpha() {
        let (w, h, px) = sample_rgba();
        let dib = png_to_dibv5(&rgba_to_png(w, h, &px).unwrap()).unwrap();
        assert_eq!(&dib[..4], &124u32.to_le_bytes());
        let back = dib_to_png(&dib).unwrap();
        assert_eq!(png_to_rgba(&back).unwrap(), (w, h, px));
    }

    #[test]
    fn dibv5_is_bottom_up_bgra() {
        let (w, h, px) = sample_rgba();
        let dib = png_to_dibv5(&rgba_to_png(w, h, &px).unwrap()).unwrap();
        // first stored row is the image's last row; first pixel is blue opaque
        assert_eq!(&dib[124..128], &[255, 0, 0, 255][..]);
    }

    #[test]
    fn plain_dib_is_24bpp_and_flattens_alpha_on_white() {
        let (w, h, px) = sample_rgba();
        let dib = png_to_dib(&rgba_to_png(w, h, &px).unwrap()).unwrap();
        assert_eq!(&dib[..4], &40u32.to_le_bytes());
        let back = png_to_rgba(&dib_to_png(&dib).unwrap()).unwrap().2;
        assert_eq!(&back[0..4], &[255, 0, 0, 255]);
        assert_eq!(&back[12..16], &[255, 255, 255, 255]);
    }

    #[test]
    fn legacy_32bpp_bi_rgb_without_alpha_mask_is_opaque() {
        let mut dib = dib_header(40, 1, 1, 32, BI_RGB, 4);
        dib.extend_from_slice(&[10, 20, 30, 0]); // BGRX
        let rgba = png_to_rgba(&dib_to_png(&dib).unwrap()).unwrap().2;
        assert_eq!(rgba, vec![30, 20, 10, 255]);
    }

    #[test]
    fn truncated_dib_is_a_conversion_error() {
        let mut dib = dib_header(40, 4, 4, 24, BI_RGB, 48);
        dib.extend_from_slice(&[0; 10]);
        assert!(matches!(dib_to_png(&dib), Err(ImageError::Conversion(_))));
        assert!(matches!(
            dib_to_png(&[1, 2]),
            Err(ImageError::Conversion(_))
        ));
    }
}
