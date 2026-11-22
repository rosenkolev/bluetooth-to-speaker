//! SSDP announcements so Home Assistant and Music Assistant discover the DLNA renderer.
//! Needs host networking (multicast 239.255.255.250:1900).

use crate::app::App;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;

fn targets(uid: &str) -> Vec<(String, String)> {
    let udn = format!("uuid:{uid}");
    let mut v = vec![("upnp:rootdevice".to_string(), format!("{udn}::upnp:rootdevice")), (udn.clone(), udn.clone())];
    for t in [
        "urn:schemas-upnp-org:device:MediaRenderer:1",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "urn:schemas-upnp-org:service:RenderingControl:1",
        "urn:schemas-upnp-org:service:ConnectionManager:1",
    ] {
        v.push((t.to_string(), format!("{udn}::{t}")));
    }
    v
}

fn listen_socket() -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    s.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

pub async fn run(app: Arc<App>) {
    let sock = match listen_socket() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("SSDP disabled (needs host networking): {e}");
            return;
        }
    };
    let port = app.cfg().http_port;
    let server = format!("Linux UPnP/1.0 bluetooth-to-speaker/{}", crate::VERSION);
    let tg = targets(&app.uid);
    let group: SocketAddr = SocketAddrV4::new(GROUP, PORT).into();
    log::info!("SSDP announcing DLNA renderer");

    let notify = |kind: &str| -> Vec<String> {
        let ip = crate::net::local_ip().map(|i| i.to_string()).unwrap_or_default();
        tg.iter()
            .map(|(nt, usn)| {
                format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age=1800\r\n\
LOCATION: http://{ip}:{port}/dlna/description.xml\r\nNT: {nt}\r\nNTS: {kind}\r\nSERVER: {server}\r\nUSN: {usn}\r\n\r\n"
                )
            })
            .collect()
    };

    let mut announce = tokio::time::interval(Duration::from_secs(60));
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = announce.tick() => {
                for m in notify("ssdp:alive") {
                    let _ = sock.send_to(m.as_bytes(), group).await;
                }
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, src)) = r else { continue };
                let msg = String::from_utf8_lossy(&buf[..n]);
                if !msg.starts_with("M-SEARCH") {
                    continue;
                }
                let st = header(&msg, "ST").unwrap_or_default();
                let ip = crate::net::local_ip_for(src).map(|i| i.to_string()).unwrap_or_default();
                for (nt, usn) in tg.iter().filter(|(nt, _)| st == "ssdp:all" || st == *nt) {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\n\
        LOCATION: http://{ip}:{port}/dlna/description.xml\r\nSERVER: {server}\r\nST: {nt}\r\nUSN: {usn}\r\n\r\n"
                    );
                    let _ = sock.send_to(resp.as_bytes(), src).await;
                }
            }
        }
    }
}

fn header(msg: &str, name: &str) -> Option<String> {
    msg.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    })
}
