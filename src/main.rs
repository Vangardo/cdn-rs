mod config;
mod db;
mod errors;
mod imaging;
mod models;
mod openapi;
mod proxy;
mod routes;
mod sentry;
mod util;

use actix_web::{middleware::Logger, web, App, HttpServer};
use config::Settings;
use db::Db;
use tracing_subscriber::{fmt, EnvFilter};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // логи
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    let settings = Settings::from_env();

    // Sentry
    let _guard = sentry::init_sentry(settings.sentry_dsn.clone());
    if settings.sentry_dsn.is_some() {
        tracing::info!("Sentry enabled");
    }

    // DB pool
    let db = Db::connect(&settings.database_url)
        .await
        .expect("db connect failed");

    // OpenAPI
    let openapi = openapi::ApiDoc::openapi();

    let bind_addr = settings.bind_addr.clone();
    tracing::info!("Listening on {}", bind_addr);

    HttpServer::new(move || {
        let swagger = if settings.swagger_enabled {
            Some(utoipa_swagger_ui::SwaggerUi::new("/docs/{_:.*}").url("/api-docs/openapi.json", openapi.clone()))
        } else {
            None
        };

        let mut app = App::new()
            .app_data(web::Data::new(settings.clone()))
            .app_data(web::Data::new(db.clone()))
            .wrap(Logger::default())
            // порядок важен, чтобы /images/... не перехватывался общим мэчером
            .service(routes::cdn::push_image)
            .service(routes::cdn::get_resize_image_prefixed)
            .service(routes::cdn::get_original_image_prefixed)
            .service(routes::cdn::get_image_with_format)
            .service(routes::cdn::get_resize_image_root)
            .service(routes::cdn::get_original_image_root);

        if let Some(sw) = swagger {
            app = app.service(sw).route("/api-docs/openapi.json", web::get().to({
                let json = serde_json::to_string(&openapi).unwrap();
                move || async move { actix_web::HttpResponse::Ok().content_type("application/json").body(json.clone()) }
            }));
        }

        app
    })
        .workers(num_cpus::get().max(4)) // разумное количество воркеров
        .bind(bind_addr)?
        .run()
        .await
}
