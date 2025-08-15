use crate::{config::Settings, db::Db, errors::ApiError, util};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fast_image_resize::{self as fir, images::Image as FirImage};
use image::imageops;
use image::{self, DynamicImage, ImageFormat, RgbImage};
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{
    collections::HashMap,
    fs,
    io::Cursor,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    fs as tokio_fs,
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore},
    task,
};
use uuid::Uuid;

// Ограничения для предотвращения утечек памяти
const MAX_IMAGE_SIZE: usize = 50 * 1024 * 1024; // 50MB
const MAX_IMAGE_DIMENSION: u32 = 8192; // 8K
const LOCK_CLEANUP_INTERVAL: u32 = 5; // очистка каждые 5 запросов (еще чаще)
const LOCK_TTL_SECONDS: u64 = 30; // TTL для блокировок: 30 секунд (еще меньше)

static BLOCKING_SEM: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(num_cpus::get()))); // Уменьшаем количество слотов

// Добавляем счетчик активных задач для мониторинга
static ACTIVE_TASKS: Lazy<Mutex<usize>> = Lazy::new(|| Mutex::new(0));

// Функция для получения статистики активных задач
pub fn get_active_tasks_count() -> usize {
    ACTIVE_TASKS.try_lock().map(|count| *count).unwrap_or(0)
}

const JPEG_Q: f32 = 85.0;

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

    // Принудительно освобождаем файловые буферы
    #[cfg(target_os = "linux")]
    {
        if let Ok(_) = std::process::Command::new("sync")
            .arg("-f")
            .arg(&tmp)
            .status()
        {
            // Синхронизируем конкретный файл
        }
    }

    tokio_fs::rename(&tmp, path).await?;

    // Финальная синхронизация
    #[cfg(target_os = "linux")]
    {
        if let Ok(_) = std::process::Command::new("sync")
            .arg("-f")
            .arg(path)
            .status()
        {
            // Синхронизируем финальный файл
        }
    }

    Ok(())
}

// Структура для отслеживания блокировок с TTL
struct LockEntry {
    lock: Arc<Mutex<()>>,
    last_used: Instant,
}

// Заменяем DashMap на более простую структуру с ручной очисткой
static VARIANT_LOCKS: Lazy<Mutex<HashMap<String, LockEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// Счетчик для периодической очистки
static CLEANUP_COUNTER: Lazy<Mutex<u32>> = Lazy::new(|| Mutex::new(0));

async fn cleanup_old_locks() {
    if let Ok(mut counter) = CLEANUP_COUNTER.try_lock() {
        *counter += 1;
        if *counter >= LOCK_CLEANUP_INTERVAL {
            *counter = 0;
            if let Ok(mut locks) = VARIANT_LOCKS.try_lock() {
                let now = Instant::now();
                let ttl = Duration::from_secs(LOCK_TTL_SECONDS);
                let before_count = locks.len();
                locks.retain(|_, entry| now.duration_since(entry.last_used) < ttl);
                let after_count = locks.len();
                let cleaned = before_count - after_count;
                if cleaned > 0 {
                    tracing::info!(
                        "Cleaned up {} old locks, remaining: {}",
                        cleaned,
                        after_count
                    );
                }
            }

            // Принудительно вызываем сборщик мусора (если доступен)
            #[cfg(target_os = "linux")]
            {
                // Попытка освободить память через системные вызовы
                if let Ok(_) = std::process::Command::new("sync").status() {
                    tracing::debug!("Called sync to flush filesystem buffers");
                }

                // Попытка очистить кэш файловой системы
                if let Ok(_) = std::process::Command::new("echo")
                    .arg("3")
                    .arg(">")
                    .arg("/proc/sys/vm/drop_caches")
                    .status()
                {
                    tracing::debug!("Attempted to drop page cache");
                }

                // Попытка освободить неиспользуемую память
                if let Ok(_) = std::process::Command::new("echo")
                    .arg("1")
                    .arg(">")
                    .arg("/proc/sys/vm/compact_memory")
                    .status()
                {
                    tracing::debug!("Attempted to compact memory");
                }
            }

            // Логируем статистику активных задач
            let active_tasks = get_active_tasks_count();
            if active_tasks > 0 {
                tracing::info!("Active tasks: {}", active_tasks);
            }

            // Получаем текущее количество блокировок для отчета
            let lock_count = VARIANT_LOCKS
                .try_lock()
                .map(|locks| locks.len())
                .unwrap_or(0);

            // Логируем общую статистику
            tracing::info!(
                "Cleanup stats - Locks: {}, Tasks: {}, Counter: {}",
                lock_count,
                active_tasks,
                *counter,
            );
        }
    }
}

