mod config;
mod db;
mod errors;
mod imaging;
mod models;
mod openapi;
mod proxy;
mod routes;
mod util;

use actix_web::{middleware::Logger, web, App, HttpServer};
use config::Settings;
use db::Db;
use reqwest::Client;
use tracing_subscriber::{fmt, EnvFilter};
use utoipa::OpenApi;
use std::time::Duration;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // логи
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    let settings = Settings::from_env();

    // DB pool
    let db = Db::connect(&settings.database_url)
        .await
        .expect("db connect failed");

    let mut client_builder = Client::builder().timeout(std::time::Duration::from_secs(4));
    if settings.use_proxies {
        if let Some(px) = proxy::random_proxy() {
            if let Ok(proxy) = reqwest::Proxy::all(&px) {
                client_builder = client_builder.proxy(proxy);
            }
        }
    }
    let client = client_builder.build().expect("reqwest client build failed");

    // OpenAPI
    let mut openapi = openapi::ApiDoc::openapi();
    openapi.info.title = settings.swagger_title.clone();
    openapi.info.version = settings.swagger_version.clone();

    let bind_addr = settings.bind_addr.clone();
    tracing::info!("Listening on {}", bind_addr);

    // Запускаем мониторинг памяти
    if !settings.production_mode {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30)); // Увеличиваем частоту мониторинга
            loop {
                interval.tick().await;
                
                // Получаем информацию о памяти (только для Linux/Unix)
                #[cfg(target_os = "linux")]
                {
                    if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
                        if let Some(mem_line) = contents.lines().find(|line| line.starts_with("VmRSS:")) {
                            if let Some(kb_str) = mem_line.split_whitespace().nth(1) {
                                if let Ok(kb) = kb_str.parse::<u64>() {
                                    let mb = kb / 1024;
                                    tracing::info!("Memory usage: {} MB", mb);
                                }
                            }
                        }
                    }
                }
                
                // Для Windows используем другой подход
                #[cfg(target_os = "windows")]
                {
                    // Windows не предоставляет простой способ получить RSS, 
                    // но можно использовать системные API если нужно
                    tracing::debug!("Memory monitoring on Windows (not implemented)");
                }
                
                // Если память превышает 500MB, логируем предупреждение
                #[cfg(target_os = "linux")]
                {
                    if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
                        if let Some(mem_line) = contents.lines().find(|line| line.starts_with("VmRSS:")) {
                            if let Some(kb_str) = mem_line.split_whitespace().nth(1) {
                                if let Ok(kb) = kb_str.parse::<u64>() {
                                    let mb = kb / 1024;
                                    if mb > 500 {
                                        tracing::warn!("High memory usage detected: {} MB", mb);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    HttpServer::new(move || {
        let swagger = if settings.swagger_enabled {
            Some(
                utoipa_swagger_ui::SwaggerUi::new("/docs/{_:.*}")
                    .url("/api-docs/openapi.json", openapi.clone()),
            )
        } else {
            None
        };

        let mut app = App::new()
            .app_data(web::Data::new(settings.clone()))
            .app_data(web::Data::new(db.clone()))
            .app_data(web::Data::new(client.clone()))
            .wrap(Logger::new("%r %s %Dms"))
            // порядок важен, чтобы /images/... не перехватывался общим мэчером
            .service(routes::cdn::push_image)
            .service(routes::cdn::get_resize_image_prefixed)
            .service(routes::cdn::get_original_image_prefixed)
            .service(routes::cdn::get_image_with_format)
            .service(routes::cdn::get_resize_image_root)
            .service(routes::cdn::get_original_image_root)
            .service(routes::cdn::get_memory_stats)
            .service(routes::cdn::force_cleanup);

        if let Some(sw) = swagger {
            app = app.service(sw).route(
                "/api-docs/openapi.json",
                web::get().to({
                    let json = serde_json::to_string(&openapi).unwrap();
                    move || {
                        let json = json.clone();
                        async move {
                            actix_web::HttpResponse::Ok()
                                .content_type("application/json")
                                .body(json)
                        }
                    }
                }),
            );
        }

        app
    })
    .workers(num_cpus::get().min(8)) // ограничиваем количество воркеров для предотвращения переполнения памяти
    .bind(bind_addr)?
    .run()
    .await
}
