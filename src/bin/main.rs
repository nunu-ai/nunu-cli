use env_logger::Env;
use log::{debug, info};

use nunu_rust_template::add_safe;

fn init_sentry() -> Option<sentry::ClientInitGuard> {
    // Get DSN from environment, or return early if not set
    let dsn = match std::env::var("SENTRY_DSN") {
        Ok(dsn) => dsn,
        Err(err) => {
            debug!(
                "SENTRY_DSN not set, skipping sentry initialization: {}",
                err
            );
            return None;
        }
    };

    // Initialize Sentry with the default filter level set to info
    Some(sentry::init((
        dsn,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            ..Default::default()
        },
    )))
}

fn main() -> anyhow::Result<()> {
    // Initialize the logger with the default filter level set to info
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    // This is dropped at the end of main() to send any pending events to Sentry
    let _sentry_guard = init_sentry();

    info!("Hello, world!");

    let sum = add_safe(5, 7)?;
    info!("5 + 7 = {}", sum);

    Ok(())
}
