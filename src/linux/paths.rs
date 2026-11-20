//! The Mac's own addresses, as it last reported them, kept across restarts so the desktop can
//! find the Mac on a cable or Wi-Fi whose address the config does not name.

use crate::proto::{MAX_ADDRESSES, Medium};
use std::net::SocketAddrV4;
use std::path::PathBuf;

fn path() -> PathBuf {
    crate::config::xdg_dir("XDG_STATE_HOME", ".local/state").join("mac-addresses")
}

pub fn load() -> Vec<(SocketAddrV4, Medium)> {
    std::fs::read_to_string(path()).map(|text| parse(&text)).unwrap_or_default()
}

pub fn save(list: &[(SocketAddrV4, Medium)]) {
    let text: String = list.iter().map(|(address, medium)| format!("{address} {}\n", medium.name())).collect();
    let target = path();
    let written = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&target, text));
    if let Err(error) = written {
        tracing::warn!("remembering the Mac's addresses in {}: {error}", target.display());
    }
}

fn parse(text: &str) -> Vec<(SocketAddrV4, Medium)> {
    text.lines()
        .filter_map(|line| {
            let (address, medium) = line.split_once(' ')?;
            Some((address.parse().ok()?, Medium::parse(medium.trim())?))
        })
        .take(MAX_ADDRESSES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn reads_back_what_it_writes_and_skips_junk() {
        let text = "192.0.2.7:47474 wired\nnonsense\n198.51.100.9:47474 wireless\n203.0.113.1:1 teleport\n";
        assert_eq!(
            parse(text),
            vec![
                (SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 47474), Medium::Wired),
                (SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 9), 47474), Medium::Wireless),
            ]
        );
    }
}
