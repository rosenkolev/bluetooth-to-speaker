//! Minimal UPnP/DLNA MediaRenderer so Home Assistant (dlna_dmr) and Music Assistant
//! can play to the speaker. Events are not supported: SUBSCRIBE is rejected and
//! controllers fall back to polling.

use crate::app::{App, BtCmd, NetworkPlayback, Source};
use crate::player::{run_network, NetState};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const CM: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const SINK_PROTOCOLS: &str = "http-get:*:audio/mpeg:*,http-get:*:audio/mp3:*,http-get:*:audio/flac:*,\
http-get:*:audio/x-flac:*,http-get:*:audio/aac:*,http-get:*:audio/mp4:*,http-get:*:audio/x-m4a:*,\
http-get:*:audio/ogg:*,http-get:*:audio/wav:*,http-get:*:audio/x-wav:*,http-get:*:audio/L16:*";

pub struct Dlna {
    app: Arc<App>,
    net: Arc<NetState>,
    media: Mutex<Media>,
}

#[derive(Default, Clone)]
struct Media {
    uri: String,
    meta: String,
    title: String,
    muted_volume: Option<u8>,
}

pub fn router(app: Arc<App>) -> Router {
    let dlna = Arc::new(Dlna { app, net: Arc::new(NetState::default()), media: Mutex::new(Media::default()) });
    Router::new()
        .route("/dlna/description.xml", get(description))
        .route("/dlna/{scpd}", get(scpd))
        .route("/dlna/control/{svc}", post(control))
        .route("/dlna/event/{svc}", any(|| async { StatusCode::NOT_IMPLEMENTED }))
        .with_state(dlna)
}

async fn description(State(d): State<Arc<Dlna>>) -> Response {
    let name = xml_escape(&d.app.cfg().name);
    let uid = &d.app.uid;
    let svc = |ty: &str, id: &str| {
        format!(
            "<service><serviceType>{ty}</serviceType><serviceId>urn:upnp-org:serviceId:{id}</serviceId>\
<SCPDURL>/dlna/{id}.xml</SCPDURL><controlURL>/dlna/control/{id}</controlURL>\
<eventSubURL>/dlna/event/{id}</eventSubURL></service>"
        )
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\
<specVersion><major>1</major><minor>0</minor></specVersion><device>\
<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>\
<friendlyName>{name}</friendlyName><manufacturer>bluetooth-to-speaker</manufacturer>\
<modelName>Bluetooth to Speaker</modelName><modelNumber>{ver}</modelNumber>\
<dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC><UDN>uuid:{uid}</UDN><serviceList>{a}{r}{c}</serviceList>\
</device></root>",
        ver = crate::VERSION,
        a = svc(AVT, "AVTransport"),
        r = svc(RC, "RenderingControl"),
        c = svc(CM, "ConnectionManager"),
    );
    xml(body)
}

async fn scpd(Path(file): Path<String>) -> Response {
    match file.as_str() {
        "AVTransport.xml" => xml(scpd_avtransport()),
        "RenderingControl.xml" => xml(scpd_rendering()),
        "ConnectionManager.xml" => xml(scpd_connection()),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn control(State(d): State<Arc<Dlna>>, Path(svc): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let body = String::from_utf8_lossy(&body);
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim_matches('"').rsplit('#').next())
        .unwrap_or("")
        .to_string();
    let (ns, result) = match svc.as_str() {
        "AVTransport" => (AVT, d.avtransport(&action, &body)),
        "RenderingControl" => (RC, d.rendering(&action, &body)),
        "ConnectionManager" => (CM, d.connection(&action)),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match result {
        Ok(args) => {
            let args: String = args.iter().map(|(k, v)| format!("<{k}>{}</{k}>", xml_escape(v))).collect();
            xml(format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><u:{action}Response xmlns:u=\"{ns}\">{args}</u:{action}Response></s:Body></s:Envelope>"
            ))
        }
        Err((code, desc)) => {
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
<UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
<errorDescription>{desc}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, [(header::CONTENT_TYPE, "text/xml; charset=\"utf-8\"")], body)
                .into_response()
        }
    }
}

type ActionResult = Result<Vec<(&'static str, String)>, (u32, &'static str)>;

