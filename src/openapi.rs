use crate::models::image::{PushImageWrapper, ResponsePushImage};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::routes::cdn::push_image,
        crate::routes::cdn::get_resize_image_root,
        crate::routes::cdn::get_original_image_root,
        crate::routes::cdn::get_image_with_format,
        crate::routes::cdn::get_resize_image_prefixed,
        crate::routes::cdn::get_original_image_prefixed,
    ),
    components(
        schemas(PushImageWrapper, ResponsePushImage)
    ),
    tags(
        (name = "CDN", description = "Onlihub Media CDN endpoints")
    )
)]
pub struct ApiDoc;
