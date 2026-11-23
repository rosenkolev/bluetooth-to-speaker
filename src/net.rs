use std::net::{IpAddr, SocketAddr, UdpSocket};

/// Local IP used to reach `target` (no packet is sent).
pub fn local_ip_for(target: SocketAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(target).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

/// Name of the network interface that has this IPv4 address ("eth0", "end1").
pub fn interface_of(ip: IpAddr) -> Option<String> {
    let IpAddr::V4(want) = ip else { return None };
    let mut found = None;
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                if u32::from_be(sin.sin_addr.s_addr) == u32::from(want) {
                    found = Some(std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned());
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
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
