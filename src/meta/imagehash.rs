//! 64-bit difference hash, tolerant to scale and compression, for recognizing
//! issue covers among a collection's pages.
//!
//! Decoding uses the Windows Imaging Component, which lets the JPEG decoder skip
//! most of the work by decoding straight to 1/8 size.

use windows::core::GUID;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat8bppGray, IWICBitmapFrameDecode, IWICBitmapSource,
    IWICBitmapSourceTransform, IWICImagingFactory, IWICPalette, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICBitmapTransformRotate0,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows_core::Interface;

const W: usize = 9;
const H: usize = 8;
/// Decode at roughly this fraction of full size.
const SCALE: u32 = 8;

thread_local! {
    static FACTORY: Option<IWICImagingFactory> = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| log!("WIC unavailable: {e}"))
            .ok()
    };
}

/// Collections often print a cover shrunk onto a decorated page, so pages are
/// hashed with several margins cropped off (fractions of width and height).
const INSETS_X: [f32; 4] = [0.0, 0.03, 0.055, 0.08];
const INSETS_Y: [f32; 4] = [0.0, 0.02, 0.035, 0.05];
pub const CROPS: usize = INSETS_X.len() * INSETS_Y.len();
pub type PageHashes = [u64; CROPS];

/// Hash of a whole image (a cover from the database).
pub fn dhash(bytes: &[u8]) -> Option<u64> {
    let (w, h, gray) = small_gray(bytes)?;
    Some(hash(&shrink(&gray, w, h, 0.0, 0.0)))
}

/// Hashes of a page under every crop.
pub fn page_hashes(bytes: &[u8]) -> Option<PageHashes> {
    let (w, h, gray) = small_gray(bytes)?;
    let mut out = [0u64; CROPS];
    for (i, (ix, iy)) in INSETS_X.iter().flat_map(|x| INSETS_Y.iter().map(move |y| (*x, *y))).enumerate() {
        out[i] = hash(&shrink(&gray, w, h, ix, iy));
    }
    Some(out)
}

/// Smallest distance between a cover and any crop of a page.
pub fn distance(page: &PageHashes, cover: u64) -> u32 {
    page.iter().map(|h| (h ^ cover).count_ones()).min().unwrap_or(64)
}

fn small_gray(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let truncated = first_progressive_scan(bytes);
    let bytes = truncated.as_deref().unwrap_or(bytes);
    FACTORY.with(|f| decode(f.as_ref()?, bytes))
}

fn hash(small: &[u32]) -> u64 {
    let mut hash = 0u64;
    for y in 0..H {
        for x in 0..W - 1 {
            hash = (hash << 1) | (small[y * W + x] < small[y * W + x + 1]) as u64;
        }
    }
    hash
}

/// A progressive JPEG's first scan holds the DC coefficients, which already form
/// a 1/8-scale image. Cutting the file after it skips ~95% of the decoding work.
/// Returns `None` for anything else, which is decoded as is.
fn first_progressive_scan(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut progressive = false;
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        match marker {
            0xC2 => progressive = true,
            0xDA => {
                if !progressive {
                    return None;
                }
                // Entropy-coded data ends at the first marker that is not a stuffed
                // byte or a restart marker.
                let mut j = i + 2 + len;
                while j + 1 < bytes.len() {
                    let next = bytes[j + 1];
                    if bytes[j] == 0xFF && next != 0 && !(0xD0..=0xD7).contains(&next) {
                        let mut out = bytes[..j].to_vec();
                        out.extend_from_slice(&[0xFF, 0xD9]);
                        return Some(out);
                    }
                    j += 1;
                }
                return None;
            }
            _ => {}
        }
        i += 2 + len;
    }
    None
}

/// Small grayscale version of an image.
fn decode(factory: &IWICImagingFactory, bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    unsafe {
        let stream = SHCreateMemStream(Some(bytes))?;
        let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut w, mut h) = (0, 0);
        frame.GetSize(&mut w, &mut h).ok()?;
        let (tw, th) = ((w / SCALE).max(W as u32), (h / SCALE).max(H as u32));
        native_scaled(&frame, tw, th).or_else(|| converted(factory, &frame, tw, th))
    }
}

