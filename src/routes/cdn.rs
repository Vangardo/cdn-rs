use actix_files::NamedFile;
use actix_web::{get, post, web, HttpRequest, HttpResponse};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::{
    config::Settings,
    db::Db,
    errors::ApiError,
    imaging::{
        convert_and_save_jpg, force_cleanup_locks, get_locks_stats, get_resize_image_bytes,
        variant_disk_path,
    },
    models::image::{PushImageWrapper, ResponsePushImage},
    util,
};
use bytes::Bytes;
use reqwest::Client;
use std::path::Path;
use std::sync::Arc;
use tokio::{
    fs as tokio_fs,
    io::{AsyncWriteExt, BufWriter},
    sync::Mutex,
};

#[utoipa::path(
    post,
    path = "/images/",
    tag = "CDN",
    request_body = PushImageWrapper,
    responses(
        (status = 200, description = "ok", body = ResponsePushImage),
        (status = 400, description = "bad request", body = ResponsePushImage)
    )
)]
#[post("/images/")]
pub async fn push_image(
    body: web::Json<PushImageWrapper>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
) -> Result<HttpResponse, ApiError> {
    let mut resp = ResponsePushImage::default();
    match convert_and_save_jpg(&body.data.img_base64, &db, &settings).await {
        Ok((id, guid)) => {
            resp.image_id = Some(id);
            resp.guid = Some(guid.to_string());
            resp.comments = Some("Image successfully written to the server".into());
            resp.status = Some("ok".into());
            Ok(HttpResponse::Ok().json(resp))
        }
        Err(e) => {
            resp.comments = Some(e.to_string());
            resp.status = Some("error".into());
            Err(ApiError::BadRequest(e.to_string()))
        }
    }
}

async fn stream_original(
    req: HttpRequest,
    guid: String,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    let uuid = match util::parse_guid(&guid) {
        Some(u) => u,
        None => {
            let file = NamedFile::open_async(settings.no_image_full_path())
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            return Ok(file.into_response(&req));
        }
    };
    let final_path = format!("{}{}.jpg", settings.media_base_dir, uuid);
    if tokio_fs::try_exists(&final_path).await.unwrap_or(false) {
        let file = NamedFile::open_async(final_path)
            .await
            .map_err(|e| ApiError::Io(e.to_string()))?;
        return Ok(file.into_response(&req));
    }

    let link = match db.get_original_link_by_guid(uuid).await? {
        Some(l) => l,
        None => {
            let file = NamedFile::open_async(settings.no_image_full_path())
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            return Ok(file.into_response(&req));
        }
    };
    let resp = match client.get(&link).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => {
            let file = NamedFile::open_async(settings.no_image_full_path())
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            return Ok(file.into_response(&req));
        }
    };
    if let Some(parent) = Path::new(&final_path).parent() {
        tokio_fs::create_dir_all(parent)
            .await
            .map_err(|e| ApiError::Io(e.to_string()))?;
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let tmp_path = format!("{}.part", final_path);
    let state = Arc::new(Mutex::new((
        BufWriter::new(
            tokio_fs::File::create(&tmp_path)
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?,
        ),
        0usize,
        false,
    )));
    const MAX_STREAM_SIZE: usize = 100 * 1024 * 1024; // 100MB limit для stream
    let stream_state = state.clone();
    let stream = resp.bytes_stream().then(move |item| {
        let state = stream_state.clone();
        async move {
            match item {
                Ok(chunk) => {
                    let mut guard = state.lock().await;
                    if guard.2 {
                        return Err(actix_web::error::ErrorInternalServerError("writer failed"));
                    }
                    guard.1 += chunk.len();
                    if guard.1 > MAX_STREAM_SIZE {
                        tracing::warn!("Stream exceeded size limit: {}", guard.1);
                        guard.2 = true;
                        return Err(actix_web::error::ErrorPayloadTooLarge("stream too big"));
                    }
                    if let Err(e) = guard.0.write_all(&chunk).await {
                        tracing::error!("Failed to write chunk: {e}");
                        guard.2 = true;
                        return Err(actix_web::error::ErrorInternalServerError(e));
                    }
                    Ok::<Bytes, actix_web::Error>(chunk)
                }
                Err(e) => {
                    let mut guard = state.lock().await;
                    guard.2 = true;
                    Err(actix_web::error::ErrorInternalServerError(e))
                }
            }
        }
    });
    let finalize_state = state.clone();
    tokio::spawn(async move {
        use tokio::time::{sleep, Duration};
        loop {
            if Arc::strong_count(&finalize_state) == 1 {
                let mut guard = finalize_state.lock().await;
                let was_error = guard.2;
                let _ = guard.0.flush().await;
                let _ = guard.0.get_ref().sync_all().await;
                drop(guard);
                if was_error {
                    let _ = tokio_fs::remove_file(&tmp_path).await;
                } else {
                    let _ = tokio_fs::rename(&tmp_path, &final_path).await;
                }
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
    });
    Ok(HttpResponse::Ok().content_type(ct).streaming(stream))
}

#[utoipa::path(
    get,
    path = "/{img_guid}/{parametr_images}",
    tag = "CDN",
    responses(
        (status = 200, description = "image", content_type = "image/jpeg")
    )
)]
#[get("/{img_guid}/{parametr_images}")]
pub async fn get_resize_image_root(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    let (guid, param) = path.into_inner();
    let mut parts = param.split('x');
    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
    let w = parts.next().and_then(|s| s.parse::<u32>().ok());

    let cap = |v: Option<u32>| v.map(|x| x.min(settings.max_image_side));
    let w_cap = cap(w);
    let h_cap = cap(h);

    if let Some(uuid) = util::parse_guid(&guid) {
        let (disk_path, _) =
            variant_disk_path(&settings, uuid, w_cap, h_cap, Some("JPEG"), Some("ffffff"));
        if tokio_fs::try_exists(&disk_path).await.unwrap_or(false) {
            let file = NamedFile::open_async(disk_path)
                .await
                .map_err(|e| ApiError::Io(e.to_string()))?;
            return Ok(file.into_response(&req));
        }
    }

    let (bytes, ct) = get_resize_image_bytes(
        &guid,
        w_cap,
        h_cap,
        Some("JPEG"),
        Some("ffffff"),
        &db,
        &settings,
        &client,
    )
    .await?;
    Ok(HttpResponse::Ok().content_type(ct).body(bytes))
}

