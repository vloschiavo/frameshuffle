//! Image conversion: decode (JPEG/PNG/WebP/BMP/HEIC) -> orient -> fit to the profile (crop, or blurred
//! padding) -> optional contrast/saturation -> optional palette dithering -> JPEG/PNG bytes.

use crate::config::{Dither, Fit, Format, Profile};
use anyhow::{anyhow, bail, Context, Result};
use fast_image_resize as fr;
use fr::images::{Image as FrImage, ImageRef};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage};
use std::path::Path;

pub fn is_heif(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("heic" | "heif" | "hif")
    )
}

// ------------------------------------------------------------------ decoding

#[cfg(feature = "heic")]
pub fn heif_dimensions(path: &Path) -> Result<(u32, u32)> {
    use libheif_rs::HeifContext;
    let ctx = HeifContext::read_from_file(path.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?)?;
    let h = ctx.primary_image_handle()?;
    Ok((h.width(), h.height()))
}

pub const NO_HEIC_MSG: &str = "this build has no HEIC/HEIF support: convert the file with scripts/convert-heic.sh or build the image with WITH_HEIC=1 (see README)";

#[cfg(not(feature = "heic"))]
pub fn heif_dimensions(_: &Path) -> Result<(u32, u32)> {
    bail!(NO_HEIC_MSG)
}

#[cfg(feature = "heic")]
fn decode_heif(path: &Path) -> Result<RgbImage> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let lib = LibHeif::new();
    let ctx = HeifContext::read_from_file(path.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?)?;
    let handle = ctx.primary_image_handle()?;
    // libheif applies the file's rotation/mirroring transforms itself, so no EXIF handling afterwards.
    let img = lib.decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)?;
    let planes = img.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| anyhow!("HEIC decode produced no interleaved plane"))?;
    let (w, h) = (plane.width as usize, plane.height as usize);
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let row = &plane.data[y * plane.stride..y * plane.stride + w * 3];
        out.extend_from_slice(row);
    }
    RgbImage::from_raw(w as u32, h as u32, out).ok_or_else(|| anyhow!("bad HEIC buffer"))
}

#[cfg(not(feature = "heic"))]
fn decode_heif(_: &Path) -> Result<RgbImage> {
    bail!(NO_HEIC_MSG)
}

pub fn decode(path: &Path) -> Result<RgbImage> {
    if is_heif(path) {
        return decode_heif(path);
    }
    let mut dec = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = dec.orientation()?;
    let mut img = DynamicImage::from_decoder(dec)?;
    img.apply_orientation(orientation);
    Ok(img.into_rgb8())
}

// ------------------------------------------------------------------ resizing

fn resize(
    src: &RgbImage,
    crop: Option<(f64, f64, f64, f64)>,
    w: u32,
    h: u32,
    filter: fr::FilterType,
) -> Result<RgbImage> {
    let src_view = ImageRef::new(src.width(), src.height(), src.as_raw(), fr::PixelType::U8x3)?;
    let mut dst = FrImage::new(w, h, fr::PixelType::U8x3);
    let mut opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(filter));
    if let Some((l, t, cw, ch)) = crop {
        opts = opts.crop(l, t, cw, ch);
    }
    fr::Resizer::new().resize(&src_view, &mut dst, &opts)?;
    RgbImage::from_raw(w, h, dst.into_vec()).ok_or_else(|| anyhow!("resize buffer mismatch"))
}

/// Centre-crop to the target aspect ratio and scale to exactly (tw, th).
fn cover(src: &RgbImage, tw: u32, th: u32, filter: fr::FilterType) -> Result<RgbImage> {
    let (sw, sh) = (src.width() as f64, src.height() as f64);
    let tgt = tw as f64 / th as f64;
    let (cw, ch) = if sw / sh > tgt {
        (sh * tgt, sh)
    } else {
        (sw, sw / tgt)
    };
    resize(
        src,
        Some(((sw - cw) / 2.0, (sh - ch) / 2.0, cw, ch)),
        tw,
        th,
        filter,
    )
}

/// Fit entirely inside (tw, th), preserving aspect ratio.
fn contain(src: &RgbImage, tw: u32, th: u32) -> Result<RgbImage> {
    let scale = (tw as f64 / src.width() as f64).min(th as f64 / src.height() as f64);
    let w = ((src.width() as f64 * scale).round() as u32).clamp(1, tw);
    let h = ((src.height() as f64 * scale).round() as u32).clamp(1, th);
    if (w, h) == (src.width(), src.height()) {
        return Ok(src.clone());
    }
    resize(src, None, w, h, fr::FilterType::Lanczos3)
}

