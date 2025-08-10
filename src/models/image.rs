use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct PushImage {
    #[schema(example = "iVBORw0KGgoAAAANSUhEUgAA... (base64)")]
    pub img_base64: String,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct PushImageWrapper {
    pub data: PushImage,
}

#[derive(Debug, Deserialize, Serialize, ToSchema, Default)]
pub struct ResponsePushImage {
    pub image_id: Option<i64>,
    pub guid: Option<String>,
    pub comments: Option<String>,
    pub status: Option<String>, // "ok" | "error"
}
