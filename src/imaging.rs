use crate::{cache::Cache, config::Settings, db::Db, errors::ApiError, util};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use dashmap::DashMap;
use fast_image_resize::{self as fir, images::Image as FirImage};
use image::imageops;
use image::{self, DynamicImage, ImageFormat, RgbImage};
use mozjpeg::{ColorSpace, Compress, Decompress};
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{fs, io::Cursor, path::Path, sync::Arc, time::Instant};
use tokio::{
    fs as tokio_fs,
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore},
    task,
};
use uuid::Uuid;

static BLOCKING_SEM: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(num_cpus::get() * 2)); // Увеличиваем параллельность

const JPEG_Q: f32 = 75.0;

async fn ensure_dir_async(path: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        tokio_fs::create_dir_all(parent).await?;
    }
    Ok(())
}

async fn atomic_write(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        tokio_fs::create_dir_all(parent).await?;
    }
    let tmp = format!("{}.part", path);
    let mut f = tokio_fs::File::create(&tmp).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    f.sync_all().await?;
    drop(f);
    tokio_fs::rename(&tmp, path).await?;
    Ok(())
}

static VARIANT_LOCKS: Lazy<DashMap<String, Arc<Mutex<()>>>> = Lazy::new(DashMap::new);

async fn with_variant_lock<T, F>(key: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let lock = VARIANT_LOCKS
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _g = lock.lock().await;
    fut.await
}

fn target_and_foreground(
    orig_w: u32,
    orig_h: u32,
    req_w: Option<u32>,
    req_h: Option<u32>,
    max_side: u32,
) -> (u32, u32, u32, u32, bool) {
    // рамка (target)
    let mut tw = req_w.unwrap_or(orig_w).min(max_side);
    let mut th = req_h.unwrap_or(orig_h).min(max_side);
    if req_w.is_none() && req_h.is_none() {
        tw = orig_w.min(max_side);
        th = orig_h.min(max_side);
    }
    // вписывание + баним апскейл
    let r = (tw as f32 / orig_w as f32)
        .min(th as f32 / orig_h as f32)
        .min(1.0);
    let fg_w = ((orig_w as f32 * r).round() as u32).max(1);
    let fg_h = ((orig_h as f32 * r).round() as u32).max(1);

    // если задана только одна сторона, то конечный холст совпадает с изображением
    if req_w.is_some() && req_h.is_none() {
        th = fg_h;
    } else if req_h.is_some() && req_w.is_none() {
        tw = fg_w;
    }

    (tw, th, fg_w, fg_h, r < 1.0)
}

