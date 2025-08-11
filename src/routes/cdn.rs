use actix_web::{get, post, web, HttpResponse};
use serde::Deserialize;

use crate::{
    config::Settings,
    db::Db,
    errors::ApiError,
    imaging::{convert_and_save_jpg, get_resize_image_bytes},
    models::image::{PushImageWrapper, ResponsePushImage},
};
use reqwest::Client;

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
    path: web::Path<(String, String)>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    let (guid, param) = path.into_inner();
    let mut parts = param.split('x');
    let h = parts.next().and_then(|s| s.parse::<u32>().ok());
    let w = parts.next().and_then(|s| s.parse::<u32>().ok());

    // лимиты 1600 как в Python
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
    path = "/{img_guid}/",
    tag = "CDN",
    responses((status = 200, description = "image", content_type = "image/jpeg"))
 )]
#[get("/{img_guid}/")]
pub async fn get_original_image_root(
    path: web::Path<String>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    let guid = path.into_inner();
    let (bytes, ct) = get_resize_image_bytes(
        &guid,
        None,
        None,
        Some("JPEG"),
        Some("ffffff"),
        &db,
        &settings,
        &client,
    )
    .await?;
    Ok(HttpResponse::Ok().content_type(ct).body(bytes))
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
    path: web::Path<String>,
    db: web::Data<Db>,
    settings: web::Data<Settings>,
    client: web::Data<Client>,
) -> Result<HttpResponse, ApiError> {
    let guid = path.into_inner();
    let (bytes, ct) = get_resize_image_bytes(
        &guid,
        None,
        None,
        Some("JPEG"),
        Some("ffffff"),
        &db,
        &settings,
        &client,
    )
    .await?;
    Ok(HttpResponse::Ok().content_type(ct).body(bytes))
}
