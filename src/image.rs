//! Minimal image decoding and OCR-oriented preprocessing.
//!
//! Screenshots differ from scans: text is small (~96 DPI), often light-on-dark,
//! and selections are cropped tightly around it. Tesseract does best with dark
//! text on a white background, a line height of roughly 30–50px and a margin,
//! so we normalise to exactly that and hand it an uncompressed PGM.

use crate::util::Error;

pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

pub struct Prepared {
    pub pgm: Vec<u8>,
    pub psm: u8,
}

/// Target height, in pixels, of a text line after scaling.
const TARGET_LINE_PX: f32 = 40.0;
/// Upper bound on pixels fed to tesseract, to keep large selections fast.
const MAX_PIXELS: f32 = 16_000_000.0;
const PAD: usize = 16;

pub fn decode(bytes: &[u8]) -> Result<Gray, Error> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        decode_png(bytes)
    } else if bytes.starts_with(b"P5") || bytes.starts_with(b"P6") {
        decode_pnm(bytes)
    } else {
        Err(Error::msg("unsupported image format"))
    }
}

fn decode_png(bytes: &[u8]) -> Result<Gray, Error> {
    let err = |e: png::DecodingError| Error::msg(format!("bad PNG: {e}"));
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(err)?;
    let size = reader.output_buffer_size().ok_or_else(|| Error::msg("PNG too large"))?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(err)?;
    let (w, h, stride) = (info.width as usize, info.height as usize, info.line_size);

    let ch = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err(Error::msg("unexpected indexed PNG after expansion")),
    };
    let mut px = Vec::with_capacity(w * h);
    for row in buf.chunks(stride).take(h) {
        for p in row[..w * ch].chunks_exact(ch) {
            let (l, a) = match ch {
                1 => (p[0] as u32, 255),
                2 => (p[0] as u32, p[1] as u32),
                3 => (luma(p[0], p[1], p[2]), 255),
                _ => (luma(p[0], p[1], p[2]), p[3] as u32),
            };
            // Composite translucent pixels over white.
            px.push(((l * a + 255 * (255 - a)) / 255) as u8);
        }
    }
    Ok(Gray { w, h, px })
}

fn decode_pnm(bytes: &[u8]) -> Result<Gray, Error> {
    let bad = || Error::msg("bad PNM image");
    let color = bytes[1] == b'6';
    // Header: magic, width, height, maxval — whitespace separated, '#' comments.
    let mut pos = 2;
    let mut fields = [0usize; 3];
    for f in &mut fields {
        loop {
            match bytes.get(pos) {
                Some(b'#') => {
                    while bytes.get(pos).is_some_and(|&c| c != b'\n') {
                        pos += 1;
                    }
                }
                Some(c) if c.is_ascii_whitespace() => pos += 1,
                Some(_) => break,
                None => return Err(bad()),
            }
        }
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        *f = std::str::from_utf8(&bytes[start..pos]).ok().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    }
    pos += 1; // single whitespace byte before the raster
    let [w, h, maxval] = fields;
    if maxval == 0 || maxval > 255 {
        return Err(Error::msg("only 8-bit PNM images are supported"));
    }
    let ch = if color { 3 } else { 1 };
    let data = bytes.get(pos..pos + w * h * ch).ok_or_else(bad)?;
    let scale = |v: u8| (v as usize * 255 / maxval) as u8;
    let px = if color {
        data.chunks_exact(3).map(|p| luma(scale(p[0]), scale(p[1]), scale(p[2])) as u8).collect()
    } else {
        data.iter().map(|&v| scale(v)).collect()
    };
    Ok(Gray { w, h, px })
}

#[inline]
fn luma(r: u8, g: u8, b: u8) -> u32 {
    // ITU-R BT.601, integer approximation.
    (77 * r as u32 + 150 * g as u32 + 29 * b as u32) >> 8
}

