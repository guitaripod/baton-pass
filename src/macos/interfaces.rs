//! The Mac's own IPv4 addresses and the kind of link each sits on. The desktop is told about
//! them, so it reaches the Mac over the best path there is (a cable before Wi-Fi, Wi-Fi before
//! the tailnet) even when its config names an address the Mac no longer has.

use crate::proto::{MAX_ADDRESSES, Medium};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::rc::Rc;
use std::time::{Duration, Instant};

const PORTS_REFRESH: Duration = Duration::from_secs(30);

/// Names each interface's hardware port ("Wi-Fi", "Thunderbolt Ethernet Slot 0") as
/// `networksetup` lists them, read in the background so the daemon's loop never waits on it.
pub struct Interfaces {
    ports: Rc<RefCell<HashMap<String, String>>>,
    ports_read: Option<Instant>,
}

impl Interfaces {
    pub fn new() -> Self {
        let mut interfaces = Self { ports: Rc::new(RefCell::new(HashMap::new())), ports_read: None };
        interfaces.read_ports();
        interfaces
    }

    /// Every usable address with its medium, listening on `port`, in a stable order.
    pub fn addresses(&mut self, port: u16) -> Vec<(SocketAddrV4, Medium)> {
        let found = ipv4_by_interface();
        let unknown = found.iter().any(|(name, _)| !self.ports.borrow().contains_key(name));
        if unknown && self.ports_read.is_none_or(|at| at.elapsed() > PORTS_REFRESH) {
            self.read_ports();
        }
        let ports = self.ports.borrow();
        found
            .into_iter()
            .map(|(name, ip)| (SocketAddrV4::new(ip, port), medium(&name, ip, ports.get(&name).map(String::as_str))))
            .take(MAX_ADDRESSES)
            .collect()
    }

    fn read_ports(&mut self) {
        self.ports_read = Some(Instant::now());
        let ports = self.ports.clone();
        tokio::task::spawn_local(async move {
            match tokio::process::Command::new("/usr/sbin/networksetup").arg("-listallhardwareports").output().await {
                Ok(output) => *ports.borrow_mut() = parse_ports(&String::from_utf8_lossy(&output.stdout)),
                Err(error) => tracing::warn!("listing hardware ports: {error}"),
            }
        });
    }
}

/// Wi-Fi is wireless; a phone, Bluetooth or a tunnel (the tailnet among them) is something
/// else; every other hardware port (Ethernet, Thunderbolt Ethernet or Bridge, USB LAN) is a
/// cable.
fn medium(name: &str, ip: Ipv4Addr, port: Option<&str>) -> Medium {
    let tunnel = ["utun", "ipsec", "ppp", "gif", "stf"].iter().any(|prefix| name.starts_with(prefix));
    if tunnel || is_shared_address_space(ip) {
        return Medium::Other;
    }
    match port {
        Some(port) if port.contains("Wi-Fi") || port.contains("AirPort") => Medium::Wireless,
        Some(port) if ["iPhone", "iPad", "Bluetooth"].iter().any(|word| port.contains(word)) => Medium::Other,
        Some(_) => Medium::Wired,
        None => Medium::Other,
    }
}

/// 100.64.0.0/10, where the tailnet hands out addresses.
fn is_shared_address_space(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    first == 100 && (64..128).contains(&second)
}

fn parse_ports(text: &str) -> HashMap<String, String> {
    let mut ports = HashMap::new();
    let mut port: Option<&str> = None;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("Hardware Port: ") {
            port = Some(name.trim());
        } else if let (Some(device), Some(name)) = (line.strip_prefix("Device: "), port.take()) {
            ports.insert(device.trim().to_string(), name.to_string());
        }
    }
    ports
}

/// The IPv4 addresses of interfaces that are up and running, leaving out loopback and
/// self-assigned link-local addresses, sorted by interface name.
fn ipv4_by_interface() -> Vec<(String, Ipv4Addr)> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let mut entry = list;
    while !entry.is_null() {
        let interface = unsafe { &*entry };
        entry = interface.ifa_next;
        let flags = interface.ifa_flags as i32;
        let usable = flags & libc::IFF_UP != 0 && flags & libc::IFF_RUNNING != 0 && flags & libc::IFF_LOOPBACK == 0;
        if !usable || interface.ifa_addr.is_null() || i32::from(unsafe { (*interface.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        let address = unsafe { &*(interface.ifa_addr as *const libc::sockaddr_in) };
        let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        if ip.is_link_local() {
            continue;
        }
        let name = unsafe { CStr::from_ptr(interface.ifa_name) }.to_string_lossy().into_owned();
        found.push((name, ip));
    }
    unsafe { libc::freeifaddrs(list) };
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_ports_name_the_medium() {
        let ports = parse_ports(
            "Hardware Port: Thunderbolt Ethernet Slot 0\nDevice: en7\nEthernet Address: 00:00:00:00:00:01\n\nHardware Port: Wi-Fi\nDevice: en0\nEthernet Address: 00:00:00:00:00:02\n\nHardware Port: iPhone USB\nDevice: en11\n",
        );
        let lan = Ipv4Addr::new(192, 168, 1, 58);
        assert_eq!(medium("en7", lan, ports.get("en7").map(String::as_str)), Medium::Wired);
        assert_eq!(medium("en0", lan, ports.get("en0").map(String::as_str)), Medium::Wireless);
        assert_eq!(medium("en11", lan, ports.get("en11").map(String::as_str)), Medium::Other);
        assert_eq!(medium("utun4", Ipv4Addr::new(100, 127, 250, 64), None), Medium::Other);
        assert_eq!(medium("en0", Ipv4Addr::new(100, 100, 1, 1), Some("Wi-Fi")), Medium::Other);
        assert_eq!(medium("bridge100", lan, None), Medium::Other);
    }
}
