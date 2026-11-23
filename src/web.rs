//! Web UI and JSON API.

use crate::app::{App, BtCmd};
use crate::config::{safe_alsa_device, safe_alsa_mixer, Config};
use axum::extract::{Path, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;

const INDEX: &str = include_str!("../web/index.html");

pub async fn run(app: Arc<App>) {
    let port = app.cfg().http_port;
    let router = Router::new()
        .route("/", get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX) }))
        .route("/api/state", get(state))
        .route("/api/outputs", get(outputs))
        .route("/api/outputs/enable", post(enable_output))
        .route("/api/outputs/volume", post(output_volume))
        .route("/api/outputs/restore", post(restore_volumes))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/pair", post(pair))
        .route("/api/pair/cancel", post(pair_cancel))
        .route("/api/volume", post(volume))
        .route("/api/media/{action}", post(media))
        .route("/api/devices/{addr}/{action}", post(device))
        .route("/api/checks", post(run_checks))
        .layer(axum::middleware::from_fn(same_site_only))
        .with_state(app.clone());
    let router = if app.cfg().dlna.enabled { router.merge(crate::dlna::router(app.clone())) } else { router };
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("web UI cannot bind port {port}: {e}");
            return;
        }
    };
    log::info!("web UI on http://0.0.0.0:{port}");
    if let Err(e) = axum::serve(listener, router).await {
        log::error!("web server: {e}");
    }
}

/// Refuse changes requested by other web sites (cross-site request forgery): a page on
/// the internet must not be able to open pairing or remove phones through the browser of
/// someone at home. Browsers mark such requests with `Sec-Fetch-Site: cross-site`; older
/// ones are checked by `Origin`. Requests through Home Assistant ingress are allowed.
async fn same_site_only(req: Request, next: Next) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let h = req.headers();
        let text = |name: &str| h.get(name).and_then(|v| v.to_str().ok());
        let cross = match text("sec-fetch-site") {
            Some(site) => site == "cross-site",
            None => match (text("origin"), text("host")) {
                (Some(origin), Some(host)) => {
                    text("x-ingress-path").is_none() && origin.split("://").nth(1) != Some(host)
                }
                _ => false, // not a browser (curl, Home Assistant)
            },
        };
        if cross {
            return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
        }
    }
    next.run(req).await
}

async fn state(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    Json(app.snapshot())
}

async fn outputs() -> Response {
    // Opens every sound card's mixer: blocking work, kept off the async thread.
    match tokio::task::spawn_blocking(crate::output::list_devices).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_config(State(app): State<Arc<App>>) -> Json<Config> {
    let mut cfg = app.cfg();
    cfg.mqtt.password = if cfg.mqtt.password.is_empty() { String::new() } else { "********".into() };
    Json(cfg)
}

async fn put_config(State(app): State<Arc<App>>, Json(mut new): Json<Config>) -> Response {
    if let Some(o) = new
        .outputs
        .iter()
        .find(|o| !safe_alsa_device(o.device.trim()) || !(o.mixer.trim().is_empty() || safe_alsa_mixer(o.mixer.trim())))
    {
        let msg = format!("output \"{}\": not a plain ALSA device or mixer name", o.device);
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "error": msg }))).into_response();
    }
    new.sanitize();
    let old = app.cfg();
    if new.mqtt.password == "********" {
        new.mqtt.password = old.mqtt.password.clone();
    }
    let restart_needed = new.adapter != old.adapter
        || new.max_bitpool != old.max_bitpool
        || new.http_port != old.http_port
        || new.dlna.enabled != old.dlna.enabled
        || new.mqtt.enabled != old.mqtt.enabled
        || new.mqtt.host != old.mqtt.host
        || new.mqtt.port != old.mqtt.port
        || new.mqtt.username != old.mqtt.username
        || new.mqtt.password != old.mqtt.password;
    if new.name != old.name {
        let _ = app.bt_tx.send(BtCmd::Rename(new.name.clone()));
    }
    let volume = new.volume;
    let outputs = new.outputs.clone();
    new.outputs = old.outputs.clone();
    *app.cfg.write().unwrap() = new;
    app.set_outputs(outputs);
    app.set_volume(volume, false);
    app.save_config();
    app.notify();
    Json(serde_json::json!({ "ok": true, "restart_needed": restart_needed })).into_response()
}

/// "Check again": run the setup checks now.
async fn run_checks(State(app): State<Arc<App>>) -> StatusCode {
    app.check_now.notify_one();
    StatusCode::NO_CONTENT
}

async fn pair(State(app): State<Arc<App>>) -> StatusCode {
    let _ = app.bt_tx.send(BtCmd::OpenPairing);
    StatusCode::NO_CONTENT
}

async fn pair_cancel(State(app): State<Arc<App>>) -> StatusCode {
    let _ = app.bt_tx.send(BtCmd::ClosePairing);
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct VolumeReq {
    volume: u8,
}

async fn volume(State(app): State<Arc<App>>, Json(req): Json<VolumeReq>) -> StatusCode {
    app.set_volume(req.volume.min(100), false); // saved by the saver task
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct EnableReq {
    device: String,
    enabled: bool,
}

async fn enable_output(State(app): State<Arc<App>>, Json(req): Json<EnableReq>) -> StatusCode {
    app.enable_output(&req.device, req.enabled);
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct OutputVolumeReq {
    device: String,
    volume: u8,
}

async fn output_volume(State(app): State<Arc<App>>, Json(req): Json<OutputVolumeReq>) -> StatusCode {
    if app.set_output_volume(&req.device, req.volume) {
        app.save_config();
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn restore_volumes(State(app): State<Arc<App>>) -> StatusCode {
    app.restore_default_volumes();
    app.save_config();
    StatusCode::NO_CONTENT
}

async fn media(State(app): State<Arc<App>>, Path(action): Path<String>) -> StatusCode {
    match action.as_str() {
        "play" | "pause" | "next" | "previous" => {
            let _ = app.bt_tx.send(BtCmd::Media(action));
            StatusCode::NO_CONTENT
        }
        _ => StatusCode::NOT_FOUND,
    }
}

async fn device(State(app): State<Arc<App>>, Path((addr, action)): Path<(String, String)>) -> StatusCode {
    let valid = addr.len() == 17 && addr.chars().all(|c| c.is_ascii_hexdigit() || c == ':');
    if !valid {
        return StatusCode::BAD_REQUEST;
    }
    let cmd = match action.as_str() {
        "connect" => BtCmd::Connect(addr),
        "disconnect" => BtCmd::Disconnect(addr),
        "remove" => BtCmd::Remove(addr),
        // Ask that phone to play (AVRCP); its stream then takes over the speaker.
        "play" => BtCmd::Resume(addr),
        _ => return StatusCode::NOT_FOUND,
    };
    let _ = app.bt_tx.send(cmd);
    StatusCode::NO_CONTENT
}