impl Dlna {
    fn transport_state(&self) -> &'static str {
        let playing = self.net.playing.load(Ordering::Relaxed);
        if playing && self.net.paused.load(Ordering::Relaxed) {
            "PAUSED_PLAYBACK"
        } else if playing {
            "PLAYING"
        } else if self.media.lock().unwrap().uri.is_empty() {
            "NO_MEDIA_PRESENT"
        } else {
            "STOPPED"
        }
    }

    fn avtransport(self: &Arc<Self>, action: &str, body: &str) -> ActionResult {
        match action {
            "SetAVTransportURI" => {
                let uri = arg(body, "CurrentURI").unwrap_or_default();
                if !(uri.starts_with("http://") || uri.starts_with("https://")) || uri.len() > 8192 {
                    return Err((714, "Illegal MIME-type"));
                }
                let meta = arg(body, "CurrentURIMetaData").unwrap_or_default();
                let title = didl_title(&meta).unwrap_or_default();
                let was_playing = self.net.playing.load(Ordering::Relaxed);
                {
                    let mut m = self.media.lock().unwrap();
                    m.uri = uri.clone();
                    m.meta = meta;
                    m.title = title.clone();
                }
                // Shown in the web UI without the query string, which may hold an access token.
                let shown = uri.split('?').next().unwrap_or_default().to_string();
                self.app.update_status(|s| {
                    s.network = Some(NetworkPlayback { uri: shown, title, state: "STOPPED".into() })
                });
                // Controllers expect a playing renderer to switch to the new URI at once.
                if was_playing {
                    self.start();
                }
                Ok(vec![])
            }
            "Play" => {
                if self.net.playing.load(Ordering::Relaxed) {
                    self.net.paused.store(false, Ordering::Relaxed);
                    self.set_state("PLAYING");
                } else if self.media.lock().unwrap().uri.is_empty() {
                    return Err((701, "Transition not available"));
                } else {
                    self.start();
                }
                Ok(vec![])
            }
            "Pause" => {
                self.net.paused.store(true, Ordering::Relaxed);
                self.set_state("PAUSED_PLAYBACK");
                Ok(vec![])
            }
            "Stop" => {
                self.app.output.stop_source(Source::Network);
                self.set_state("STOPPED");
                Ok(vec![])
            }
            "GetTransportInfo" => Ok(vec![
                ("CurrentTransportState", self.transport_state().into()),
                ("CurrentTransportStatus", "OK".into()),
                ("CurrentSpeed", "1".into()),
            ]),
            "GetPositionInfo" => {
                let m = self.media.lock().unwrap().clone();
                let dur = hms(self.net.duration_ms.load(Ordering::Relaxed));
                let pos = hms(self.net.position_ms.load(Ordering::Relaxed));
                Ok(vec![
                    ("Track", if m.uri.is_empty() { "0" } else { "1" }.into()),
                    ("TrackDuration", dur),
                    ("TrackMetaData", m.meta),
                    ("TrackURI", m.uri),
                    ("RelTime", pos.clone()),
                    ("AbsTime", pos),
                    ("RelCount", "2147483647".into()),
                    ("AbsCount", "2147483647".into()),
                ])
            }
            "GetMediaInfo" => {
                let m = self.media.lock().unwrap().clone();
                Ok(vec![
                    ("NrTracks", if m.uri.is_empty() { "0" } else { "1" }.into()),
                    ("MediaDuration", hms(self.net.duration_ms.load(Ordering::Relaxed))),
                    ("CurrentURI", m.uri),
                    ("CurrentURIMetaData", m.meta),
                    ("NextURI", String::new()),
                    ("NextURIMetaData", String::new()),
                    ("PlayMedium", "NETWORK".into()),
                    ("RecordMedium", "NOT_IMPLEMENTED".into()),
                    ("WriteStatus", "NOT_IMPLEMENTED".into()),
                ])
            }
            "GetTransportSettings" => {
                Ok(vec![("PlayMode", "NORMAL".into()), ("RecQualityMode", "NOT_IMPLEMENTED".into())])
            }
            "GetDeviceCapabilities" => Ok(vec![
                ("PlayMedia", "NETWORK".into()),
                ("RecMedia", "NOT_IMPLEMENTED".into()),
                ("RecQualityModes", "NOT_IMPLEMENTED".into()),
            ]),
            "GetCurrentTransportActions" => {
                let actions = match self.transport_state() {
                    "PLAYING" => "Pause,Stop",
                    "PAUSED_PLAYBACK" => "Play,Stop",
                    "STOPPED" => "Play",
                    _ => "",
                };
                Ok(vec![("Actions", actions.into())])
            }
            _ => Err((401, "Invalid Action")),
        }
    }

    fn rendering(&self, action: &str, body: &str) -> ActionResult {
        match action {
            "GetVolume" => Ok(vec![("CurrentVolume", self.app.volume().to_string())]),
            "SetVolume" => {
                let v: u8 = arg(body, "DesiredVolume").and_then(|v| v.parse().ok()).ok_or((402, "Invalid Args"))?;
                self.media.lock().unwrap().muted_volume = None;
                self.app.set_volume(v.min(100), false);
                Ok(vec![])
            }
            "GetMute" => {
                let muted = self.media.lock().unwrap().muted_volume.is_some();
                Ok(vec![("CurrentMute", if muted { "1" } else { "0" }.into())])
            }
            "SetMute" => {
                let want = matches!(arg(body, "DesiredMute").as_deref(), Some("1") | Some("true") | Some("True"));
                let mut m = self.media.lock().unwrap();
                if want && m.muted_volume.is_none() {
                    m.muted_volume = Some(self.app.volume());
                    self.app.set_volume(0, false);
                } else if !want {
                    if let Some(v) = m.muted_volume.take() {
                        self.app.set_volume(v, false);
                    }
                }
                Ok(vec![])
            }
            "ListPresets" => Ok(vec![("CurrentPresetNameList", "FactoryDefaults".into())]),
            "SelectPreset" => Ok(vec![]),
            _ => Err((401, "Invalid Action")),
        }
    }

    fn connection(&self, action: &str) -> ActionResult {
        match action {
            "GetProtocolInfo" => Ok(vec![("Source", String::new()), ("Sink", SINK_PROTOCOLS.into())]),
            "GetCurrentConnectionIDs" => Ok(vec![("ConnectionIDs", "0".into())]),
            "GetCurrentConnectionInfo" => Ok(vec![
                ("RcsID", "0".into()),
                ("AVTransportID", "0".into()),
                ("ProtocolInfo", String::new()),
                ("PeerConnectionManager", String::new()),
                ("PeerConnectionID", "-1".into()),
                ("Direction", "Input".into()),
                ("Status", "OK".into()),
            ]),
            _ => Err((401, "Invalid Action")),
        }
    }

    fn set_state(&self, state: &str) {
        self.app.update_status(|s| {
            if let Some(n) = s.network.as_mut() {
                n.state = state.into();
            }
        });
    }

    /// Start playing the current URI, taking the output from Bluetooth if needed.
    fn start(self: &Arc<Self>) {
        let uri = self.media.lock().unwrap().uri.clone();
        let (lease, prev) = self.app.output.claim(Source::Network, None);
        if let Some(p) = prev {
            if p.source == Source::Bluetooth {
                if let Some(dev) = p.device {
                    let _ = self.app.bt_tx.send(BtCmd::Preempted(dev.clone()));
                    if self.app.cfg().resume_bluetooth {
                        *self.net.resume.lock().unwrap() = Some(dev);
                    }
                }
            }
        }
        self.net.paused.store(false, Ordering::Relaxed);
        // Mark playing now so polling controllers see PLAYING right away.
        self.net.playing.store(true, Ordering::Relaxed);
        self.set_state("PLAYING");
        let app = self.app.clone();
        let net = self.net.clone();
        // Only the host: media URLs of Home Assistant carry signed access tokens.
        log::info!("dlna play from {}", uri.split('/').nth(2).unwrap_or("?"));
        // Normal priority: HTTP and decoding may burst; only the output writers are real-time.
        let res = std::thread::Builder::new().name("net-audio".into()).spawn(move || run_network(app, uri, net, lease));
        if let Err(e) = res {
            log::error!("cannot start network audio thread: {e}");
        }
    }
}

