use crate::{config::Settings, db::Db, errors::ApiError, proxy, util};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::{imageops::FilterType, DynamicImage, GenericImageView, ImageFormat, Rgba, RgbaImage};
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
    url: &str,
    output_path: &str,
    settings: &Settings,
) -> Result<(), ApiError> {
    if url.is_empty() {
        return Err(ApiError::BadRequest("empty url".into()));
    }
    if Path::new(output_path).exists() {
        return Ok(());
    }
    ensure_dir(output_path).map_err(|e| ApiError::Io(e.to_string()))?;

    let norm_url = normalize_onlihub_extensions(url.to_string());

    let mut builder = Client::builder().timeout(std::time::Duration::from_secs(4));
    if settings.use_proxies {
        if let Some(px) = proxy::random_proxy() {
            if let Ok(proxy) = reqwest::Proxy::all(&px) {
                builder = builder.proxy(proxy);
            }
        }
    }
    let client = builder.build().map_err(|e| ApiError::Io(e.to_string()))?;

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
    tokio_fs::write(output_path, &bytes)
        .await
        .map_err(|e| ApiError::Io(e.to_string()))?;
    Ok(())
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
        let mut buf = Vec::new();
        {
            let mut cursor = Cursor::new(&mut buf);
            JpegEncoder::new_with_quality(&mut cursor, 85)
                .encode_image(&rgb)
                .map_err(|e| ApiError::Img(e.to_string()))?;
        }
        fs::write(&out_path, &buf).map_err(|e| ApiError::Io(e.to_string()))?;
        Ok(())
    })
    .await
    .map_err(|_| ApiError::Internal)??;

    db.update_image_status(id, 4).await?;
    tracing::info!("convert_and_save_jpg took {}ms", started.elapsed().as_millis());
    Ok((id, guid))
}

fn fit_to_box(img: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    // аккуратно вписываем в рамку с сохранением пропорций
    img.resize_to_fill(width, height, FilterType::Triangle)
}

fn fit_inside(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    img.resize(max_w, max_h, FilterType::CatmullRom)
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

    // Попытка открыть локально; при неудаче — авто-даунлоад по link_o
    let mut img = match task::spawn_blocking({
        let input_path = input_path.clone();
        move || image::open(&input_path)
    })
    .await
    {
        Ok(Ok(img)) => img,
        _ => {
            let open_no_image = || -> Result<DynamicImage, ApiError> {
                match image::open(settings.no_image_full_path()) {
                    Ok(img) => Ok(img),
                    Err(_) => {
                        let img = RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 255]));
                        Ok(DynamicImage::ImageRgba8(img))
                    }
                }
            };
            if !guid.is_nil() {
                if let Some(link) = db.get_original_link_by_guid(guid).await? {
                    let _ = save_img_from_url(&link, &input_path, settings).await;
                    match task::spawn_blocking({
                        let input_path = input_path.clone();
                        move || image::open(&input_path)
                    })
                    .await
                    .ok()
                    .and_then(Result::ok)
                    {
                        Some(img) => img,
                        None => open_no_image()?,
                    }
                } else {
                    open_no_image()?
                }
            } else {
                open_no_image()?
            }
        }
    };

    // Определяем рамку
    let (w, h) = img.dimensions();
    let (mut target_w, mut target_h) = match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            let scale = w as f32 / w.max(1) as f32;
            ((w), ((h as f32 * scale) as u32).max(1))
        }
        (None, Some(hh)) => {
            let scale = hh as f32 / h.max(1) as f32;
            (((w as f32 * scale) as u32).max(1), hh)
        }
        (None, None) => (w, h),
    };

    // Лимиты (как в Python)
    let max_side = settings.max_image_side;
    if target_w > max_side {
        target_w = max_side;
    }
    if target_h > max_side {
        target_h = max_side;
    }

    if width.is_some() || height.is_some() {
        // вписываем внутрь рамки (без выхода за пределы)
        img = fit_inside(&img, target_w, target_h);
    }
    let (fg_w, fg_h) = img.dimensions();

    // Фон
    let (r, g, b) = hex_to_rgb(bg_hex.unwrap_or("ffffff"));
    let mut bg = RgbaImage::from_pixel(target_w, target_h, image::Rgba([r, g, b, 255]));
    let (x, y) = place_center(target_w, target_h, fg_w, fg_h);
    image::imageops::overlay(&mut bg, &img.to_rgba8(), x.into(), y.into());
    let composed = DynamicImage::ImageRgba8(bg);

    // Формат
    let fmt = match format.map(|f| f.to_ascii_uppercase()) {
        Some(ref s) if s == "PNG" => ImageFormat::Png,
        Some(ref s) if s == "GIF" => ImageFormat::Gif,
        Some(ref s) if s == "TIFF" => ImageFormat::Tiff,
        Some(ref s) if s == "WEBP" => ImageFormat::WebP,
        _ => ImageFormat::Jpeg,
    };

    let buf = task::spawn_blocking(move || -> Result<Vec<u8>, image::ImageError> {
        let mut buf = Vec::new();
        {
            let mut cur = Cursor::new(&mut buf);
            match fmt {
                ImageFormat::Jpeg => {
                    JpegEncoder::new_with_quality(&mut cur, 85).encode_image(&composed)?
                }
                other => composed.write_to(&mut cur, other)?,
            }
        }
        Ok(buf)
    })
    .await
    .map_err(|_| ApiError::Internal)?
    .map_err(|e| ApiError::Img(e.to_string()))?;

    // content-type
    let ct = match format.map(|f| f.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("tiff") => "image/tiff",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
    .to_string();

    tracing::info!("get_resize_image_bytes took {}ms", started.elapsed().as_millis());
    Ok((buf, ct))
}
