//! Small helpers for network discovery / share URL construction. Kept in a
//! standalone module so both `main` (startup banner) and `server` (JSON API)
//! can use the same logic.

use std::collections::HashSet;
use std::net::IpAddr;

use serde::Serialize;

use crate::config::Config;

/// One LAN URL the user might share, paired with the interface name so they
/// can pick the right one when multiple NICs exist (physical vs virtual
/// switch on the same host).
#[derive(Debug, Clone, Serialize)]
pub struct ShareEntry {
    pub url: String,
    pub interface: String,
    pub virtual_iface: bool,
}

/// Return every non-loopback IPv4 address on this machine, tagged with the
/// interface name (empty string if unavailable). IPv6 is omitted in v0.1.
pub fn list_lan_ipv4() -> Vec<(String, std::net::Ipv4Addr)> {
    match local_ip_address::list_afinet_netifas() {
        Ok(v) => v
            .into_iter()
            .filter_map(|(name, ip)| match ip {
                std::net::IpAddr::V4(a)
                    if !a.is_loopback() && !a.is_unspecified() && !a.is_link_local() =>
                {
                    Some((name, a))
                }
                _ => None,
            })
            .collect(),
        // Returning empty stays correct — but it must not be silent. The Host
        // allowlist is built from this list, so an enumeration failure here is
        // how "every request from the LAN suddenly 403s" happens invisibly.
        Err(e) => {
            eprintln!("[warn] cannot enumerate network interfaces ({e}); no LAN addresses found");
            Vec::new()
        }
    }
}

/// Heuristic: does this interface name look like a virtual switch (Hyper-V,
/// WSL, Docker, VirtualBox, Bluetooth PAN, etc.) rather than a physical NIC?
/// Used only for sort order — never filters anything out.
pub fn is_virtual_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("vethernet")
        || n.contains("virtual")
        || n.contains("wsl")
        || n.contains("docker")
        || n.contains("hyper-v")
        || n.contains("vmware")
        || n.contains("vbox")
        || n.contains("virtualbox")
        || n.contains("bluetooth")
        || n.contains("loopback")
        || n.contains("tap-")
        || n.contains("tun")
}

/// Share entries with the token pre-embedded. Physical NICs first, virtual
/// switches last. Empty when LAN is not exposed or there is no usable token.
///
/// `token` is passed in rather than read from `cfg` because the live token set
/// can change while the server runs (see `crate::auth`), and a share URL built
/// from a revoked token is worse than no share URL.
pub fn build_share_entries(cfg: &Config, token: Option<&str>) -> Vec<ShareEntry> {
    if !cfg.lan_exposed() {
        return Vec::new();
    }
    let Some(token) = token else {
        return Vec::new();
    };
    let port = cfg.server.port;
    let mut entries: Vec<ShareEntry> = list_lan_ipv4()
        .into_iter()
        .map(|(iface, ip)| ShareEntry {
            url: format!("http://{ip}:{port}/#t={token}"),
            virtual_iface: is_virtual_iface(&iface),
            interface: iface,
        })
        .collect();
    // Stable sort by `virtual_iface` so physical NICs come first without
    // disturbing the OS enumeration order among peers.
    entries.sort_by_key(|e| e.virtual_iface);
    entries
}

/// The URL a user on the host itself should open (auth bypassed for loopback).
///
/// Loopback when the server actually answers there — `0.0.0.0`, `127.0.0.1`,
/// `localhost`. A bind pinned to one LAN address does **not** answer on
/// 127.0.0.1, and `::`/`::1` only answer over IPv6 on Windows, so those get a
/// URL that can really connect instead of a convenient lie.
pub fn build_host_url(cfg: &Config) -> String {
    let port = cfg.server.port;
    match crate::config::parse_bind(&cfg.server.bind) {
        Ok(IpAddr::V4(v4)) if v4.is_loopback() || v4.is_unspecified() => {
            format!("http://127.0.0.1:{port}/")
        }
        Ok(IpAddr::V4(v4)) => format!("http://{v4}:{port}/"),
        Ok(IpAddr::V6(v6)) if v6.is_loopback() || v6.is_unspecified() => {
            format!("http://[::1]:{port}/")
        }
        Ok(IpAddr::V6(v6)) => format!("http://[{v6}]:{port}/"),
        // Unparseable bind never reaches here in practice (startup validates it
        // first); loopback is the least surprising thing to print.
        Err(_) => format!("http://127.0.0.1:{port}/"),
    }
}