// ---------------------------------------------------------------- SCPD

struct Var(&'static str, &'static str, &'static [&'static str]);

type Action<'a> = (&'a str, &'a [(&'a str, bool, &'a str)]);

fn scpd_doc(actions: &[Action<'_>], vars: &[Var], extra_vars: &str) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">\
<specVersion><major>1</major><minor>0</minor></specVersion><actionList>",
    );
    for (name, args) in actions {
        s.push_str(&format!("<action><name>{name}</name><argumentList>"));
        for (arg, out, var) in args.iter() {
            let dir = if *out { "out" } else { "in" };
            s.push_str(&format!(
                "<argument><name>{arg}</name><direction>{dir}</direction><relatedStateVariable>{var}</relatedStateVariable></argument>"
            ));
        }
        s.push_str("</argumentList></action>");
    }
    s.push_str("</actionList><serviceStateTable>");
    for Var(name, ty, allowed) in vars {
        let events = if *name == "LastChange" { "yes" } else { "no" };
        s.push_str(&format!("<stateVariable sendEvents=\"{events}\"><name>{name}</name><dataType>{ty}</dataType>"));
        if !allowed.is_empty() {
            s.push_str("<allowedValueList>");
            for v in allowed.iter() {
                s.push_str(&format!("<allowedValue>{v}</allowedValue>"));
            }
            s.push_str("</allowedValueList>");
        }
        s.push_str("</stateVariable>");
    }
    s.push_str(extra_vars);
    s.push_str("</serviceStateTable></scpd>");
    s
}

