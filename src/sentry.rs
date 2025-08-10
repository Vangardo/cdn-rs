use sentry::{ClientInitGuard, IntoDsn};
use std::time::Duration;

pub fn init_sentry(dsn: Option<String>) -> Option<ClientInitGuard> {
    dsn.as_ref()?;
    let guard = sentry::init((
        dsn.into_dsn().ok()?,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            traces_sample_rate: 0.1,
            shutdown_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    ));
    Some(guard)
}