fn fir_resize(rgb: RgbImage, dst_w: u32, dst_h: u32) -> Result<RgbImage, ApiError> {
    if rgb.width() == dst_w && rgb.height() == dst_h {
        return Ok(rgb);
    }
    if dst_w > rgb.width() || dst_h > rgb.height() {
        return Err(ApiError::Img("upscale not supported".into()));
    }

    let max_dim = dst_w.max(dst_h);
    let filter = if max_dim >= 512 {
        fir::FilterType::Lanczos3
    } else if max_dim >= 256 {
        fir::FilterType::Mitchell
    } else {
        fir::FilterType::Hamming
    };

    let src = FirImage::from_vec_u8(
        rgb.width(),
        rgb.height(),
        rgb.into_raw(),
        fir::PixelType::U8x3,
    )
    .map_err(|e| ApiError::Img(e.to_string()))?;
    let mut dst = FirImage::new(dst_w, dst_h, fir::PixelType::U8x3);
    let mut resizer = fir::Resizer::new();
    let opts = fir::ResizeOptions::new().resize_alg(fir::ResizeAlg::Convolution(filter));
    resizer
        .resize(&src, &mut dst, &opts)
        .map_err(|e| ApiError::Img(e.to_string()))?;
    RgbImage::from_raw(dst_w, dst_h, dst.into_vec())
        .ok_or_else(|| ApiError::Img("resize failed".into()))
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
    if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
        let ok = ct
            .to_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .starts_with("image/");
        if !ok {
            return Err(ApiError::Img(format!("bad content-type: {ct:?}")));
        }
    }
    let content_length = resp.content_length();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ApiError::Io(e.to_string()))?;
    if let Some(cl) = content_length {
        if bytes.len() as u64 != cl {
            return Err(ApiError::Img("truncated download".into()));
        }
    }
    let buf = bytes.to_vec();
    // verify that downloaded bytes represent a valid image before saving
    image::load_from_memory(&buf)
        .map_err(|e| ApiError::Img(format!("invalid image data: {}", e)))?;
    atomic_write(output_path, &buf)
        .await
        .map_err(|e| ApiError::Io(e.to_string()))?;
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
    ensure_dir_async(&out_path)
        .await
        .map_err(|e| ApiError::Io(e.to_string()))?;

    let base64_img = base64_img.trim();
    let decoded = BASE64
        .decode(base64_img)
        .map_err(|e| ApiError::BadRequest(format!("invalid base64: {}", e)))?;

    let permit = BLOCKING_SEM.acquire().await.unwrap();
    task::spawn_blocking(move || -> Result<(), ApiError> {
        let rgb = match Decompress::new_mem(&decoded) {
            Ok(dec) => {
                let mut dec = dec.rgb().map_err(|e| ApiError::Img(e.to_string()))?;
                let w = dec.width() as u32;
                let h = dec.height() as u32;
                let mut data = vec![0u8; (w * h * 3) as usize];
                dec.read_scanlines_into(&mut data)
                    .map_err(|e| ApiError::Img(e.to_string()))?;
                RgbImage::from_raw(w, h, data)
                    .ok_or_else(|| ApiError::Img("decode failed".into()))?
            }
            Err(_) => image::load_from_memory(&decoded)
                .map_err(|e| ApiError::Img(e.to_string()))?
                .to_rgb8(),
        };
        let mut comp = Compress::new(ColorSpace::JCS_RGB);
        comp.set_size(rgb.width() as usize, rgb.height() as usize);
        comp.set_quality(JPEG_Q);
        comp.set_optimize_scans(false);
        comp.set_optimize_coding(false);
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
    drop(permit);

    db.update_image_status(id, 4).await?;
    if !settings.production_mode {
        tracing::info!(
            "convert_and_save_jpg took {}ms",
            started.elapsed().as_millis()
        );
    }
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

pub fn variant_disk_path(
    settings: &Settings,
    guid: Uuid,
    width: Option<u32>,
    height: Option<u32>,
    fmt: Option<&str>,
    bg_hex: Option<&str>,
) -> (String, String) {
    let fmt_upper = fmt.unwrap_or("JPEG").to_ascii_uppercase();
    let (fmt_word, ext, ct) = match fmt_upper.as_str() {
        "PNG" => ("png", "png", "image/png"),
        "GIF" => ("gif", "gif", "image/gif"),
        "TIFF" => ("tiff", "tiff", "image/tiff"),
        "WEBP" => ("webp", "webp", "image/webp"),
        _ => ("jpeg", "jpg", "image/jpeg"),
    };
    let bg = bg_hex.unwrap_or("ffffff").to_lowercase();
    let path = format!(
        "{}variants/{}/{w}x{h}_{bg}_{fmt_word}_q{q}.{ext}",
        settings.media_base_dir,
        guid,
        w = width.unwrap_or(0),
        h = height.unwrap_or(0),
        bg = bg,
        fmt_word = fmt_word,
        q = JPEG_Q as u8,
        ext = ext
    );
    (path, ct.to_string())
}

/// Оптимизированная версия - максимальная скорость
pub async fn get_resize_image_bytes(
    guid_or_slug: &str,
    width: Option<u32>,
    height: Option<u32>,
    format: Option<&str>,
    bg_hex: Option<&str>,
    db: &Db,
    settings: &Settings,
    client: &Client,
    _cache: &Cache, // Оставляем для совместимости, но не используем
) -> Result<(Vec<u8>, String), ApiError> {
    let started = Instant::now();

    let guid = if util::is_guid(guid_or_slug) {
        util::parse_guid(guid_or_slug).ok_or(ApiError::BadRequest("invalid guid".into()))?
    } else {
        Uuid::nil()
    };

    let fmt_owned = format.map(|f| f.to_ascii_uppercase());
    let bg_hex_owned = bg_hex.map(|s| s.to_string());

    // 1. СНАЧАЛА ПРОВЕРЯЕМ ДИСК - самый быстрый для CDN
    let (disk_path, ct_guess) = variant_disk_path(
        settings,
        guid,
        width,
        height,
        fmt_owned.as_deref(),
        bg_hex_owned.as_deref(),
    );

    if let Ok(bytes) = tokio_fs::read(&disk_path).await {
        return Ok((bytes, ct_guess));
    }

    let result = with_variant_lock(&disk_path, async {
        if let Ok(bytes) = tokio_fs::read(&disk_path).await {
            return Ok((bytes, ct_guess.clone()));
        }

        let input_path = if guid.is_nil() {
            settings.no_image_full_path()
        } else {
            format!("{}{}.jpg", settings.media_base_dir, guid)
        };

        let file_exists = tokio_fs::try_exists(&input_path).await.unwrap_or(false);
        let mut downloaded: Option<Vec<u8>> = None;
        if !file_exists && !guid.is_nil() {
            if let Some(link) = db.get_original_link_by_guid(guid).await? {
                downloaded = save_img_from_url(client, &link, &input_path).await.ok();
            }
        }

        let no_image_path = settings.no_image_full_path();
        let max_side = settings.max_image_side;

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
            let buf = if let Some(buf) = downloaded {
                buf
            } else if file_exists {
                tokio_fs::read(&input_path)
                    .await
                    .map_err(|e| ApiError::Io(e.to_string()))?
            } else {
                tokio_fs::read(&no_image_path)
                    .await
                    .map_err(|e| ApiError::Io(e.to_string()))?
            };
            atomic_write(&disk_path, &buf)
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            return Ok((buf, ct_guess.clone()));
        }

        let permit = BLOCKING_SEM.acquire().await.unwrap();
        let input_path_clone = input_path.clone();
        let no_image_path_clone = no_image_path.clone();
        let (buf, ct) = task::spawn_blocking(move || -> Result<(Vec<u8>, String), ApiError> {
            let data = if let Some(b) = downloaded {
                b
            } else if Path::new(&input_path_clone).exists() {
                fs::read(&input_path_clone).map_err(|e| ApiError::Io(e.to_string()))?
            } else {
                fs::read(&no_image_path_clone).map_err(|e| ApiError::Io(e.to_string()))?
            };

            let mut early_ct: Option<String> = None;
            let (mut rgb, tw, th) = if let Ok(mut d) = Decompress::new_mem(&data) {
                let (sw, sh) = (d.width() as u32, d.height() as u32);
                let (tw, th, fg_w, fg_h, need_resize) =
                    target_and_foreground(sw, sh, width, height, max_side);

                if (width.is_some() || height.is_some())
                    && fg_w == sw
                    && fg_h == sh
                    && tw == sw
                    && th == sh
                    && fmt_owned
                        .as_deref()
                        .map_or(true, |f| f.eq_ignore_ascii_case("JPEG"))
                    && bg_hex_owned
                        .as_deref()
                        .map(|s| s.eq_ignore_ascii_case("ffffff"))
                        .unwrap_or(true)
                {
                    drop(d);
                    early_ct = Some("image/jpeg".to_string());
                    (RgbImage::new(0, 0), 0, 0)
                } else {
                    let mut started = d.rgb().map_err(|e| ApiError::Img(e.to_string()))?;
                    let (dw, dh) = (started.width() as u32, started.height() as u32);
                    let mut buf = vec![0u8; (dw * dh * 3) as usize];
                    started
                        .read_scanlines_into(&mut buf)
                        .map_err(|e| ApiError::Img(e.to_string()))?;
                    let base = RgbImage::from_raw(dw, dh, buf)
                        .ok_or_else(|| ApiError::Img("bad rgb".into()))?;
                    let rgb = if need_resize {
                        fir_resize(base, fg_w, fg_h)?
                    } else {
                        base
                    };
                    (rgb, tw, th)
                }
            } else {
                let img = if data.is_empty() {
                    DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255])))
                } else {
                    image::load_from_memory(&data).map_err(|e| ApiError::Img(e.to_string()))?
                };
                let (sw, sh) = (img.width(), img.height());
                let (tw, th, fg_w, fg_h, need_resize) =
                    target_and_foreground(sw, sh, width, height, max_side);
                let base = img.to_rgb8();
                let rgb = if need_resize {
                    fir_resize(base, fg_w, fg_h)?
                } else {
                    base
                };
                (rgb, tw, th)
            };

            if let Some(ct) = early_ct {
                return Ok((data, ct));
            }

            let bg_is_default = bg_hex_owned
                .as_deref()
                .unwrap_or("ffffff")
                .eq_ignore_ascii_case("ffffff");
            if rgb.width() != tw || rgb.height() != th || !bg_is_default {
                let (r, g, b) = hex_to_rgb(bg_hex_owned.as_deref().unwrap_or("ffffff"));
                let mut bg = RgbImage::from_pixel(tw, th, image::Rgb([r, g, b]));
                let x = ((tw - rgb.width()) / 2) as i64;
                let y = ((th - rgb.height()) / 2) as i64;
                imageops::replace(&mut bg, &rgb, x, y);
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
                    comp.set_size(tw as usize, th as usize);
                    comp.set_quality(JPEG_Q);
                    comp.set_optimize_scans(false);
                    comp.set_optimize_coding(false);
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
        drop(permit);

        atomic_write(&disk_path, &buf)
            .await
            .map_err(|e| ApiError::Io(e.to_string()))?;
        Ok((buf, ct))
    })
    .await?;

    if !settings.production_mode {
        tracing::info!(
            "get_resize_image_bytes took {}ms",
            started.elapsed().as_millis()
        );
    }
    Ok(result)
}