const I: bool = false;
const O: bool = true;

fn scpd_avtransport() -> String {
    let id = ("InstanceID", I, "A_ARG_TYPE_InstanceID");
    scpd_doc(
        &[
            (
                "SetAVTransportURI",
                &[id, ("CurrentURI", I, "AVTransportURI"), ("CurrentURIMetaData", I, "AVTransportURIMetaData")],
            ),
            ("Play", &[id, ("Speed", I, "TransportPlaySpeed")]),
            ("Pause", &[id]),
            ("Stop", &[id]),
            (
                "GetTransportInfo",
                &[
                    id,
                    ("CurrentTransportState", O, "TransportState"),
                    ("CurrentTransportStatus", O, "TransportStatus"),
                    ("CurrentSpeed", O, "TransportPlaySpeed"),
                ],
            ),
            (
                "GetPositionInfo",
                &[
                    id,
                    ("Track", O, "CurrentTrack"),
                    ("TrackDuration", O, "CurrentTrackDuration"),
                    ("TrackMetaData", O, "CurrentTrackMetaData"),
                    ("TrackURI", O, "CurrentTrackURI"),
                    ("RelTime", O, "RelativeTimePosition"),
                    ("AbsTime", O, "AbsoluteTimePosition"),
                    ("RelCount", O, "RelativeCounterPosition"),
                    ("AbsCount", O, "AbsoluteCounterPosition"),
                ],
            ),
            (
                "GetMediaInfo",
                &[
                    id,
                    ("NrTracks", O, "NumberOfTracks"),
                    ("MediaDuration", O, "CurrentMediaDuration"),
                    ("CurrentURI", O, "AVTransportURI"),
                    ("CurrentURIMetaData", O, "AVTransportURIMetaData"),
                    ("NextURI", O, "NextAVTransportURI"),
                    ("NextURIMetaData", O, "NextAVTransportURIMetaData"),
                    ("PlayMedium", O, "PlaybackStorageMedium"),
                    ("RecordMedium", O, "RecordStorageMedium"),
                    ("WriteStatus", O, "RecordMediumWriteStatus"),
                ],
            ),
            (
                "GetTransportSettings",
                &[id, ("PlayMode", O, "CurrentPlayMode"), ("RecQualityMode", O, "CurrentRecordQualityMode")],
            ),
            (
                "GetDeviceCapabilities",
                &[
                    id,
                    ("PlayMedia", O, "PossiblePlaybackStorageMedia"),
                    ("RecMedia", O, "PossibleRecordStorageMedia"),
                    ("RecQualityModes", O, "PossibleRecordQualityModes"),
                ],
            ),
            ("GetCurrentTransportActions", &[id, ("Actions", O, "CurrentTransportActions")]),
        ],
        &[
            Var(
                "TransportState",
                "string",
                &["STOPPED", "PLAYING", "PAUSED_PLAYBACK", "TRANSITIONING", "NO_MEDIA_PRESENT"],
            ),
            Var("TransportStatus", "string", &["OK", "ERROR_OCCURRED"]),
            Var("TransportPlaySpeed", "string", &["1"]),
            Var("CurrentMediaDuration", "string", &[]),
            Var("AVTransportURI", "string", &[]),
            Var("AVTransportURIMetaData", "string", &[]),
            Var("NextAVTransportURI", "string", &[]),
            Var("NextAVTransportURIMetaData", "string", &[]),
            Var("CurrentTrackDuration", "string", &[]),
            Var("CurrentTrackMetaData", "string", &[]),
            Var("CurrentTrackURI", "string", &[]),
            Var("RelativeTimePosition", "string", &[]),
            Var("AbsoluteTimePosition", "string", &[]),
            Var("RelativeCounterPosition", "i4", &[]),
            Var("AbsoluteCounterPosition", "i4", &[]),
            Var("PlaybackStorageMedium", "string", &["NETWORK", "NONE"]),
            Var("RecordStorageMedium", "string", &["NOT_IMPLEMENTED"]),
            Var("PossiblePlaybackStorageMedia", "string", &[]),
            Var("PossibleRecordStorageMedia", "string", &[]),
            Var("PossibleRecordQualityModes", "string", &[]),
            Var("RecordMediumWriteStatus", "string", &["NOT_IMPLEMENTED"]),
            Var("CurrentPlayMode", "string", &["NORMAL"]),
            Var("CurrentRecordQualityMode", "string", &["NOT_IMPLEMENTED"]),
            Var("CurrentTransportActions", "string", &[]),
            Var("LastChange", "string", &[]),
            Var("A_ARG_TYPE_InstanceID", "ui4", &[]),
        ],
        "<stateVariable sendEvents=\"no\"><name>NumberOfTracks</name><dataType>ui4</dataType>\
<allowedValueRange><minimum>0</minimum><maximum>1</maximum></allowedValueRange></stateVariable>\
<stateVariable sendEvents=\"no\"><name>CurrentTrack</name><dataType>ui4</dataType>\
<allowedValueRange><minimum>0</minimum><maximum>1</maximum><step>1</step></allowedValueRange></stateVariable>",
    )
}