/// Host header values accepted by the AR-02 DNS-rebinding guard, built once at
/// startup: loopback names plus every current LAN IPv4, each as bare host and
/// `host:port`. A request whose `Host` is outside this set is refused, so an
/// attacker page whose DNS re-resolves to `127.0.0.1` (rebinding) cannot pass
/// its own hostname through even though the TCP peer looks like loopback.
/// Snapshot at startup — a NIC added later needs a restart to be accepted.
pub fn build_allowed_hosts(cfg: &Config) -> HashSet<String> {
    let port = cfg.server.port;
    let mut names = vec![
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        "::1".to_string(),
        "[::1]".to_string(),
    ];
    for (_iface, ip) in list_lan_ipv4() {
        names.push(ip.to_string());
    }
    // A bind pinned to a single address may not appear in the enumeration above
    // (a static IPv6 address, or a NIC the enumerator skips) — and it is exactly
    // what the operator will type into the browser.
    if let Ok(ip) = crate::config::parse_bind(&cfg.server.bind) {
        if !ip.is_unspecified() {
            names.push(match ip {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            });
        }
    }
    let mut set = HashSet::new();
    for name in names {
        let h = name.to_ascii_lowercase();
        set.insert(format!("{h}:{port}"));
        set.insert(h);
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(bind: &str, port: u16) -> Config {
        let mut c = Config::default();
        c.server.bind = bind.to_string();
        c.server.port = port;
        c
    }

    #[test]
    fn host_url_points_where_the_server_actually_listens() {
        for (bind, expected) in [
            ("0.0.0.0", "http://127.0.0.1:17650/"),
            ("127.0.0.1", "http://127.0.0.1:17650/"),
            ("localhost", "http://127.0.0.1:17650/"),
            ("::", "http://[::1]:17650/"),
            ("::1", "http://[::1]:17650/"),
            ("[::1]", "http://[::1]:17650/"),
            // Pinned to one LAN address: 127.0.0.1 would not answer at all.
            ("203.0.113.5", "http://203.0.113.5:17650/"),
        ] {
            assert_eq!(build_host_url(&cfg(bind, 17650)), expected, "bind = {bind}");
        }
    }

    #[test]
    fn share_entries_need_both_lan_exposure_and_a_token() {
        assert!(
            build_share_entries(&cfg("127.0.0.1", 17650), Some("ar_live_x")).is_empty(),
            "host-only bind must not advertise share URLs"
        );
        assert!(
            build_share_entries(&cfg("0.0.0.0", 17650), None).is_empty(),
            "no usable token means no share URL"
        );

        // The machine's own NIC list is environment-dependent, so assert the shape
        // of whatever comes back rather than the count.
        for entry in build_share_entries(&cfg("0.0.0.0", 17650), Some("ar_live_x")) {
            assert!(entry.url.starts_with("http://"), "{}", entry.url);
            assert!(entry.url.contains(":17650/#t=ar_live_x"), "{}", entry.url);
        }
    }

    #[test]
    fn allowed_hosts_cover_loopback_names_with_and_without_the_port() {
        let hosts = build_allowed_hosts(&cfg("0.0.0.0", 17650));
        for expected in [
            "127.0.0.1",
            "127.0.0.1:17650",
            "localhost",
            "localhost:17650",
            "::1",
            "[::1]",
            "[::1]:17650",
        ] {
            assert!(hosts.contains(expected), "missing {expected}: {hosts:?}");
        }
        assert!(!hosts.contains("evil.example"));
        // `0.0.0.0` is a wildcard, not something a browser ever sends as Host.
        assert!(!hosts.contains("0.0.0.0"));
    }

    #[test]
    fn allowed_hosts_include_a_pinned_bind_address() {
        let v4 = build_allowed_hosts(&cfg("203.0.113.5", 17650));
        assert!(v4.contains("203.0.113.5"));
        assert!(v4.contains("203.0.113.5:17650"));

        let v6 = build_allowed_hosts(&cfg("2001:db8::1", 17650));
        assert!(v6.contains("[2001:db8::1]"));
        assert!(v6.contains("[2001:db8::1]:17650"));
    }

    #[test]
    fn virtual_interfaces_are_recognised_by_name() {
        for name in [
            "vEthernet (Default Switch)",
            "WSL (Hyper-V firewall)",
            "Docker Desktop",
            "VirtualBox Host-Only Network",
            "Bluetooth Network Connection",
            "VMware Network Adapter VMnet1",
        ] {
            assert!(is_virtual_iface(name), "{name}");
        }
        for name in ["Ethernet", "Wi-Fi", "イーサネット 2"] {
            assert!(!is_virtual_iface(name), "{name}");
        }
    }
}