#[utoipa::path(
    get,
    path = "/{img_guid}/",
    tag = "CDN",
    responses((status = 200, description = "image", content_type = "image/jpeg"))
 )]
#[get("/{img_guid}/")]
pub async fn get_original_image_root(
    req: HttpRequest,
    path: web::Path<String>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    stream_original(req, path.into_inner(), db, settings, client).await
}

#[derive(Deserialize)]
pub struct FormatQuery {
    #[serde(rename = "odnWidth")]
    pub odn_width: Option<u32>,
    #[serde(rename = "odnHeight")]
    pub odn_height: Option<u32>,
    #[serde(rename = "odnBg")]
    pub odn_bg: Option<String>,
}

#[utoipa::path(
    get,
    path = "/{img_guid}.{format}",
    tag = "CDN",
    responses((status=200, description="image", content_type="image/*"))
 )]
#[get("/{img_guid}.{format}")]
pub async fn get_image_with_format(
    path: web::Path<(String, String)>,
    q: web::Query<FormatQuery>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    let (guid, mut format) = path.into_inner();
    let valid = ["JPEG", "PNG", "GIF", "TIFF", "WebP"];
    if !valid.iter().any(|v| v.eq_ignore_ascii_case(&format)) {
        format = "jpeg".into();
    }
    let (bytes, ct) = get_resize_image_bytes(
        &guid,
        q.odn_width,
        q.odn_height,
        Some(&format),
        q.odn_bg.as_deref(),
        &db,
        &settings,
        &client,
    )
    .await?;
    Ok(HttpResponse::Ok().content_type(ct).body(bytes))
}

#[utoipa::path(
    get,
    path = "/images/{img_guid}/{parametr_images}",
    tag = "CDN",
    responses((status = 200, description="image", content_type="image/jpeg"))
 )]
#[get("/images/{img_guid}/{parametr_images}")]
pub async fn get_resize_image_prefixed(
    path: web::Path<(String, String)>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    let (guid, param) = path.into_inner();
    let mut parts = param.split('x');
    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
    let w = parts.next().and_then(|s| s.parse::<u32>().ok());
    let cap = |v: Option<u32>| v.map(|x| x.min(settings.max_image_side));

    let (bytes, ct) = get_resize_image_bytes(
        &guid,
        cap(w),
        cap(h),
        Some("JPEG"),
        Some("ffffff"),
        &db,
        &settings,
        &client,
    )
    .await?;
    Ok(HttpResponse::Ok().content_type(ct).body(bytes))
}

#[utoipa::path(
    get,
    path = "/images/{img_guid}/",
    tag = "CDN",
    responses((status = 200, description="image", content_type="image/jpeg"))
 )]
#[get("/images/{img_guid}/")]
pub async fn get_original_image_prefixed(
    req: HttpRequest,
    path: web::Path<String>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    stream_original(req, path.into_inner(), db, settings, client).await
}

#[utoipa::path(
    get,
    path = "/health/memory",
    tag = "System",
    responses((status = 200, description = "Memory and locks statistics"))
)]
#[get("/health/memory")]
pub async fn get_memory_stats() -> Result<HttpResponse, ApiError> {
    let (lock_count, cleanup_counter, active_tasks) = get_locks_stats();

    #[cfg(target_os = "linux")]
    let memory_info = {
        if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
            if let Some(mem_line) = contents.lines().find(|line| line.starts_with("VmRSS:")) {
                if let Some(kb_str) = mem_line.split_whitespace().nth(1) {
                    if let Ok(kb) = kb_str.parse::<u64>() {
                        format!("{} MB", kb / 1024)
                    } else {
                        "Unknown".to_string()
                    }
                } else {
                    "Unknown".to_string()
                }
            } else {
                "Unknown".to_string()
            }
        } else {
            "Unknown".to_string()
        }
    };

    #[cfg(not(target_os = "linux"))]
    let memory_info = "Not available on this platform".to_string();

    let stats = serde_json::json!({
        "memory_usage": memory_info,
        "active_locks": lock_count,
        "cleanup_counter": cleanup_counter,
        "active_tasks": active_tasks,
        "timestamp": chrono::Utc::now().to_rfc3339()
    });

    Ok(HttpResponse::Ok().json(stats))
}

#[utoipa::path(
    post,
    path = "/health/cleanup",
    tag = "System",
    responses((status = 200, description = "Cleanup completed"))
)]
#[post("/health/cleanup")]
pub async fn force_cleanup() -> Result<HttpResponse, ApiError> {
    force_cleanup_locks().await;

    let response = serde_json::json!({
        "status": "cleanup_completed",
        "message": "All locks have been force cleaned",
        "timestamp": chrono::Utc::now().to_rfc3339()
    });

    Ok(HttpResponse::Ok().json(response))
}