fn scpd_rendering() -> String {
    let id = ("InstanceID", I, "A_ARG_TYPE_InstanceID");
    let ch = ("Channel", I, "A_ARG_TYPE_Channel");
    scpd_doc(
        &[
            ("ListPresets", &[id, ("CurrentPresetNameList", O, "PresetNameList")]),
            ("SelectPreset", &[id, ("PresetName", I, "A_ARG_TYPE_PresetName")]),
            ("GetMute", &[id, ch, ("CurrentMute", O, "Mute")]),
            ("SetMute", &[id, ch, ("DesiredMute", I, "Mute")]),
            ("GetVolume", &[id, ch, ("CurrentVolume", O, "Volume")]),
            ("SetVolume", &[id, ch, ("DesiredVolume", I, "Volume")]),
        ],
        &[
            Var("PresetNameList", "string", &[]),
            Var("LastChange", "string", &[]),
            Var("Mute", "boolean", &[]),
            Var("A_ARG_TYPE_Channel", "string", &["Master"]),
            Var("A_ARG_TYPE_InstanceID", "ui4", &[]),
            Var("A_ARG_TYPE_PresetName", "string", &["FactoryDefaults"]),
        ],
        "<stateVariable sendEvents=\"no\"><name>Volume</name><dataType>ui2</dataType>\
<allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange></stateVariable>",
    )
}

