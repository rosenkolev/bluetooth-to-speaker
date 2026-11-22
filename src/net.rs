use std::net::{IpAddr, SocketAddr, UdpSocket};

/// Local IP used to reach `target` (no packet is sent).
pub fn local_ip_for(target: SocketAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(target).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

/// Main LAN IP of this host.
pub fn local_ip() -> Option<IpAddr> {
    if let Ok(ip) = std::env::var("ADVERTISE_IP") {
        if let Ok(ip) = ip.parse() {
            return Some(ip);
        }
    }
    local_ip_for("239.255.255.250:1900".parse().unwrap())
        .filter(|ip| !ip.is_unspecified())
        .or_else(|| local_ip_for("192.0.2.1:80".parse().unwrap()))
}
