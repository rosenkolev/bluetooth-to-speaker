/// Release version: APP_VERSION at build time (set by CI from the git tag), else Cargo.toml.
pub const VERSION: &str = match option_env!("APP_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

mod app;
mod bluez;
mod config;
mod dlna;
mod group;
mod mqtt;
mod net;
mod output;
mod player;
mod sbc;
mod ssdp;
mod web;

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).format_timestamp_secs().init();

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        log::error!("cannot create data dir {}: {e}", data_dir.display());
    }
    let cfg = config::Config::load(&data_dir);
    cfg.save(&data_dir);
    log::info!("bluetooth-to-speaker {} starting as \"{}\"", crate::VERSION, cfg.name);

    let (app, bt_rx) = app::App::new(data_dir, cfg.clone());
    app.restore_default_volumes();

    // One thread for all control work; audio runs on its own threads.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async move {
        tokio::spawn(bluez::run(app.clone(), bt_rx));
        tokio::spawn(web::run(app.clone()));
        tokio::spawn(mqtt::run(app.clone()));
        if cfg.dlna.enabled {
            tokio::spawn(ssdp::run(app.clone()));
        }
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        log::info!("stopping");
        app.output.stop_source(app::Source::Bluetooth);
        app.output.stop_source(app::Source::Network);
        app.save_config();
    });
}
