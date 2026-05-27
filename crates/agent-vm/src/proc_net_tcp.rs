//! Parse Linux `/proc/net/tcp{,6}` listen entries.
//!
//! Lines look like:
//!
//! ```text
//!   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ...
//!    0: 0100007F:2382 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1089 1 ...
//! ```
//!
//! - `local_address` is `HEX_IP:HEX_PORT` (each IPv4 byte
//!   reversed within the 4-byte word — little-endian as written by
//!   the kernel formatter).
//! - `st = 0A` is TCP_LISTEN.
//!
//! For `/proc/net/tcp6` the IP is 32 hex chars (16 bytes) in the
//! same per-word endianness convention.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One listening socket: bind address + port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListenEntry {
    pub addr: IpAddr,
    pub port: u16,
}

/// Parse a single `/proc/net/tcp` body (skipping the header row).
/// Returns only LISTEN state entries.
pub fn parse_listen_v4(body: &str) -> BTreeSet<ListenEntry> {
    let mut out = BTreeSet::new();
    for line in body.lines().skip(1) {
        if let Some(entry) = parse_v4_line(line) {
            out.insert(entry);
        }
    }
    out
}

pub fn parse_listen_v6(body: &str) -> BTreeSet<ListenEntry> {
    let mut out = BTreeSet::new();
    for line in body.lines().skip(1) {
        if let Some(entry) = parse_v6_line(line) {
            out.insert(entry);
        }
    }
    out
}

fn parse_v4_line(line: &str) -> Option<ListenEntry> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // sl, local, remote, st, ...
    if fields.len() < 4 || fields[3] != "0A" {
        return None;
    }
    let (ip_hex, port_hex) = fields[1].split_once(':')?;
    if ip_hex.len() != 8 {
        return None;
    }
    let raw = u32::from_str_radix(ip_hex, 16).ok()?;
    // kernel writes in native (little-endian on x86_64) byte order:
    // 0100007F → bytes [01, 00, 00, 7F] → 127.0.0.1 read big-endian.
    let bytes = raw.to_be_bytes();
    let addr = IpAddr::V4(Ipv4Addr::new(bytes[3], bytes[2], bytes[1], bytes[0]));
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    Some(ListenEntry { addr, port })
}

fn parse_v6_line(line: &str) -> Option<ListenEntry> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 || fields[3] != "0A" {
        return None;
    }
    let (ip_hex, port_hex) = fields[1].split_once(':')?;
    if ip_hex.len() != 32 {
        return None;
    }
    // The address is four little-endian u32 words concatenated.
    // Convert each word to big-endian bytes so an all-zero v6
    // address shows up as 0::0 and ::1 maps correctly.
    let mut bytes = [0u8; 16];
    for i in 0..4 {
        let word = u32::from_str_radix(&ip_hex[i * 8..(i + 1) * 8], 16).ok()?;
        let be = word.to_be_bytes();
        bytes[i * 4] = be[3];
        bytes[i * 4 + 1] = be[2];
        bytes[i * 4 + 2] = be[1];
        bytes[i * 4 + 3] = be[0];
    }
    let addr = IpAddr::V6(Ipv6Addr::from(bytes));
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    Some(ListenEntry { addr, port })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const SAMPLE_TCP4: &str = "  sl  local_address rem_address   st\n\
         0: 0100007F:2382 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1089 1\n\
         1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 42 1\n\
         2: 0100007F:0050 0100007F:8000 01 00000000:00000000 00:00000000 00000000     0        0 99 1\n";

    #[test]
    fn parses_v4_loopback_and_wildcard() {
        let s = parse_listen_v4(SAMPLE_TCP4);
        assert!(s.contains(&ListenEntry {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0x2382,
        }));
        assert!(s.contains(&ListenEntry {
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0x1F90,
        }));
    }

    #[test]
    fn skips_non_listen_states() {
        let s = parse_listen_v4(SAMPLE_TCP4);
        // st=01 (ESTABLISHED) for the third row must not appear.
        assert!(!s.contains(&ListenEntry {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0x0050,
        }));
    }

    #[test]
    fn parses_v6_unspecified() {
        let sample = "  sl  local_address                         remote_address                        st\n\
             0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99 1\n";
        let s = parse_listen_v6(sample);
        assert!(s.contains(&ListenEntry {
            addr: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            port: 0x1F90,
        }));
    }

    #[test]
    fn parses_v6_loopback() {
        // ::1 = 00000000000000000000000001000000 in kernel little-endian-per-u32 form
        //       (last 4 bytes are 01 00 00 00 in memory → ::1)
        let sample = "  sl  local_address                         remote_address                        st\n\
             0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99 1\n";
        let s = parse_listen_v6(sample);
        assert!(
            s.contains(&ListenEntry {
                addr: IpAddr::V6(Ipv6Addr::LOCALHOST),
                port: 0x1F90,
            }),
            "got {s:?}"
        );
    }

    #[test]
    fn empty_or_garbage_returns_empty_set() {
        assert!(parse_listen_v4("").is_empty());
        assert!(parse_listen_v4("garbage garbage garbage\n").is_empty());
    }
}