// Функция для принудительной очистки всех блокировок
pub async fn force_cleanup_locks() {
    if let Ok(mut locks) = VARIANT_LOCKS.try_lock() {
        let count = locks.len();
        locks.clear();
        tracing::info!("Force cleaned up {} locks", count);
    }

    // Принудительно очищаем счетчик активных задач
    if let Ok(mut counter) = CLEANUP_COUNTER.try_lock() {
        *counter = 0;
    }

    // Принудительно очищаем счетчик активных задач
    if let Ok(mut tasks) = ACTIVE_TASKS.try_lock() {
        *tasks = 0;
        tracing::info!("Reset active tasks counter to 0");
    }

    // Принудительная очистка файловых буферов
    #[cfg(target_os = "linux")]
    {
        if let Ok(_) = std::process::Command::new("sync").status() {
            tracing::info!("Force synced filesystem buffers");
        }

        // Попытка очистить кэш файловой системы (требует root)
        if let Ok(_) = std::process::Command::new("echo")
            .arg("3")
            .arg(">")
            .arg("/proc/sys/vm/drop_caches")
            .status()
        {
            tracing::info!("Attempted to drop page cache");
        }
    }
}

// Функция для получения статистики блокировок
pub fn get_locks_stats() -> (usize, u32, usize) {
    let lock_count = VARIANT_LOCKS
        .try_lock()
        .map(|locks| locks.len())
        .unwrap_or(0);
    let cleanup_counter = CLEANUP_COUNTER
        .try_lock()
        .map(|counter| *counter)
        .unwrap_or(0);
    let active_tasks = get_active_tasks_count();
    (lock_count, cleanup_counter, active_tasks)
}

