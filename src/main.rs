//! jff-portal: an xdg-desktop-portal ScreenCast backend for jff that can share a single window.
//!
//! See `portal/README.md`.

/// `JFF_PORTAL_DEBUG=1` turns on the `[jff-portal-debug]` lines on stderr.
pub fn debug_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("JFF_PORTAL_DEBUG")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

macro_rules! jdbg {
    ($($arg:tt)*) => {
        if crate::debug_on() {
            eprintln!("[jff-portal-debug] {}", format!($($arg)*));
        }
    };
}

mod capture;
mod config;
mod dbus;
mod model;
mod stream;
mod wayland;

use anyhow::{Context as _, Result};
use tracing_subscriber::EnvFilter;

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.jff";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = config::load()
        .map_err(anyhow::Error::msg)
        .context("bad configuration")?;
    tracing::info!("picker: {:?}", config.chooser);

    let wl = wayland::spawn().context("could not set up the Wayland side")?;
    let backend = dbus::Backend::new(wl, config);

    zbus::block_on(async {
        let connection = zbus::connection::Builder::session()
            .context("no D-Bus session bus")?
            .name(BUS_NAME)
            .with_context(|| format!("bad bus name {BUS_NAME}"))?
            .serve_at(
                OBJECT_PATH,
                dbus::ScreenCast {
                    backend: backend.clone(),
                },
            )
            .context("could not export the ScreenCast object")?
            .build()
            .await
            .with_context(|| {
                format!("could not own {BUS_NAME} (is another jff-portal running?)")
            })?;
        backend.set_connection(connection);
        tracing::info!("jff-portal is up as {BUS_NAME}");
        std::future::pending::<()>().await;
        Ok(())
    })
}