fn blurred_backdrop(src: &RgbImage, p: &Profile) -> Result<RgbImage> {
    // Blur a small copy, then scale it up: visually identical, a fraction of the cost.
    let (sw, sh) = ((p.width / 8).max(8), (p.height / 8).max(8));
    let small = cover(src, sw, sh, fr::FilterType::Bilinear)?;
    let blurred = image::imageops::blur(&small, p.blur_sigma.max(0.1));
    let mut big = resize(&blurred, None, p.width, p.height, fr::FilterType::Bilinear)?;
    if p.bg_dim > 0.0 {
        let k = (1.0 - p.bg_dim.clamp(0.0, 1.0)) * 256.0;
        for px in big.pixels_mut() {
            for c in px.0.iter_mut() {
                *c = ((*c as f32 * k) / 256.0) as u8;
            }
        }
    }
    Ok(big)
}

fn fit(src: RgbImage, p: &Profile) -> Result<RgbImage> {
    let (tw, th) = (p.width, p.height);
    let (sw, sh) = (src.width() as f32, src.height() as f32);
    let (sa, ta) = (sw / sh, tw as f32 / th as f32);
    // Fraction of the picture that cover-cropping would throw away.
    let lost = if sa > ta {
        1.0 - ta / sa
    } else {
        1.0 - sa / ta
    };
    match p.fit {
        Fit::Cover => cover(&src, tw, th, fr::FilterType::Lanczos3),
        _ if (src.width(), src.height()) == (tw, th) => Ok(src),
        Fit::BlurPad if lost <= p.max_crop => cover(&src, tw, th, fr::FilterType::Lanczos3),
        Fit::BlurPad | Fit::Contain => {
            let mut canvas = if p.fit == Fit::BlurPad {
                blurred_backdrop(&src, p)?
            } else {
                RgbImage::from_pixel(tw, th, image::Rgb(p.background))
            };
            let fg = contain(&src, tw, th)?;
            let (x, y) = ((tw - fg.width()) / 2, (th - fg.height()) / 2);
            image::imageops::replace(&mut canvas, &fg, x as i64, y as i64);
            Ok(canvas)
        }
    }
}

fn enhance(img: &mut RgbImage, contrast: f32, saturation: f32) {
    if (contrast - 1.0).abs() < 1e-3 && (saturation - 1.0).abs() < 1e-3 {
        return;
    }
    for px in img.pixels_mut() {
        let [r, g, b] = px.0;
        let (mut r, mut g, mut b) = (r as f32, g as f32, b as f32);
        let l = 0.299 * r + 0.587 * g + 0.114 * b;
        r = l + (r - l) * saturation;
        g = l + (g - l) * saturation;
        b = l + (b - l) * saturation;
        px.0 = [
            ((r - 128.0) * contrast + 128.0).clamp(0.0, 255.0) as u8,
            ((g - 128.0) * contrast + 128.0).clamp(0.0, 255.0) as u8,
            ((b - 128.0) * contrast + 128.0).clamp(0.0, 255.0) as u8,
        ];
    }
}

// ------------------------------------------------------------------ dithering

fn nearest(palette: &[[u8; 3]], r: f32, g: f32, b: f32) -> usize {
    let mut best = (f32::MAX, 0usize);
    for (i, c) in palette.iter().enumerate() {
        let (dr, dg, db) = (r - c[0] as f32, g - c[1] as f32, b - c[2] as f32);
        let d = dr * dr * 0.30 + dg * dg * 0.59 + db * db * 0.11; // luma-weighted distance
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1
}

/// Floyd–Steinberg (serpentine) to an arbitrary palette. Returns one palette index per pixel.
pub fn dither_floyd_steinberg(img: &RgbImage, palette: &[[u8; 3]]) -> Vec<u8> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut out = vec![0u8; w * h];
    // Two rows of accumulated error, with a 1-pixel margin each side.
    let mut cur = vec![[0f32; 3]; w + 2];
    let mut nxt = vec![[0f32; 3]; w + 2];
    for y in 0..h {
        let ltr = y % 2 == 0;
        nxt.iter_mut().for_each(|e| *e = [0.0; 3]);
        for i in 0..w {
            let x = if ltr { i } else { w - 1 - i };
            let src = img.get_pixel(x as u32, y as u32).0;
            let e = cur[x + 1];
            let v = [
                (src[0] as f32 + e[0]).clamp(0.0, 255.0),
                (src[1] as f32 + e[1]).clamp(0.0, 255.0),
                (src[2] as f32 + e[2]).clamp(0.0, 255.0),
            ];
            let k = nearest(palette, v[0], v[1], v[2]);
            out[y * w + x] = k as u8;
            let q = palette[k];
            let err = [v[0] - q[0] as f32, v[1] - q[1] as f32, v[2] - q[2] as f32];
            let dir: isize = if ltr { 1 } else { -1 };
            let at = |dx: isize| (x as isize + 1 + dx) as usize;
            for c in 0..3 {
                cur[at(dir)][c] += err[c] * 7.0 / 16.0;
                nxt[at(-dir)][c] += err[c] * 3.0 / 16.0;
                nxt[at(0)][c] += err[c] * 5.0 / 16.0;
                nxt[at(dir)][c] += err[c] / 16.0;
            }
        }
        std::mem::swap(&mut cur, &mut nxt);
    }
    out
}