async fn with_variant_lock<T, F>(key: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let lock = {
        let mut locks = VARIANT_LOCKS.lock().await;
        let entry = locks.entry(key.to_string()).or_insert_with(|| LockEntry {
            lock: Arc::new(Mutex::new(())),
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        entry.lock.clone()
    };

    let guard = lock.lock().await;
    let result = fut.await;
    drop(guard);

    {
        let mut locks = VARIANT_LOCKS.lock().await;
        if let Some(entry) = locks.get(key) {
            if Arc::strong_count(&entry.lock) == 1 {
                locks.remove(key);
            }
        }
    }

    cleanup_old_locks().await;
    result
}

fn target_and_foreground(
    orig_w: u32,
    orig_h: u32,
    req_w: Option<u32>,
    req_h: Option<u32>,
    max_side: u32,
) -> (u32, u32, u32, u32, bool) {
    // Проверяем исходные размеры
    if orig_w == 0 || orig_h == 0 {
        return (1, 1, 1, 1, false);
    }

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

    // Финальная проверка - все размеры должны быть > 0
    let tw = tw.max(1);
    let th = th.max(1);
    let fg_w = fg_w.max(1);
    let fg_h = fg_h.max(1);

    (tw, th, fg_w, fg_h, r < 1.0)
}

fn fir_resize(rgb: RgbImage, dst_w: u32, dst_h: u32) -> Result<RgbImage, ApiError> {
    if rgb.width() == dst_w && rgb.height() == dst_h {
        return Ok(rgb);
    }

    // Проверяем на нулевые размеры
    if dst_w == 0 || dst_h == 0 {
        return Err(ApiError::Img(
            "invalid destination dimensions: zero size".into(),
        ));
    }

    // Проверяем, что исходное изображение не пустое
    if rgb.width() == 0 || rgb.height() == 0 {
        return Err(ApiError::Img("source image has zero dimensions".into()));
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

    // Сохраняем размеры до вызова into_raw()
    let width = rgb.width();
    let height = rgb.height();

    let rgb_data = rgb.into_raw();

    // Проверяем, что данные RGB корректны
    if rgb_data.len() != (width * height * 3) as usize {
        return Err(ApiError::Img("RGB data length mismatch".into()));
    }

    // Проверяем, что данные RGB корректны (u8 не может быть > 255, но оставляем для документации)
    // if rgb_data.iter().any(|&b| b > 255) {
    //     return Err(ApiError::Img("Invalid RGB values detected".into()));
    // }

    let src = FirImage::from_vec_u8(width, height, rgb_data, fir::PixelType::U8x3)
        .map_err(|e| ApiError::Img(e.to_string()))?;
    let mut dst = FirImage::new(dst_w, dst_h, fir::PixelType::U8x3);
    let mut resizer = fir::Resizer::new();
    let opts = fir::ResizeOptions::new().resize_alg(fir::ResizeAlg::Convolution(filter));
    resizer
        .resize(&src, &mut dst, &opts)
        .map_err(|e| ApiError::Img(e.to_string()))?;

    let result_data = dst.into_vec();

    // Проверяем, что результат ресайза корректный
    if result_data.len() != (dst_w * dst_h * 3) as usize {
        return Err(ApiError::Img("resize result data length mismatch".into()));
    }

    let result = RgbImage::from_raw(dst_w, dst_h, result_data)
        .ok_or_else(|| ApiError::Img("resize failed".into()))?;

    // Дополнительная проверка результата
    if result.width() != dst_w || result.height() != dst_h {
        return Err(ApiError::Img("resize result has wrong dimensions".into()));
    }

    // Проверяем, что результат не пустой
    if result.width() == 0 || result.height() == 0 {
        return Err(ApiError::Img("resize result has zero dimensions".into()));
    }

    Ok(result)
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

    // Проверяем content-type
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

    // Проверяем размер файла до загрузки
    let content_length = resp.content_length();
    if let Some(cl) = content_length {
        if cl > MAX_IMAGE_SIZE as u64 {
            return Err(ApiError::BadRequest(format!(
                "image too large: {} bytes (max: {} bytes)",
                cl, MAX_IMAGE_SIZE
            )));
        }
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ApiError::Io(e.to_string()))?;

    // Проверяем размер после загрузки
    if bytes.len() > MAX_IMAGE_SIZE {
        return Err(ApiError::BadRequest(format!(
            "image too large: {} bytes (max: {} bytes)",
            bytes.len(),
            MAX_IMAGE_SIZE
        )));
    }

    if let Some(cl) = content_length {
        if bytes.len() as u64 != cl {
            return Err(ApiError::Img("truncated download".into()));
        }
    }

    let buf = bytes.to_vec();

    // Проверяем, что это валидное изображение и получаем его размеры
    let img = image::load_from_memory(&buf)
        .map_err(|e| ApiError::Img(format!("invalid image data: {}", e)))?;

    // Проверяем размеры изображения
    if img.width() > MAX_IMAGE_DIMENSION || img.height() > MAX_IMAGE_DIMENSION {
        return Err(ApiError::BadRequest(format!(
            "image dimensions too large: {}x{} (max: {}x{})",
            img.width(),
            img.height(),
            MAX_IMAGE_DIMENSION,
            MAX_IMAGE_DIMENSION
        )));
    }

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

    // Проверяем размер base64 строки
    if base64_img.len() > MAX_IMAGE_SIZE * 2 {
        // base64 примерно в 1.33 раза больше
        return Err(ApiError::BadRequest(format!(
            "base64 image too large: {} chars (max: {} chars)",
            base64_img.len(),
            MAX_IMAGE_SIZE * 2
        )));
    }

    let decoded = BASE64
        .decode(base64_img)
        .map_err(|e| ApiError::BadRequest(format!("invalid base64: {}", e)))?;

    // Проверяем размер декодированных данных
    if decoded.len() > MAX_IMAGE_SIZE {
        return Err(ApiError::BadRequest(format!(
            "decoded image too large: {} bytes (max: {} bytes)",
            decoded.len(),
            MAX_IMAGE_SIZE
        )));
    }

    // Увеличиваем счетчик активных задач
    {
        if let Ok(mut count) = ACTIVE_TASKS.try_lock() {
            *count += 1;
        }
    }

    let permit = BLOCKING_SEM.clone().acquire_owned().await.unwrap();
    let _result = task::spawn_blocking(move || -> Result<(), ApiError> {
        let _permit = permit; // permit автоматически освободится при drop

        // Проверяем, что это валидное изображение
        let img = image::load_from_memory(&decoded).map_err(|e| ApiError::Img(e.to_string()))?;

        // Проверяем размеры изображения
        if img.width() > MAX_IMAGE_DIMENSION || img.height() > MAX_IMAGE_DIMENSION {
            return Err(ApiError::BadRequest(format!(
                "image dimensions too large: {}x{} (max: {}x{})",
                img.width(),
                img.height(),
                MAX_IMAGE_DIMENSION,
                MAX_IMAGE_DIMENSION
            )));
        }

        let rgb = img.to_rgb8();

        // Используем image crate для JPEG-кодирования
        let mut buf = Vec::new();
        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_Q as u8);
        encoder
            .encode(
                &rgb,
                rgb.width(),
                rgb.height(),
                image::ColorType::Rgb8.into(),
            )
            .map_err(|e| ApiError::Img(format!("JPEG encoding failed: {}", e)))?;

        fs::write(&out_path, &buf).map_err(|e| ApiError::Io(e.to_string()))?;
        Ok(())
    })
    .await
    .map_err(|_| ApiError::Internal)??;

    // Уменьшаем счетчик активных задач
    {
        if let Ok(mut count) = ACTIVE_TASKS.try_lock() {
            *count = count.saturating_sub(1);
        }
    }

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

        // Увеличиваем счетчик активных задач
        {
            if let Ok(mut count) = ACTIVE_TASKS.try_lock() {
                *count += 1;
            }
        }

        let permit = BLOCKING_SEM.clone().acquire_owned().await.unwrap();
        let input_path_clone = input_path.clone();
        let no_image_path_clone = no_image_path.clone();
        let (buf, ct) = task::spawn_blocking(move || -> Result<(Vec<u8>, String), ApiError> {
            let _permit = permit; // permit автоматически освободится при drop

            // Проверяем размер данных перед обработкой
            let data = if let Some(b) = downloaded {
                if b.len() > MAX_IMAGE_SIZE {
                    return Err(ApiError::BadRequest(format!(
                        "downloaded image too large: {} bytes (max: {} bytes)",
                        b.len(),
                        MAX_IMAGE_SIZE
                    )));
                }
                b
            } else if Path::new(&input_path_clone).exists() {
                let file_data =
                    fs::read(&input_path_clone).map_err(|e| ApiError::Io(e.to_string()))?;
                if file_data.len() > MAX_IMAGE_SIZE {
                    return Err(ApiError::BadRequest(format!(
                        "file image too large: {} bytes (max: {} bytes)",
                        file_data.len(),
                        MAX_IMAGE_SIZE
                    )));
                }
                file_data
            } else {
                let no_img_data =
                    fs::read(&no_image_path_clone).map_err(|e| ApiError::Io(e.to_string()))?;
                if no_img_data.len() > MAX_IMAGE_SIZE {
                    return Err(ApiError::BadRequest(format!(
                        "no-image file too large: {} bytes (max: {} bytes)",
                        no_img_data.len(),
                        MAX_IMAGE_SIZE
                    )));
                }
                no_img_data
            };

            let (mut rgb, tw, th) = {
                let img = if data.is_empty() {
                    DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255])))
                } else {
                    image::load_from_memory(&data).map_err(|e| ApiError::Img(e.to_string()))?
                };

                let (sw, sh) = (img.width(), img.height());

                // Проверяем, что изображение не пустое
                if sw == 0 || sh == 0 {
                    return Err(ApiError::Img("source image has zero dimensions".into()));
                }

                let (tw, th, fg_w, fg_h, need_resize) =
                    target_and_foreground(sw, sh, width, height, max_side);

                // Проверяем, нужен ли ресайз вообще
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
                    // Если ресайз не нужен, возвращаем оригинал
                    return Ok((data, "image/jpeg".to_string()));
                }

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
                // Проверяем, что размеры корректны
                if tw == 0 || th == 0 {
                    return Err(ApiError::Img(
                        "invalid target dimensions for background".into(),
                    ));
                }

                let (r, g, b) = hex_to_rgb(bg_hex_owned.as_deref().unwrap_or("ffffff"));
                let mut bg = RgbImage::from_pixel(tw, th, image::Rgb([r, g, b]));

                // Безопасный расчет позиции для центрирования
                let x = if rgb.width() <= tw {
                    ((tw - rgb.width()) / 2) as i64
                } else {
                    0
                };
                let y = if rgb.height() <= th {
                    ((th - rgb.height()) / 2) as i64
                } else {
                    0
                };

                // Проверяем, что позиция не отрицательная
                if x >= 0 && y >= 0 {
                    imageops::replace(&mut bg, &rgb, x, y);
                    rgb = bg;
                } else {
                    return Err(ApiError::Img(
                        "failed to position image on background".into(),
                    ));
                }
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
                    let mut cur = Cursor::new(&mut buf);
                    let mut encoder =
                        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, JPEG_Q as u8);
                    encoder
                        .encode(
                            &rgb,
                            rgb.width(),
                            rgb.height(),
                            image::ColorType::Rgb8.into(),
                        )
                        .map_err(|e| ApiError::Img(format!("JPEG encoding failed: {}", e)))?;
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

            // Финальная проверка - буфер не должен быть пустым
            if buf.is_empty() {
                return Err(ApiError::Img("generated image buffer is empty".into()));
            }

            Ok((buf, ct))
        })
        .await
        .map_err(|_| ApiError::Internal)??;

        atomic_write(&disk_path, &buf)
            .await
            .map_err(|e| ApiError::Io(e.to_string()))?;
        let mut buf = buf;
        buf.shrink_to_fit();
        Ok((buf, ct))
    })
    .await?;

    // Уменьшаем счетчик активных задач
    {
        if let Ok(mut count) = ACTIVE_TASKS.try_lock() {
            *count = count.saturating_sub(1);
        }
    }

    if !settings.production_mode {
        tracing::info!(
            "get_resize_image_bytes took {}ms",
            started.elapsed().as_millis()
        );
    }
    Ok(result)
}
