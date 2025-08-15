use dotenvy::dotenv;
use std::env;

#[derive(Clone, Debug)]
pub struct Settings {
    pub bind_addr: String,
    pub media_base_dir: String, // castom_addres
    pub no_image_file: String,  // relative to media_base_dir, как в Python
    pub max_image_side: u32,
    pub database_url: String,
    pub swagger_enabled: bool,
    pub swagger_title: String,
    pub swagger_version: String,
    pub use_proxies: bool,
    pub production_mode: bool,
}

impl Settings {
    pub fn from_env() -> Self {
        dotenv().ok();
        let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
        let media_base_dir = env::var("MEDIA_BASE_DIR").unwrap_or_else(|_| "/images/".into());
        let no_image_file = env::var("NO_IMAGE_FILE").unwrap_or_else(|_| "no-image-01.jpg".into());
        let max_image_side = env::var("MAX_IMAGE_SIDE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1600);

        let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgres://postgres:0X90uyyz8QKMmV7jlJiQ@65.21.189.170:5432/tradeshow".into()
        });

        let swagger_enabled = env::var("SWAGGER_ENABLED")
            .map(|s| s == "true" || s == "1")
            .unwrap_or(true);

        let swagger_title =
            env::var("SWAGGER_TITLE").unwrap_or_else(|_| "Onlihub Media CDN".into());
        let swagger_version = env::var("SWAGGER_VERSION").unwrap_or_else(|_| "1.0.0".into());

        let use_proxies = env::var("USE_PROXIES")
            .map(|s| s == "true" || s == "1")
            .unwrap_or(false);

        let production_mode = env::var("PRODUCTION_MODE")
            .map(|s| s == "true" || s == "1")
            .unwrap_or(false);

        Self {
            bind_addr,
            media_base_dir,
            no_image_file,
            max_image_side,
            database_url,
            swagger_enabled,
            swagger_title,
            swagger_version,
            use_proxies,
            production_mode,
        }
    }

    pub fn no_image_full_path(&self) -> String {
        format!("{}{}", self.media_base_dir, self.no_image_file)
    }
}
