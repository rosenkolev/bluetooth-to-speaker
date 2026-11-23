/// Release version: APP_VERSION at build time (set by CI from the git tag), else Cargo.toml.
pub const VERSION: &str = match option_env!("APP_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

mod app;
mod bluez;
mod checks;
mod config;
mod dlna;
mod group;
mod mqtt;
mod net;
mod output;
mod player;
mod privilege;
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
    // Before any thread starts: become a normal user when that is safe. Then read the
    // settings as that user (the files are now theirs).
    let privilege = privilege::drop_root(&data_dir, config::Config::peek_http_port(&data_dir));
    let cfg = config::Config::load(&data_dir);
    cfg.save(&data_dir);
    log::info!("bluetooth-to-speaker {} starting as \"{}\"", crate::VERSION, cfg.name);

    let (app, bt_rx) = app::App::new(data_dir, cfg.clone(), privilege);
    app.restore_default_volumes();

    // One thread for all control work; audio runs on its own threads.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async move {
        tokio::spawn(app.clone().run_saver());
        tokio::spawn(bluez::run(app.clone(), bt_rx));
        tokio::spawn(web::run(app.clone()));
        tokio::spawn(mqtt::run(app.clone()));
        tokio::spawn(checks::run(app.clone()));
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
        app.flush_config();
    });
}
