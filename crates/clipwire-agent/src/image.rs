//! Картинки: буфер ОС отдаёт пиксели RGBA, по сети и в истории они живут в PNG.

use std::io::Cursor;

use anyhow::{Context, Result};
use clipwire_core::Content;
use image::{ImageBuffer, ImageEncoder, Rgba, RgbaImage};

/// Больше этого (в пикселях) не берём: такие «картинки» — обычно ошибка
/// или скриншот нескольких мониторов в 8K, и в буфер обмена их таскать незачем.
pub const MAX_PIXELS: u64 = 64_000_000;

/// Длинная сторона миниатюры для списка в окне вставки. С запасом под экраны
/// с масштабом больше 100%.
pub const THUMB_SIDE: u32 = 320;

/// RGBA из буфера → PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        (width as u64) * (height as u64) <= MAX_PIXELS,
        "картинка {width}×{height} слишком большая"
    );
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
    .context("не удалось закодировать PNG")?;
    Ok(out)
}

/// PNG → RGBA для записи в буфер.
pub fn decode_png(png: &[u8]) -> Result<RgbaImage> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .context("PNG повреждён")?;
    Ok(img.to_rgba8())
}

/// Содержимое-картинка из пикселей буфера.
pub fn content_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Content> {
    Ok(Content::Image {
        width,
        height,
        png: encode_png(width, height, rgba)?,
    })
}

/// Миниатюра в PNG: длинная сторона не больше `THUMB_SIDE`, пропорции сохраняются.
pub fn thumbnail(png: &[u8]) -> Result<Vec<u8>> {
    let img = decode_png(png)?;
    let (w, h) = img.dimensions();
    let scale = (THUMB_SIDE as f32 / w.max(h) as f32).min(1.0);
    let (tw, th) = (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    );
    let small: ImageBuffer<Rgba<u8>, Vec<u8>> =
        image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
    let mut out = Cursor::new(Vec::new());
    small
        .write_to(&mut out, image::ImageFormat::Png)
        .context("не удалось сохранить миниатюру")?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let on = (x / 8 + y / 8) % 2 == 0;
                v.extend_from_slice(if on {
                    &[230, 166, 59, 255]
                } else {
                    &[28, 30, 34, 255]
                });
            }
        }
        v
    }

    #[test]
    fn png_roundtrip_is_lossless() {
        let px = checker(40, 24);
        let png = encode_png(40, 24, &px).unwrap();
        let back = decode_png(&png).unwrap();
        assert_eq!(back.dimensions(), (40, 24));
        assert_eq!(back.into_raw(), px);
    }

    #[test]
    fn thumbnail_keeps_aspect_and_limits_side() {
        let png = encode_png(1600, 900, &checker(1600, 900)).unwrap();
        let t = decode_png(&thumbnail(&png).unwrap()).unwrap();
        assert_eq!(t.dimensions(), (320, 180));
        // Маленькая картинка не растягивается.
        let small = encode_png(40, 24, &checker(40, 24)).unwrap();
        assert_eq!(
            decode_png(&thumbnail(&small).unwrap())
                .unwrap()
                .dimensions(),
            (40, 24)
        );
    }

    #[test]
    fn rejects_absurd_size() {
        assert!(encode_png(20_000, 20_000, &[]).is_err());
    }
}