fn scpd_connection() -> String {
    scpd_doc(
        &[
            ("GetProtocolInfo", &[("Source", O, "SourceProtocolInfo"), ("Sink", O, "SinkProtocolInfo")]),
            ("GetCurrentConnectionIDs", &[("ConnectionIDs", O, "CurrentConnectionIDs")]),
            (
                "GetCurrentConnectionInfo",
                &[
                    ("ConnectionID", I, "A_ARG_TYPE_ConnectionID"),
                    ("RcsID", O, "A_ARG_TYPE_RcsID"),
                    ("AVTransportID", O, "A_ARG_TYPE_AVTransportID"),
                    ("ProtocolInfo", O, "A_ARG_TYPE_ProtocolInfo"),
                    ("PeerConnectionManager", O, "A_ARG_TYPE_ConnectionManager"),
                    ("PeerConnectionID", O, "A_ARG_TYPE_ConnectionID"),
                    ("Direction", O, "A_ARG_TYPE_Direction"),
                    ("Status", O, "A_ARG_TYPE_ConnectionStatus"),
                ],
            ),
        ],
        &[
            Var("SourceProtocolInfo", "string", &[]),
            Var("SinkProtocolInfo", "string", &[]),
            Var("CurrentConnectionIDs", "string", &[]),
            Var(
                "A_ARG_TYPE_ConnectionStatus",
                "string",
                &["OK", "ContentFormatMismatch", "InsufficientBandwidth", "UnreliableChannel", "Unknown"],
            ),
            Var("A_ARG_TYPE_ConnectionManager", "string", &[]),
            Var("A_ARG_TYPE_Direction", "string", &["Input", "Output"]),
            Var("A_ARG_TYPE_ProtocolInfo", "string", &[]),
            Var("A_ARG_TYPE_ConnectionID", "i4", &[]),
            Var("A_ARG_TYPE_AVTransportID", "i4", &[]),
            Var("A_ARG_TYPE_RcsID", "i4", &[]),
        ],
        "",
    )
}

// ---------------------------------------------------------------- helpers

fn xml(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/xml; charset=\"utf-8\"")], body).into_response()
}

pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            _ => o.push(c),
        }
    }
    o
}

fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Value of a SOAP argument element (namespace prefixes tolerated).
fn arg(body: &str, name: &str) -> Option<String> {
    let mut search = 0;
    while let Some(pos) = body[search..].find(name) {
        let start = search + pos;
        search = start + name.len();
        // Must be an opening tag: "<name" or "<prefix:name", followed by '>', ' ' or '/'.
        let before = &body[..start];
        let lt = before.rfind('<')?;
        let tag_prefix = &before[lt + 1..];
        if tag_prefix.starts_with('/') || tag_prefix.contains(|c: char| c.is_whitespace() || c == '>') {
            continue;
        }
        let after = &body[search..];
        let gt = after.find('>')?;
        if after[..gt].ends_with('/') {
            return Some(String::new());
        }
        if !(after.starts_with('>') || after.starts_with(' ')) {
            continue;
        }
        let content = &after[gt + 1..];
        let close = format!("</{tag_prefix}{name}>");
        let end = content.find(&close)?;
        return Some(xml_unescape(&content[..end]));
    }
    None
}

fn didl_title(meta: &str) -> Option<String> {
    let start = meta.find("<dc:title>")? + "<dc:title>".len();
    let end = meta[start..].find("</dc:title>")?;
    Some(xml_unescape(&meta[start..start + end]))
}

fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_soap_args() {
        let body = r#"<s:Envelope><s:Body><u:SetAVTransportURI xmlns:u="urn:x"><InstanceID>0</InstanceID>
<CurrentURI>http://ha:8123/api/tts?a=1&amp;b=2</CurrentURI>
<CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;item&gt;&lt;dc:title&gt;Rock &amp;amp; Roll&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>
</u:SetAVTransportURI></s:Body></s:Envelope>"#;
        assert_eq!(arg(body, "InstanceID").as_deref(), Some("0"));
        assert_eq!(arg(body, "CurrentURI").as_deref(), Some("http://ha:8123/api/tts?a=1&b=2"));
        let meta = arg(body, "CurrentURIMetaData").unwrap();
        assert_eq!(didl_title(&meta).as_deref(), Some("Rock & Roll"));
        assert_eq!(arg("<a><DesiredMute/></a>", "DesiredMute").as_deref(), Some(""));
    }

    #[test]
    fn formats_time() {
        assert_eq!(hms(3_725_000), "1:02:05");
    }
}
