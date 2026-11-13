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
    thumb_of(decode_png(png)?)
}

/// Файлы-картинки больше этого не открываем ради миниатюры и предпросмотра.
pub const MAX_IMAGE_FILE: u64 = 64 * 1024 * 1024;

/// MIME по расширению, если это картинка, которую мы умеем показать.
pub fn image_mime(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// Размеры картинки по заголовку файла, без декодирования. `None` — не картинка.
pub fn file_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    image_mime(&path.file_name()?.to_string_lossy())?;
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Миниатюра файла-картинки.
pub fn thumbnail_file(path: &std::path::Path) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path).context("файл недоступен")?;
    anyhow::ensure!(
        meta.len() <= MAX_IMAGE_FILE,
        "файл-картинка слишком большой"
    );
    let img = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()
        .context("картинка не читается")?;
    thumb_of(img.to_rgba8())
}

fn thumb_of(img: RgbaImage) -> Result<Vec<u8>> {
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
    fn image_files_are_recognised_and_thumbnailed() {
        assert_eq!(image_mime("Скрин.JPG"), Some("image/jpeg"));
        assert_eq!(image_mime("отчёт.pdf"), None);
        assert_eq!(image_mime("без_расширения"), None);
        let dir = std::env::temp_dir().join(format!("clipwire-img-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("кадр.png");
        std::fs::write(&path, encode_png(800, 400, &checker(800, 400)).unwrap()).unwrap();
        assert_eq!(file_dimensions(&path), Some((800, 400)));
        let t = decode_png(&thumbnail_file(&path).unwrap()).unwrap();
        assert_eq!(t.dimensions(), (320, 160));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_absurd_size() {
        assert!(encode_png(20_000, 20_000, &[]).is_err());
    }
}