/// Fast path: the decoder scales during decoding (JPEG DCT scaling).
unsafe fn native_scaled(frame: &IWICBitmapFrameDecode, tw: u32, th: u32) -> Option<(usize, usize, Vec<u8>)> {
    let transform: IWICBitmapSourceTransform = frame.cast().ok()?;
    let (mut w, mut h) = (tw, th);
    transform.GetClosestSize(&mut w, &mut h).ok()?;
    let mut format = GUID_WICPixelFormat8bppGray;
    transform.GetClosestPixelFormat(&mut format).ok()?;
    let bpp = bytes_per_pixel(&format)?;
    let stride = w * bpp as u32;
    let mut buf = vec![0u8; (stride * h) as usize];
    transform
        .CopyPixels(std::ptr::null(), w, h, &format, WICBitmapTransformRotate0, stride, &mut buf)
        .ok()?;
    let gray = if bpp == 1 { buf } else { buf.chunks_exact(bpp).map(|p| luma_bgr(p[0], p[1], p[2])).collect() };
    Some((w as usize, h as usize, gray))
}

/// Generic path for formats without native scaling (PNG, WebP, ...).
unsafe fn converted(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode, tw: u32, th: u32)
    -> Option<(usize, usize, Vec<u8>)>
{
    let scaler = factory.CreateBitmapScaler().ok()?;
    scaler.Initialize(frame, tw, th, WICBitmapInterpolationModeFant).ok()?;
    let converter = factory.CreateFormatConverter().ok()?;
    let source: IWICBitmapSource = scaler.cast().ok()?;
    converter
        .Initialize(
            &source,
            &GUID_WICPixelFormat8bppGray,
            WICBitmapDitherTypeNone,
            None::<&IWICPalette>,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
    let mut buf = vec![0u8; (tw * th) as usize];
    converter.CopyPixels(std::ptr::null(), tw, &mut buf).ok()?;
    Some((tw as usize, th as usize, buf))
}

fn bytes_per_pixel(format: &GUID) -> Option<usize> {
    use windows::Win32::Graphics::Imaging::{
        GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppBGR, GUID_WICPixelFormat32bppBGRA,
    };
    if *format == GUID_WICPixelFormat8bppGray {
        Some(1)
    } else if *format == GUID_WICPixelFormat24bppBGR {
        Some(3)
    } else if *format == GUID_WICPixelFormat32bppBGR || *format == GUID_WICPixelFormat32bppBGRA {
        Some(4)
    } else {
        None
    }
}

fn luma_bgr(b: u8, g: u8, r: u8) -> u8 {
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8
}

/// Box-filter downscale to W×H after cutting `inset_x`/`inset_y` off each side.
fn shrink(gray: &[u8], w: usize, h: usize, inset_x: f32, inset_y: f32) -> Vec<u32> {
    let (left, top) = ((w as f32 * inset_x) as usize, (h as f32 * inset_y) as usize);
    let (cw, ch) = (w - 2 * left, h - 2 * top);
    let mut out = vec![0u32; W * H];
    for (y, row) in out.chunks_mut(W).enumerate() {
        let (y0, y1) = (top + y * ch / H, top + ((y + 1) * ch / H).max(y * ch / H + 1));
        for (x, cell) in row.iter_mut().enumerate() {
            let (x0, x1) = (left + x * cw / W, left + ((x + 1) * cw / W).max(x * cw / W + 1));
            let mut sum = 0u32;
            for yy in y0..y1.min(h) {
                for xx in x0..x1.min(w) {
                    sum += gray[yy * w + xx] as u32;
                }
            }
            *cell = sum / ((y1 - y0) * (x1 - x0)) as u32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_hash() {
        // Brightness rising to the right sets every bit.
        let (w, h) = (90, 80);
        let gray: Vec<u8> = (0..w * h).map(|i| ((i % w) * 255 / w) as u8).collect();
        let small = shrink(&gray, w, h, 0.0, 0.0);
        assert!(small.windows(2).take(W - 1).all(|p| p[0] < p[1]));
    }

    #[test]
    fn crop_finds_inset_picture() {
        // A 90×80 picture framed by a 10px flat border on each side.
        let (w, h) = (110, 100);
        let picture = |x: usize, y: usize| ((x * 7 + y * 13) % 251) as u8;
        let framed: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if (10..100).contains(&x) && (10..90).contains(&y) { picture(x - 10, y - 10) } else { 128 }
            })
            .collect();
        let bare: Vec<u8> = (0..90 * 80).map(|i| picture(i % 90, i / 90)).collect();
        let cover = hash(&shrink(&bare, 90, 80, 0.0, 0.0));
        let whole = hash(&shrink(&framed, w, h, 0.0, 0.0));
        let cropped = hash(&shrink(&framed, w, h, 10.0 / 110.0, 0.1));
        assert!((cropped ^ cover).count_ones() < (whole ^ cover).count_ones());
    }
}