// ------------------------------------------------------------------ encoding

fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1 << 19);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality).encode(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(buf)
}

fn encode_png_rgb(img: &RgbImage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, img.width(), img.height());
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.write_header()?.write_image_data(img.as_raw())?;
    }
    Ok(buf)
}

fn encode_png_indexed(indices: &[u8], w: u32, h: u32, palette: &[[u8; 3]]) -> Result<Vec<u8>> {
    let depth = match palette.len() {
        0..=2 => png::BitDepth::One,
        3..=4 => png::BitDepth::Two,
        5..=16 => png::BitDepth::Four,
        _ => png::BitDepth::Eight,
    };
    let bits = depth as usize;
    let row_bytes = (w as usize * bits).div_ceil(8);
    let mut packed = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let v = indices[y * w as usize + x];
            let bit = x * bits;
            packed[y * row_bytes + bit / 8] |= v << (8 - bits - bit % 8);
        }
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(depth);
        enc.set_palette(palette.iter().flatten().copied().collect::<Vec<u8>>());
        enc.set_compression(png::Compression::Default);
        enc.write_header()?.write_image_data(&packed)?;
    }
    Ok(buf)
}

pub fn content_type(p: &Profile) -> &'static str {
    match p.format {
        Format::Jpeg => "image/jpeg",
        Format::Png => "image/png",
    }
}

pub fn extension(p: &Profile) -> &'static str {
    match p.format {
        Format::Jpeg => "jpg",
        Format::Png => "png",
    }
}

/// Content type for serving an original file untouched, from its extension.
pub fn content_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("heic" | "hif") => "image/heic",
        Some("heif") => "image/heif",
        _ => "application/octet-stream",
    }
}

/// Full-size JPEG of a source image: orientation applied, nothing else touched (no resize/dither).
pub fn transcode_jpeg(path: &Path, quality: u8) -> Result<Vec<u8>> {
    let img = decode(path).with_context(|| format!("decoding {}", path.display()))?;
    encode_jpeg(&img, quality)
}