pub fn prepare(mut img: Gray, user_scale: Option<f32>) -> Prepared {
    let mut hist = [0usize; 256];
    for &p in &img.px {
        hist[p as usize] += 1;
    }
    let n = img.px.len().max(1);
    let percentile = |hist: &[usize; 256], q: f64| {
        let target = (n as f64 * q) as usize;
        let mut acc = 0;
        hist.iter().position(|&c| {
            acc += c;
            acc > target
        })
        .unwrap_or(255)
    };

    // The background dominates the selection, so the median is its brightness.
    // Dark background → invert so text becomes dark on light.
    if percentile(&hist, 0.5) < 128 {
        img.px.iter_mut().for_each(|p| *p = 255 - *p);
        hist.reverse();
    }

    // Stretch contrast: background → white, darkest text → black.
    let hi = percentile(&hist, 0.5);
    let lo = percentile(&hist, 0.001);
    if hi > lo + 16 {
        let (lo, range) = (lo as i32, (hi - lo) as i32);
        let lut: Vec<u8> = (0..256).map(|v| ((v - lo) * 255 / range).clamp(0, 255) as u8).collect();
        img.px.iter_mut().for_each(|p| *p = lut[*p as usize]);
    }

    let (lines, line_h) = measure_lines(&img);

    let mut scale = user_scale.unwrap_or_else(|| match line_h {
        Some(h) => {
            let s = (TARGET_LINE_PX / h as f32).clamp(1.0, 4.0);
            if s < 1.25 { 1.0 } else { s }
        }
        None => 2.0,
    });
    let area = (img.w * img.h) as f32;
    if user_scale.is_none() && area * scale * scale > MAX_PIXELS {
        scale = (MAX_PIXELS / area).sqrt().max(1.0);
    }

    // A single short text line is recognised far better in single-line mode.
    let psm = match (lines, line_h) {
        (1, Some(h)) if h <= 100 => 7,
        _ => 3,
    };

    let img = if (scale - 1.0).abs() > 0.01 { resize(&img, scale) } else { img };
    Prepared { pgm: to_pgm_padded(&img, PAD), psm }
}

/// Count text lines via a horizontal projection profile and return
/// (number of lines, median line height).
fn measure_lines(img: &Gray) -> (usize, Option<usize>) {
    // Ignore isolated specks: a row needs a few dark pixels to count as text.
    let min_ink = (img.w / 400).max(1);
    let mut runs = Vec::new();
    let mut run = 0;
    for row in img.px.chunks_exact(img.w.max(1)) {
        let ink = row.iter().filter(|&&p| p < 128).count();
        if ink >= min_ink {
            run += 1;
        } else if run > 0 {
            runs.push(run);
            run = 0;
        }
    }
    if run > 0 {
        runs.push(run);
    }
    runs.retain(|&r| r >= 4);
    if runs.is_empty() {
        return (0, None);
    }
    let count = runs.len();
    runs.sort_unstable();
    (count, Some(runs[count / 2]))
}

/// Bilinear resize (pixel-centre aligned).
fn resize(src: &Gray, s: f32) -> Gray {
    let w = ((src.w as f32 * s).round() as usize).max(1);
    let h = ((src.h as f32 * s).round() as usize).max(1);
    let axis = |dst: usize, len: usize| -> Vec<(usize, usize, u32)> {
        let f = len as f32 / dst as f32;
        (0..dst)
            .map(|d| {
                let x = ((d as f32 + 0.5) * f - 0.5).clamp(0.0, (len - 1) as f32);
                let x0 = x as usize;
                (x0, (x0 + 1).min(len - 1), ((x - x0 as f32) * 256.0) as u32)
            })
            .collect()
    };
    let xs = axis(w, src.w);
    let ys = axis(h, src.h);
    let mut px = Vec::with_capacity(w * h);
    for &(y0, y1, wy) in &ys {
        let (r0, r1) = (&src.px[y0 * src.w..][..src.w], &src.px[y1 * src.w..][..src.w]);
        for &(x0, x1, wx) in &xs {
            let top = r0[x0] as u32 * (256 - wx) + r0[x1] as u32 * wx;
            let bot = r1[x0] as u32 * (256 - wx) + r1[x1] as u32 * wx;
            px.push(((top * (256 - wy) + bot * wy + (1 << 15)) >> 16) as u8);
        }
    }
    Gray { w, h, px }
}

fn to_pgm_padded(img: &Gray, pad: usize) -> Vec<u8> {
    let (w, h) = (img.w + 2 * pad, img.h + 2 * pad);
    let header = format!("P5\n{w} {h}\n255\n");
    let mut out = Vec::with_capacity(header.len() + w * h);
    out.extend_from_slice(header.as_bytes());
    out.resize(out.len() + w * pad, 255);
    for row in img.px.chunks_exact(img.w) {
        out.resize(out.len() + pad, 255);
        out.extend_from_slice(row);
        out.resize(out.len() + pad, 255);
    }
    out.resize(out.len() + w * pad, 255);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnm_roundtrip_and_invert() {
        // 4x2 white-on-black P6 image.
        let mut ppm = b"P6\n# comment\n4 2\n255\n".to_vec();
        ppm.extend(std::iter::repeat_n(0u8, 4 * 2 * 3));
        let img = decode(&ppm).unwrap();
        assert_eq!((img.w, img.h), (4, 2));
        let p = prepare(img, Some(1.0));
        assert!(p.pgm.starts_with(format!("P5\n{} {}\n255\n", 4 + 2 * PAD, 2 + 2 * PAD).as_bytes()));
        // Black background was inverted to white.
        assert!(p.pgm.ends_with(&[255, 255]));
    }
}
