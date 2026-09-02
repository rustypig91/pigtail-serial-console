//! Port discovery, [`PortIdentity`] construction, and identity matching.
//!
//! Matching is by *device identity*, never by path when USB identity is
//! available: a reset can turn `/dev/ttyACM0` into `/dev/ttyACM1`, and Windows
//! reassigns COM numbers freely (spec §7.1).

use crate::config::PortIdentity;
use crate::wake::Wake;
use crossbeam_channel::Sender;
use serialport::{SerialPortInfo, SerialPortType};
use std::time::Duration;

/// A port found by enumeration: its OS path plus derived identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPort {
    pub path: String,
    pub identity: PortIdentity,
}

/// Build a [`PortIdentity`] from a `SerialPortInfo`.
pub fn identity_from_info(info: &SerialPortInfo) -> PortIdentity {
    match &info.port_type {
        SerialPortType::UsbPort(usb) => PortIdentity {
            vid: Some(usb.vid),
            pid: Some(usb.pid),
            serial_number: usb.serial_number.clone(),
            manufacturer: usb.manufacturer.clone(),
            product: usb.product.clone(),
            path_fallback: info.port_name.clone(),
            interface_hint: usb_interface(usb),
        },
        // (interface_hint stays available for callers that build identities
        // from other sources, e.g. hand-edited config.)
        _ => PortIdentity {
            path_fallback: info.port_name.clone(),
            ..Default::default()
        },
    }
}

// serialport 4.9's `UsbPortInfo` does not expose the USB interface number, so
// enumeration cannot fill `interface_hint`. It remains part of `PortIdentity`
// for hand-edited config that disambiguates multi-interface probes by hand,
// and the matcher honours it when both sides set it.
fn usb_interface(_usb: &serialport::UsbPortInfo) -> Option<u8> {
    None
}