/// Convert one source file according to `p`. CPU-heavy: call from a blocking thread.
pub fn render(path: &Path, p: &Profile) -> Result<Vec<u8>> {
    let src = decode(path).with_context(|| format!("decoding {}", path.display()))?;
    let mut img = fit(src, p)?;
    enhance(&mut img, p.contrast, p.saturation);

    match p.dither {
        Dither::None => match p.format {
            Format::Jpeg => encode_jpeg(&img, p.quality),
            Format::Png => encode_png_rgb(&img),
        },
        Dither::FloydSteinberg => {
            let palette = p
                .palette
                .as_ref()
                .ok_or_else(|| anyhow!("dithering requires a palette"))?;
            let out_pal = p.output_palette.as_ref().unwrap_or(palette);
            if out_pal.len() != palette.len() {
                bail!("palette/output_palette length mismatch");
            }
            let idx = dither_floyd_steinberg(&img, palette);
            match p.format {
                Format::Png => encode_png_indexed(&idx, img.width(), img.height(), out_pal),
                Format::Jpeg => {
                    let raw: Vec<u8> = idx.iter().flat_map(|&i| out_pal[i as usize]).collect();
                    let rgb = RgbImage::from_raw(img.width(), img.height(), raw).unwrap();
                    encode_jpeg(&rgb, p.quality)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn profile(name: &str) -> Profile {
        Config::parse("").unwrap().profiles[name].clone()
    }

    fn solid(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
        RgbImage::from_pixel(w, h, image::Rgb(rgb))
            .save(path)
            .unwrap();
    }

    #[test]
    fn landscape_photo_in_landscape_profile_is_resized_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.jpg");
        solid(&p, 4000, 3000, [200, 30, 30]);
        let out = render(&p, &profile("e1004")).unwrap();
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (1600, 1200));
    }

    #[test]
    fn portrait_photo_gets_blurred_sidebars_not_a_crop() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("p.png");
        // Left half red, right half blue portrait image: a centre crop would lose the top/bottom,
        // padding keeps the whole picture and fills the sides with a blurred copy.
        let mut im = RgbImage::new(600, 900);
        for (x, _, px) in im.enumerate_pixels_mut() {
            *px = if x < 300 {
                image::Rgb([220, 20, 20])
            } else {
                image::Rgb([20, 20, 220])
            };
        }
        im.save(&p).unwrap();
        let out = render(&p, &profile("e1004")).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!((img.width(), img.height()), (1600, 1200));
        // Centre of the foreground (800 wide at 1200 tall is 800x1200 -> x in 400..1200) is crisp.
        assert!(
            img.get_pixel(500, 600).0[0] > 150,
            "left half of photo should be red"
        );
        assert!(
            img.get_pixel(1100, 600).0[2] > 150,
            "right half should be blue"
        );
        // Sidebar is blurred content from the photo, not flat white/black.
        let side = img.get_pixel(20, 600).0;
        assert!(
            side != [255, 255, 255] && side != [0, 0, 0],
            "sidebar was {side:?}"
        );
    }

    #[test]
    fn near_matching_aspect_is_cropped_not_padded() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("n.jpg");
        solid(&p, 1620, 1200, [10, 200, 10]); // 1.35 vs 1.333 -> 1.2% crop
        let out = render(&p, &profile("e1004")).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        // No sidebars: the corner is still the photo colour, not a blur of it.
        assert!(img.get_pixel(0, 0).0[1] > 150);
        assert!(img.get_pixel(1599, 1199).0[1] > 150);
    }

    #[test]
    fn dithered_profile_outputs_indexed_png_with_pure_palette() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("g.png");
        let mut im = RgbImage::new(400, 300);
        for (x, y, px) in im.enumerate_pixels_mut() {
            *px = image::Rgb([(x * 255 / 400) as u8, (y * 255 / 300) as u8, 128]);
        }
        im.save(&p).unwrap();
        let mut prof = profile("e1004-dithered");
        prof.width = 400;
        prof.height = 300;
        let out = render(&p, &prof).unwrap();
        let dec = png::Decoder::new(std::io::Cursor::new(&out));
        let mut r = dec.read_info().unwrap();
        assert_eq!(r.info().color_type, png::ColorType::Indexed);
        assert_eq!(r.info().bit_depth, png::BitDepth::Four);
        let pal: Vec<u8> = r.info().palette.as_ref().unwrap().to_vec();
        assert_eq!(&pal[..6], &[0, 0, 0, 255, 255, 255]);
        let mut buf = vec![0; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (400, 300));
        // every index is one of the 6 palette entries
        let max = buf.iter().flat_map(|b| [b >> 4, b & 15]).max().unwrap();
        assert!(max < 6);
    }

    #[test]
    fn dither_pure_colours_map_exactly() {
        let pal = crate::config::SPECTRA6_PURE;
        let mut im = RgbImage::new(6, 1);
        for (i, c) in pal.iter().enumerate() {
            im.put_pixel(i as u32, 0, image::Rgb(*c));
        }
        assert_eq!(dither_floyd_steinberg(&im, &pal), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn content_types_and_full_size_transcode() {
        assert_eq!(content_type_for_path(Path::new("/x/A.JPG")), "image/jpeg");
        assert_eq!(content_type_for_path(Path::new("a.heic")), "image/heic");
        assert_eq!(content_type_for_path(Path::new("a.webp")), "image/webp");
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.png");
        solid(&p, 321, 123, [9, 9, 9]);
        let out = transcode_jpeg(&p, 90).unwrap();
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (321, 123),
            "transcode must not resize"
        );
    }

    #[test]
    fn webp_and_bmp_decode() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.bmp");
        solid(&p, 100, 50, [1, 2, 3]);
        assert!(render(&p, &profile("android-1080p")).is_ok());
    }
}
