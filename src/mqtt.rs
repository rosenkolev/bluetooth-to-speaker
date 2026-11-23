//! MQTT with Home Assistant discovery.

use crate::app::{App, BtCmd};
use crate::checks::{Level, Problem};
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub async fn run(app: Arc<App>) {
    let mut cfg = app.cfg();
    if !cfg.mqtt.enabled {
        // Home Assistant add-on: take the broker from the Supervisor (Mosquitto add-on).
        match tokio::task::spawn_blocking(supervisor_mqtt).await.ok().flatten() {
            Some(m) => {
                log::info!("mqtt: using the broker from the Home Assistant Supervisor");
                cfg.mqtt.enabled = true;
                cfg.mqtt.host = m.host;
                cfg.mqtt.port = m.port;
                cfg.mqtt.username = m.username;
                cfg.mqtt.password = m.password;
            }
            None => return,
        }
    }
    let node = format!("bt2speaker_{}", &app.uid[..8]);
    let base = format!("bluetooth-to-speaker/{node}");
    let avail = format!("{base}/availability");

    let mut opts = MqttOptions::new(node.clone(), cfg.mqtt.host.clone(), cfg.mqtt.port);
    opts.set_keep_alive(Duration::from_secs(30));
    if !cfg.mqtt.username.is_empty() {
        opts.set_credentials(cfg.mqtt.username.clone(), cfg.mqtt.password.clone());
    }
    opts.set_last_will(LastWill::new(avail.clone(), "offline", QoS::AtLeastOnce, true));
    let (client, mut events) = AsyncClient::new(opts, 64);

    // Publish state on every change (debounced); re-announce entities when outputs change.
    {
        let app = app.clone();
        let client = client.clone();
        let topic = format!("{base}/state");
        let (prefix, node, base) = (cfg.mqtt.discovery_prefix.clone(), node.clone(), base.clone());
        tokio::spawn(async move {
            let mut rx = app.changed.subscribe();
            let mut last = String::new();
            let mut gen = app.outputs_gen.load(std::sync::atomic::Ordering::Relaxed);
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                let g = app.outputs_gen.load(std::sync::atomic::Ordering::Relaxed);
                if g != gen {
                    gen = g;
                    publish_discovery(&app, &client, &prefix, &node, &base).await;
                }
                let payload = state_json(&app);
                if payload != last {
                    let _ = client.publish(&topic, QoS::AtMostOnce, true, payload.clone()).await;
                    last = payload;
                }
            }
        });
    }

    loop {
        match events.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                log::info!("mqtt connected to {}:{}", cfg.mqtt.host, cfg.mqtt.port);
                app.clear_problem("mqtt");
                // In its own task: publishing waits for room in the client queue, which only
                // this loop empties. Awaiting it here could block the loop on itself.
                let (app, client) = (app.clone(), client.clone());
                let (prefix, node, base, avail) =
                    (cfg.mqtt.discovery_prefix.clone(), node.clone(), base.clone(), avail.clone());
                tokio::spawn(async move {
                    publish_discovery(&app, &client, &prefix, &node, &base).await;
                    let _ = client.subscribe(format!("{base}/cmd/#"), QoS::AtLeastOnce).await;
                    let _ = client.publish(&avail, QoS::AtLeastOnce, true, "online").await;
                    let _ = client.publish(format!("{base}/state"), QoS::AtMostOnce, true, state_json(&app)).await;
                });
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let payload = String::from_utf8_lossy(&p.payload).trim().to_string();
                let cmd = p.topic.strip_prefix(&format!("{base}/cmd/")).unwrap_or("");
                if let Some(id) = cmd.strip_prefix("output_volume/") {
                    let device =
                        app.cfg().outputs.iter().find(|o| output_id(&o.device) == id).map(|o| o.device.clone());
                    if let (Some(device), Ok(v)) = (device, payload.parse::<f32>()) {
                        if v.is_finite() {
                            app.set_output_volume(&device, v.clamp(0.0, 100.0).round() as u8);
                            app.save_config();
                        }
                    }
                    continue;
                }
                if let Some(id) = cmd.strip_prefix("output/") {
                    let device =
                        app.cfg().outputs.iter().find(|o| output_id(&o.device) == id).map(|o| o.device.clone());
                    if let Some(device) = device {
                        app.enable_output(&device, payload.eq_ignore_ascii_case("ON"));
                    }
                    continue;
                }
                match cmd {
                    "pair" => {
                        let _ = app.bt_tx.send(BtCmd::OpenPairing);
                    }
                    "volume" => {
                        if let Ok(v) = payload.parse::<f32>() {
                            if v.is_finite() {
                                app.set_volume(v.clamp(0.0, 100.0).round() as u8, false);
                            }
                        }
                    }
                    "media" => {
                        if ["play", "pause", "next", "previous"].contains(&payload.as_str()) {
                            let _ = app.bt_tx.send(BtCmd::Media(payload));
                        }
                    }
                    "disconnect" => {
                        let active = app.status.lock().unwrap().active_device.clone();
                        if let Some(a) = active {
                            let _ = app.bt_tx.send(BtCmd::Disconnect(a));
                        }
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::debug!("mqtt: {e}");
                let auth = matches!(
                    &e,
                    rumqttc::ConnectionError::ConnectionRefused(
                        rumqttc::ConnectReturnCode::BadUserNamePassword | rumqttc::ConnectReturnCode::NotAuthorized
                    )
                );
                let (title, fix) = if auth {
                    ("MQTT broker refuses the login", "# web UI: Settings > MQTT: check the username and password")
                } else {
                    ("Cannot reach the MQTT broker", "# web UI: Settings > MQTT: check the broker host and port")
                };
                app.set_problem(
                    Problem::new(
                        "mqtt",
                        Level::Warning,
                        title,
                        format!(
                            "{}:{}: {e}. Home Assistant controls (MQTT) do not work; DLNA playback does.",
                            cfg.mqtt.host, cfg.mqtt.port
                        ),
                    )
                    .fix(&[fix])
                    .help("home-assistant-entities-are-missing"),
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

struct SupervisorMqtt {
    host: String,
    port: u16,
    username: String,
    password: String,
}

/// MQTT service from the Home Assistant Supervisor, when running as an add-on.
fn supervisor_mqtt() -> Option<SupervisorMqtt> {
    let token = std::env::var("SUPERVISOR_TOKEN").ok()?;
    let body = ureq::get("http://supervisor/services/mqtt")
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(Duration::from_secs(5))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let d = v.get("data")?;
    Some(SupervisorMqtt {
        host: d.get("host")?.as_str()?.to_string(),
        port: d.get("port").and_then(|p| p.as_u64()).unwrap_or(1883) as u16,
        username: d.get("username").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
        password: d.get("password").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
    })
}

fn state_json(app: &App) -> String {
    let st = app.status.lock().unwrap().clone();
    let phone = st
        .active_device
        .as_ref()
        .and_then(|a| st.devices.iter().find(|d| &d.address == a))
        .map(|d| d.name.clone())
        .or_else(|| st.devices.iter().find(|d| d.connected).map(|d| d.name.clone()))
        .unwrap_or_else(|| "none".into());
    let track = match app.output.current() {
        crate::app::Source::Network => st.network.map(|n| n.title).unwrap_or_default(),
        _ if !st.track.title.is_empty() => {
            if st.track.artist.is_empty() {
                st.track.title
            } else {
                format!("{} — {}", st.track.artist, st.track.title)
            }
        }
        _ => String::new(),
    };
    let outputs: serde_json::Map<String, serde_json::Value> =
        app.cfg().outputs.iter().map(|o| (output_id(&o.device), json!(if o.enabled { "ON" } else { "OFF" }))).collect();
    let output_volumes: serde_json::Map<String, serde_json::Value> =
        app.cfg().outputs.iter().map(|o| (output_id(&o.device), json!(o.volume))).collect();
    // Problems that need the user (info-level hints stay in the web UI).
    let problems: Vec<String> =
        app.problems().into_iter().filter(|p| p.level != Level::Info).map(|p| p.title).collect();
    json!({
        "problem": problems.first().cloned().unwrap_or_else(|| "none".into()),
        "problems": problems,
        "volume": app.volume(),
        "source": app.output.current(),
        "phone": phone,
        "track": track,
        "pairing": app.pairing_open(),
        "outputs": outputs,
        "output_volumes": output_volumes,
    })
    .to_string()
}

/// MQTT-safe id for an ALSA device name.
fn output_id(device: &str) -> String {
    device.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
}

async fn publish_discovery(app: &App, client: &AsyncClient, prefix: &str, node: &str, base: &str) {
    let name = app.cfg().name;
    let device = json!({
        "identifiers": [node],
        "name": name,
        "manufacturer": "bluetooth-to-speaker",
        "model": "Bluetooth speaker",
        "sw_version": crate::VERSION,
        "configuration_url": format!("http://{}:{}/", crate::net::local_ip().map(|i| i.to_string()).unwrap_or_default(), app.cfg().http_port),
    });
    let common = |id: &str, ent_name: &str| {
        json!({
            "unique_id": format!("{node}_{id}"),
            "name": ent_name,
            "availability_topic": format!("{base}/availability"),
            "device": device,
        })
    };
    let state = format!("{base}/state");
    let mut entities: Vec<(&str, &str, serde_json::Value)> = Vec::new();

    let mut e = common("pair", "Pair new phone");
    e["command_topic"] = json!(format!("{base}/cmd/pair"));
    e["icon"] = json!("mdi:bluetooth-connect");
    entities.push(("button", "pair", e));

    let mut e = common("volume", "Volume");
    e["command_topic"] = json!(format!("{base}/cmd/volume"));
    e["state_topic"] = json!(state);
    e["value_template"] = json!("{{ value_json.volume }}");
    e["min"] = json!(0);
    e["max"] = json!(100);
    e["icon"] = json!("mdi:volume-high");
    entities.push(("number", "volume", e));

    for (id, label, tpl, icon) in [
        ("source", "Source", "{{ value_json.source }}", "mdi:speaker"),
        ("phone", "Connected phone", "{{ value_json.phone }}", "mdi:cellphone"),
        ("track", "Now playing", "{{ value_json.track }}", "mdi:music"),
    ] {
        let mut e = common(id, label);
        e["state_topic"] = json!(state);
        e["value_template"] = json!(tpl);
        e["icon"] = json!(icon);
        entities.push(("sensor", id, e));
    }

    // Setup problems: on when something needs the user; the text says what (details in the web UI).
    let mut e = common("problem", "Setup problem");
    e["state_topic"] = json!(state);
    e["value_template"] = json!("{{ 'ON' if value_json.problems | length > 0 else 'OFF' }}");
    e["json_attributes_topic"] = json!(state);
    e["json_attributes_template"] = json!("{{ {'problems': value_json.problems} | tojson }}");
    e["device_class"] = json!("problem");
    e["entity_category"] = json!("diagnostic");
    entities.push(("binary_sensor", "problem", e));

    let mut e = common("problem_text", "Setup problem details");
    e["state_topic"] = json!(state);
    e["value_template"] = json!("{{ value_json.problem }}");
    e["icon"] = json!("mdi:alert-circle-outline");
    e["entity_category"] = json!("diagnostic");
    entities.push(("sensor", "problem_text", e));

    let mut e = common("pairing", "Pairing");
    e["state_topic"] = json!(state);
    e["value_template"] = json!("{{ 'ON' if value_json.pairing else 'OFF' }}");
    e["icon"] = json!("mdi:bluetooth-audio");
    entities.push(("binary_sensor", "pairing", e));

    for (action, label, icon) in [
        ("play", "Play", "mdi:play"),
        ("pause", "Pause", "mdi:pause"),
        ("next", "Next track", "mdi:skip-next"),
        ("previous", "Previous track", "mdi:skip-previous"),
    ] {
        let mut e = common(action, label);
        e["command_topic"] = json!(format!("{base}/cmd/media"));
        e["payload_press"] = json!(action);
        e["icon"] = json!(icon);
        entities.push(("button", action, e));
    }

    let mut e = common("disconnect", "Disconnect phone");
    e["command_topic"] = json!(format!("{base}/cmd/disconnect"));
    e["icon"] = json!("mdi:bluetooth-off");
    entities.push(("button", "disconnect", e));

    // One switch per output: play here on/off.
    let mut switches = Vec::new();
    let mut numbers = Vec::new();
    for o in app.cfg().outputs {
        let id = output_id(&o.device);
        let mut e = common(&format!("output_{id}"), &format!("Output {}", o.name));
        e["command_topic"] = json!(format!("{base}/cmd/output/{id}"));
        e["state_topic"] = json!(state);
        e["value_template"] = json!(format!("{{{{ value_json.outputs['{id}'] | default('OFF') }}}}"));
        e["icon"] = json!("mdi:speaker-wireless");
        switches.push((format!("output_{id}"), e));

        let mut e = common(&format!("output_volume_{id}"), &format!("Volume {}", o.name));
        e["command_topic"] = json!(format!("{base}/cmd/output_volume/{id}"));
        e["state_topic"] = json!(state);
        e["value_template"] = json!(format!("{{{{ value_json.output_volumes['{id}'] | default(0) }}}}"));
        e["min"] = json!(0);
        e["max"] = json!(100);
        e["icon"] = json!("mdi:volume-medium");
        numbers.push((format!("output_volume_{id}"), e));
    }
    for (id, payload) in numbers {
        let topic = format!("{prefix}/number/{node}/{id}/config");
        let _ = client.publish(topic, QoS::AtLeastOnce, true, payload.to_string()).await;
    }

    for (component, id, payload) in entities {
        let topic = format!("{prefix}/{component}/{node}/{id}/config");
        let _ = client.publish(topic, QoS::AtLeastOnce, true, payload.to_string()).await;
    }
    for (id, payload) in switches {
        let topic = format!("{prefix}/switch/{node}/{id}/config");
        let _ = client.publish(topic, QoS::AtLeastOnce, true, payload.to_string()).await;
    }
}
