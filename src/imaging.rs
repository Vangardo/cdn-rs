use crate::{config::Settings, db::Db, errors::ApiError, util};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fast_image_resize::{self as fir, images::Image as FirImage};
use image::imageops;
use image::{self, DynamicImage, ImageFormat, RgbImage};
use mozjpeg::{ColorSpace, Compress, Decompress};
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{fs, io::Cursor, path::Path, time::Instant};
use tokio::{fs as tokio_fs, sync::Semaphore, task};
use uuid::Uuid;

static BLOCKING_SEM: Lazy<Semaphore> =
    Lazy::new(|| Semaphore::new(num_cpus::get().saturating_sub(1).max(1)));

const JPEG_Q: f32 = 75.0;

trait DecompressExt {
    fn set_scale(&mut self, num: u32, denom: u32);
}

impl<R> DecompressExt for Decompress<R> {
    fn set_scale(&mut self, num: u32, denom: u32) {
        let denom = denom.max(1);
        let numer = ((num * 8) / denom).clamp(1, 16) as u8;
        self.scale(numer);
    }
}

fn ensure_dir(path: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
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
    (tw, th, fg_w, fg_h, r < 1.0)
}

fn choose_dct_denom(sw: u32, sh: u32, fg_w: u32, fg_h: u32) -> u8 {
    // хотим после DCT-скейла иметь >= fg_w×fg_h (чтобы дальше ТОЛЬКО даунскейл)
    for &den in &[8u8, 4, 2, 1] {
        if sw / den as u32 >= fg_w && sh / den as u32 >= fg_h {
            return den;
        }
    }
    1
}

fn fir_resize(rgb: RgbImage, dst_w: u32, dst_h: u32) -> Result<RgbImage, ApiError> {
    if rgb.width() == dst_w && rgb.height() == dst_h {
        return Ok(rgb);
    }
    let ratio = (dst_w as f32 / rgb.width() as f32)
        .min(dst_h as f32 / rgb.height() as f32);
    let filter = if ratio < 0.5 {
        fir::FilterType::Lanczos3
    } else {
        fir::FilterType::Bilinear
    };
    let src = FirImage::from_vec_u8(rgb.width(), rgb.height(), rgb.into_raw(), fir::PixelType::U8x3)
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

    let permit = BLOCKING_SEM.acquire().await.unwrap();
    let (buf, ct) = task::spawn_blocking(move || -> Result<(Vec<u8>, String), ApiError> {
        let data = if let Some(b) = downloaded {
            b
        } else if let Ok(bytes) = fs::read(&input_path) {
            bytes
        } else if let Ok(bytes) = fs::read(&no_image_path) {
            bytes
        } else {
            Vec::new()
        };

        let orig_data = data.clone();
        let try_mozjpeg = Decompress::new_mem(&data);
        let (mut rgb, tw, th) = if let Ok(mut d) = try_mozjpeg {
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
                return Ok((orig_data, "image/jpeg".to_string()));
            }

            let denom = choose_dct_denom(sw, sh, fg_w, fg_h);
            d.set_scale(1, denom as u32);
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
        } else {
            let img = if data.is_empty() {
                DynamicImage::ImageRgb8(RgbImage::from_pixel(
                    1,
                    1,
                    image::Rgb([255, 255, 255]),
                ))
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

    tracing::info!(
        "get_resize_image_bytes took {}ms",
        started.elapsed().as_millis()
    );
    Ok((buf, ct))
}