/// Enumerate all currently present ports.
pub fn enumerate_ports() -> Vec<DiscoveredPort> {
    #[allow(unused_mut)]
    let mut discovered = match serialport::available_ports() {
        Ok(ports) => ports
            .iter()
            .map(|info| DiscoveredPort {
                path: info.port_name.clone(),
                identity: identity_from_info(info),
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    #[cfg(debug_assertions)]
    for (path, product) in [
        (crate::source::DEBUG_ECHO_PATH, "pigtail debug echo port -1"),
        (
            crate::source::DEBUG_ECHO_PATH_2,
            "pigtail debug echo port -2",
        ),
    ] {
        discovered.push(DiscoveredPort {
            path: path.into(),
            identity: PortIdentity {
                path_fallback: path.into(),
                product: Some(product.into()),
                ..Default::default()
            },
        });
    }
    discovered
}

/// The outcome of matching a saved identity against discovered ports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchResult {
    /// Exactly one match; the value is an index into the discovered slice.
    Definite(usize),
    /// Several equally-valid matches; the user must choose (spec §7.1 rule 2).
    Ambiguous(Vec<usize>),
    /// No match present.
    None,
}

/// Match a saved identity to discovered ports, in the priority order of §7.1.
pub fn match_identity(saved: &PortIdentity, discovered: &[DiscoveredPort]) -> MatchResult {
    // Rule 1: VID + PID + serial number all match → definite.
    if saved.has_usb() && saved.serial_number.is_some() {
        let hits = collect(discovered, |d| {
            d.identity.vid == saved.vid
                && d.identity.pid == saved.pid
                && d.identity.serial_number == saved.serial_number
                && interface_ok(saved, &d.identity)
        });
        match hits.len() {
            1 => return MatchResult::Definite(hits[0]),
            n if n > 1 => return MatchResult::Ambiguous(hits),
            _ => {}
        }
    }

    // Rule 2: VID + PID match and serial number is None on both → match, but
    // only if exactly one such device is present.
    if saved.has_usb() && saved.serial_number.is_none() {
        let hits = collect(discovered, |d| {
            d.identity.vid == saved.vid
                && d.identity.pid == saved.pid
                && d.identity.serial_number.is_none()
                && interface_ok(saved, &d.identity)
        });
        match hits.len() {
            1 => return MatchResult::Definite(hits[0]),
            n if n > 1 => return MatchResult::Ambiguous(hits),
            _ => {}
        }
    }

    // Rule 3: path fallback → match, for non-USB ports only.
    if !saved.has_usb() && !saved.path_fallback.is_empty() {
        let hits = collect(discovered, |d| {
            !d.identity.has_usb() && d.path == saved.path_fallback
        });
        if let Some(&i) = hits.first() {
            return MatchResult::Definite(i);
        }
    }

    MatchResult::None
}

/// True when `a` and `b` name the same physical device by the priority rules
/// above, not by struct equality — two observations of one device (a saved
/// profile vs. a live connection, or a connection vs. a departure event) can
/// disagree on fields like `path_fallback` that don't change what the device
/// *is*.
pub fn identities_match(a: &PortIdentity, b: &PortIdentity) -> bool {
    if a.has_usb() && a.serial_number.is_some() {
        return b.vid == a.vid
            && b.pid == a.pid
            && b.serial_number == a.serial_number
            && interface_ok(a, b);
    }
    if a.has_usb() && a.serial_number.is_none() {
        return b.vid == a.vid && b.pid == a.pid && b.serial_number.is_none() && interface_ok(a, b);
    }
    if !a.has_usb() && !a.path_fallback.is_empty() {
        return !b.has_usb() && b.path_fallback == a.path_fallback;
    }
    false
}

fn collect(discovered: &[DiscoveredPort], pred: impl Fn(&DiscoveredPort) -> bool) -> Vec<usize> {
    discovered
        .iter()
        .enumerate()
        .filter(|(_, d)| pred(d))
        .map(|(i, _)| i)
        .collect()
}

// If both sides specify an interface hint, they must agree. Otherwise ignore it.
fn interface_ok(saved: &PortIdentity, discovered: &PortIdentity) -> bool {
    match (saved.interface_hint, discovered.interface_hint) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// A change detected between two enumeration snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnumEvent {
    Arrived(DiscoveredPort),
    Departed(DiscoveredPort),
    /// The full current snapshot, sent whenever the port set changes, for
    /// consumers that prefer to diff themselves.
    Snapshot(Vec<DiscoveredPort>),
}

/// Diff two snapshots into arrival/departure events (by path).
pub fn diff_snapshots(prev: &[DiscoveredPort], next: &[DiscoveredPort]) -> Vec<EnumEvent> {
    let mut events = Vec::new();
    for p in next {
        if !prev.iter().any(|q| q.path == p.path) {
            events.push(EnumEvent::Arrived(p.clone()));
        }
    }
    for p in prev {
        if !next.iter().any(|q| q.path == p.path) {
            events.push(EnumEvent::Departed(p.clone()));
        }
    }
    events
}

/// Spawn a thread polling enumeration every `interval` (spec §7.1: 500ms),
/// sending a `Snapshot` plus arrival/departure events whenever the set of
/// present ports changes, and signalling `wake` so a sleeping UI comes back to
/// read them. Runs until the receiver is dropped.
///
/// Nothing is sent while the port set is unchanged. An unconditional snapshot
/// per tick would wake the UI twice a second forever, which is precisely what
/// the wake signal exists to avoid — and it carries no news. Reconnect does not
/// depend on this channel: the reader re-enumerates for itself in
/// `reader::resolve_path`.
pub fn spawn_enumerator(
    tx: Sender<EnumEvent>,
    interval: Duration,
    wake: Wake,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("enumerator".into())
        .spawn(move || {
            let mut prev: Vec<DiscoveredPort> = Vec::new();
            loop {
                let next = enumerate_ports();
                if next != prev {
                    let events = diff_snapshots(&prev, &next);
                    if tx.send(EnumEvent::Snapshot(next.clone())).is_err() {
                        break;
                    }
                    for e in events {
                        if tx.send(e).is_err() {
                            return;
                        }
                    }
                    prev = next;
                    wake.signal();
                }
                std::thread::sleep(interval);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb(vid: u16, pid: u16, serial: Option<&str>, path: &str) -> DiscoveredPort {
        DiscoveredPort {
            path: path.into(),
            identity: PortIdentity {
                vid: Some(vid),
                pid: Some(pid),
                serial_number: serial.map(|s| s.into()),
                path_fallback: path.into(),
                ..Default::default()
            },
        }
    }

    fn saved_usb(vid: u16, pid: u16, serial: Option<&str>) -> PortIdentity {
        PortIdentity {
            vid: Some(vid),
            pid: Some(pid),
            serial_number: serial.map(|s| s.into()),
            ..Default::default()
        }
    }

    #[test]
    fn rule1_serial_definite() {
        let ports = vec![
            usb(0x0483, 0x374B, Some("AAA"), "COM3"),
            usb(0x0483, 0x374B, Some("BBB"), "COM4"),
        ];
        let saved = saved_usb(0x0483, 0x374B, Some("BBB"));
        assert_eq!(match_identity(&saved, &ports), MatchResult::Definite(1));
    }

    #[test]
    fn rule2_no_serial_single_matches() {
        let ports = vec![usb(0x10C4, 0xEA60, None, "/dev/ttyUSB0")];
        let saved = saved_usb(0x10C4, 0xEA60, None);
        assert_eq!(match_identity(&saved, &ports), MatchResult::Definite(0));
    }

    #[test]
    fn rule2_two_identical_no_serial_is_ambiguous() {
        let ports = vec![
            usb(0x10C4, 0xEA60, None, "/dev/ttyUSB0"),
            usb(0x10C4, 0xEA60, None, "/dev/ttyUSB1"),
        ];
        let saved = saved_usb(0x10C4, 0xEA60, None);
        match match_identity(&saved, &ports) {
            MatchResult::Ambiguous(v) => assert_eq!(v, vec![0, 1]),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn reappears_on_different_path_still_matches_by_serial() {
        // Same device, path changed from ACM0 to ACM1 after a reset.
        let before = vec![usb(0x0483, 0x374B, Some("SER1"), "/dev/ttyACM0")];
        let after = vec![usb(0x0483, 0x374B, Some("SER1"), "/dev/ttyACM1")];
        let saved = saved_usb(0x0483, 0x374B, Some("SER1"));
        assert_eq!(match_identity(&saved, &before), MatchResult::Definite(0));
        assert_eq!(match_identity(&saved, &after), MatchResult::Definite(0));
    }

    #[test]
    fn never_matches_usb_saved_against_path() {
        // Saved has USB identity; a non-USB port with the same path must NOT match.
        let ports = vec![DiscoveredPort {
            path: "/dev/ttyACM0".into(),
            identity: PortIdentity {
                path_fallback: "/dev/ttyACM0".into(),
                ..Default::default()
            },
        }];
        let saved = saved_usb(0x0483, 0x374B, Some("SER1"));
        assert_eq!(match_identity(&saved, &ports), MatchResult::None);
    }

    #[test]
    fn non_usb_path_fallback_matches() {
        let ports = vec![DiscoveredPort {
            path: "/dev/ttyS0".into(),
            identity: PortIdentity {
                path_fallback: "/dev/ttyS0".into(),
                ..Default::default()
            },
        }];
        let saved = PortIdentity {
            path_fallback: "/dev/ttyS0".into(),
            ..Default::default()
        };
        assert_eq!(match_identity(&saved, &ports), MatchResult::Definite(0));
    }

    #[test]
    fn interface_hint_disambiguates() {
        let mut a = usb(0x0483, 0x374B, Some("SER"), "COM3");
        a.identity.interface_hint = Some(0);
        let mut b = usb(0x0483, 0x374B, Some("SER"), "COM4");
        b.identity.interface_hint = Some(2);
        let ports = vec![a, b];
        let mut saved = saved_usb(0x0483, 0x374B, Some("SER"));
        saved.interface_hint = Some(2);
        assert_eq!(match_identity(&saved, &ports), MatchResult::Definite(1));
    }

    #[test]
    fn diff_detects_arrival_and_departure() {
        let prev = vec![usb(1, 2, Some("A"), "COM1")];
        let next = vec![usb(3, 4, Some("B"), "COM2")];
        let events = diff_snapshots(&prev, &next);
        assert!(events.contains(&EnumEvent::Arrived(next[0].clone())));
        assert!(events.contains(&EnumEvent::Departed(prev[0].clone())));
    }
}
