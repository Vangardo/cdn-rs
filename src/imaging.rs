use crate::{config::Settings, db::Db, errors::ApiError, util};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fast_image_resize::{self as fir, images::Image as FirImage};
use image::imageops;
use image::{self, DynamicImage, GenericImageView, ImageFormat, RgbImage};
use mozjpeg::{ColorSpace, Compress};
use reqwest::Client;
use std::{fs, io::Cursor, path::Path, time::Instant};
use tokio::{fs as tokio_fs, task};
use uuid::Uuid;

fn ensure_dir(path: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn normalize_onlihub_extensions(mut url: String) -> String {
    let low = url.to_lowercase();
    if low.contains("onlihub") || low.contains("mysellerhub") {
        if low.ends_with(".png") {
            url = url.trim_end_matches(".png").to_string() + ".jpg";
        } else if low.ends_with(".gif") {
            url = url.trim_end_matches(".gif").to_string() + ".jpg";
        }
    }
    url
}

pub async fn save_img_from_url(
    client: &Client,
    url: &str,
    output_path: &str,
) -> Result<Vec<u8>, ApiError> {
    if url.is_empty() {
        return Err(ApiError::BadRequest("empty url".into()));
    }
    ensure_dir(output_path).map_err(|e| ApiError::Io(e.to_string()))?;

    let norm_url = normalize_onlihub_extensions(url.to_string());

    let resp = client
        .get(&norm_url)
        .send()
        .await
        .map_err(|e| ApiError::Io(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(ApiError::Io(format!(
            "download failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ApiError::Io(e.to_string()))?;
    let buf = bytes.to_vec();
    let path = output_path.to_string();
    let data = buf.clone();
    tokio::spawn(async move {
        let _ = tokio_fs::write(path, &data).await;
    });
    Ok(buf)
}

pub async fn convert_and_save_jpg(
    base64_img: &str,
    db: &Db,
    settings: &Settings,
) -> Result<(i64, Uuid), ApiError> {
    let started = Instant::now();
    // status: 1 => downloading/creating, 4 => converted
    let guid = Uuid::new_v4();
    let id = db.insert_image_with_status(guid, 1).await?;

    // Декод и запись — в blocking, чтобы не забивать runtime
    let out_path = format!("{}{}.jpg", settings.media_base_dir, guid);
    ensure_dir(&out_path).map_err(|e| ApiError::Io(e.to_string()))?;

    let base64_img = base64_img.trim();
    let decoded = BASE64
        .decode(base64_img)
        .map_err(|e| ApiError::BadRequest(format!("invalid base64: {}", e)))?;

    task::spawn_blocking(move || -> Result<(), ApiError> {
        let img = image::load_from_memory(&decoded).map_err(|e| ApiError::Img(e.to_string()))?;
        let rgb = img.to_rgb8();
        let mut comp = Compress::new(ColorSpace::JCS_RGB);
        comp.set_size(rgb.width() as usize, rgb.height() as usize);
        comp.set_quality(85.0);
        let mut comp = comp
            .start_compress(Vec::new())
            .map_err(|e| ApiError::Img(e.to_string()))?;
        comp.write_scanlines(&rgb)
            .map_err(|e| ApiError::Img(e.to_string()))?;
        let buf = comp.finish().map_err(|e| ApiError::Img(e.to_string()))?;
        fs::write(&out_path, &buf).map_err(|e| ApiError::Io(e.to_string()))?;
        Ok(())
    })
    .await
    .map_err(|_| ApiError::Internal)??;

    db.update_image_status(id, 4).await?;
    tracing::info!(
        "convert_and_save_jpg took {}ms",
        started.elapsed().as_millis()
    );
    Ok((id, guid))
}

fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let h = hex.trim();
    if h.len() == 6 {
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(255);
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(255);
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(255);
        (r, g, b)
    } else {
        (255, 255, 255)
    }
}

fn place_center(bg_w: u32, bg_h: u32, fg_w: u32, fg_h: u32) -> (u32, u32) {
    let x = ((bg_w as i64 - fg_w as i64) / 2).max(0) as u32;
    let y = ((bg_h as i64 - fg_h as i64) / 2).max(0) as u32;
    (x, y)
}

/// Точный аналог Python get_resize_image (с фолбэком и даунлоадом по link_o)
pub async fn get_resize_image_bytes(
    guid_or_slug: &str,
    width: Option<u32>,
    height: Option<u32>,
    format: Option<&str>,
    bg_hex: Option<&str>,
    db: &Db,
    settings: &Settings,
    client: &Client,
) -> Result<(Vec<u8>, String), ApiError> {
    let started = Instant::now();

    // Разбираем guid
    let guid = if util::is_guid(guid_or_slug) {
        util::parse_guid(guid_or_slug).ok_or(ApiError::BadRequest("invalid guid".into()))?
    } else {
        // no-image
        // поведение Python: если не GUID — берём no-image
        Uuid::nil()
    };

    let input_path = if guid.is_nil() {
        settings.no_image_full_path()
    } else {
        format!("{}{}.jpg", settings.media_base_dir, guid)
    };

    let mut downloaded: Option<Vec<u8>> = None;
    if !Path::new(&input_path).exists() && !guid.is_nil() {
        if let Some(link) = db.get_original_link_by_guid(guid).await? {
            if let Ok(bytes) = save_img_from_url(client, &link, &input_path).await {
                downloaded = Some(bytes);
            }
        }
    }

    let no_image_path = settings.no_image_full_path();
    let max_side = settings.max_image_side;
    let bg_hex_owned = bg_hex.map(|s| s.to_string());
    let fmt_owned = format.map(|f| f.to_ascii_uppercase());

    if width.is_none()
        && height.is_none()
        && fmt_owned
            .as_deref()
            .map_or(true, |f| f.eq_ignore_ascii_case("JPEG"))
        && bg_hex_owned
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ffffff"))
            .unwrap_or(true)
    {
        if let Some(buf) = downloaded {
            return Ok((buf, "image/jpeg".to_string()));
        }
        let path = if Path::new(&input_path).exists() {
            input_path
        } else {
            no_image_path
        };
        let buf = tokio_fs::read(&path)
            .await
            .map_err(|e| ApiError::Io(e.to_string()))?;
        return Ok((buf, "image/jpeg".to_string()));
    }

    let (buf, ct) = task::spawn_blocking(move || -> Result<(Vec<u8>, String), ApiError> {
        let img = if let Some(b) = downloaded {
            image::load_from_memory(&b).map_err(|e| ApiError::Img(e.to_string()))?
        } else {
            image::open(&input_path)
                .or_else(|_| image::open(&no_image_path))
                .unwrap_or_else(|_| {
                    let img = RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255]));
                    DynamicImage::ImageRgb8(img)
                })
        };

        let (orig_w, orig_h) = img.dimensions();
        let (mut target_w, mut target_h) = match (width, height) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => {
                let scale = w as f32 / orig_w.max(1) as f32;
                (w, ((orig_h as f32 * scale).round() as u32).max(1))
            }
            (None, Some(hh)) => {
                let scale = hh as f32 / orig_h.max(1) as f32;
                (((orig_w as f32 * scale).round() as u32).max(1), hh)
            }
            (None, None) => (orig_w, orig_h),
        };

        if target_w > max_side {
            target_w = max_side;
        }
        if target_h > max_side {
            target_h = max_side;
        }

        let mut rgb = img.to_rgb8();
        if width.is_some() || height.is_some() {
            let src_image =
                FirImage::from_vec_u8(orig_w, orig_h, rgb.into_raw(), fir::PixelType::U8x3)
                    .map_err(|e| ApiError::Img(e.to_string()))?;
            let mut dst_image = FirImage::new(target_w, target_h, fir::PixelType::U8x3);
            let mut resizer = fir::Resizer::new();
            let options = fir::ResizeOptions::new()
                .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Box));
            resizer
                .resize(&src_image, &mut dst_image, &options)
                .map_err(|e| ApiError::Img(e.to_string()))?;
            rgb = RgbImage::from_raw(target_w, target_h, dst_image.into_vec())
                .ok_or_else(|| ApiError::Img("resize failed".into()))?;
        } else {
            target_w = orig_w;
            target_h = orig_h;
        }

        let bg_is_default = bg_hex_owned
            .as_deref()
            .unwrap_or("ffffff")
            .eq_ignore_ascii_case("ffffff");
        if rgb.width() != target_w || rgb.height() != target_h || !bg_is_default {
            let (r, g, b) = hex_to_rgb(bg_hex_owned.as_deref().unwrap_or("ffffff"));
            let mut bg = RgbImage::from_pixel(target_w, target_h, image::Rgb([r, g, b]));
            let (x, y) = place_center(target_w, target_h, rgb.width(), rgb.height());
            imageops::replace(&mut bg, &rgb, x as i64, y as i64);
            rgb = bg;
        }

        let fmt = match fmt_owned.as_deref() {
            Some("PNG") => ImageFormat::Png,
            Some("GIF") => ImageFormat::Gif,
            Some("TIFF") => ImageFormat::Tiff,
            Some("WEBP") => ImageFormat::WebP,
            _ => ImageFormat::Jpeg,
        };

        let mut buf = Vec::new();
        match fmt {
            ImageFormat::Jpeg => {
                let mut comp = Compress::new(ColorSpace::JCS_RGB);
                comp.set_size(target_w as usize, target_h as usize);
                comp.set_quality(85.0);
                let mut comp = comp
                    .start_compress(Vec::new())
                    .map_err(|e| ApiError::Img(e.to_string()))?;
                comp.write_scanlines(&rgb)
                    .map_err(|e| ApiError::Img(e.to_string()))?;
                buf = comp.finish().map_err(|e| ApiError::Img(e.to_string()))?;
            }
            other => {
                let mut cur = Cursor::new(&mut buf);
                DynamicImage::ImageRgb8(rgb)
                    .write_to(&mut cur, other)
                    .map_err(|e| ApiError::Img(e.to_string()))?;
            }
        }

        let ct = match fmt {
            ImageFormat::Png => "image/png",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Tiff => "image/tiff",
            ImageFormat::WebP => "image/webp",
            _ => "image/jpeg",
        }
        .to_string();

        Ok((buf, ct))
    })
    .await
    .map_err(|_| ApiError::Internal)??;

    tracing::info!(
        "get_resize_image_bytes took {}ms",
        started.elapsed().as_millis()
    );
    Ok((buf, ct))
}
