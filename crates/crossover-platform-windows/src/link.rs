//! Windows [`LinkStateProbe`]: which local interface carries a peer, and is
//! it up (docs/ARCHITECTURE.md §4).
//!
//! Two IP Helper calls, both pure reads of kernel tables — no network
//! traffic, no waiting, no handles to own:
//!
//! 1. [`GetBestInterfaceEx`] answers *which* interface the routing table
//!    would send this peer's traffic over. Asking per peer is the point: a
//!    laptop with live Wi-Fi and a dead dock has one interface up and one
//!    down, and only the one this session routes over is evidence about this
//!    session.
//! 2. [`GetIfEntry2`] reads that interface's row, which carries both the
//!    administrative view (`OperStatus`) and the physical one
//!    (`MediaConnectState`).
//!
//! `MediaConnectState` is checked first and deliberately, because it is the
//! field that catches the incident this exists for: an Intel I225 2.5 `GbE`
//! NIC in a dock repeatedly dropping and renegotiating its physical link.
//! During those outages the adapter is still present, still administratively
//! enabled, and its media is disconnected — which is exactly
//! `MediaConnectStateDisconnected` while `OperStatus` may still lag.
//!
//! Everything that can go wrong answers [`LinkState::Unknown`]: no route to
//! the peer, an interface index the table no longer has, a status value
//! outside the enum. The caller is holding a dead session and about to
//! reconnect; it must never get an error to handle, and never be delayed.

use std::net::SocketAddr;

use crossover_platform::{LinkState, LinkStateProbe};
use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{GetBestInterfaceEx, GetIfEntry2, MIB_IF_ROW2};
use windows::Win32::NetworkManagement::Ndis::{
    IF_OPER_STATUS, IfOperStatusDown, IfOperStatusLowerLayerDown, IfOperStatusNotPresent,
    IfOperStatusUp, MediaConnectStateConnected, MediaConnectStateDisconnected,
    NET_IF_MEDIA_CONNECT_STATE,
};
use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0,
};

/// Reads local interface state from the Windows IP Helper tables.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsLinkStateProbe;

impl LinkStateProbe for WindowsLinkStateProbe {
    fn link_state(&self, peer: SocketAddr) -> LinkState {
        let Some(interface_index) = best_interface_for(peer) else {
            return LinkState::Unknown;
        };
        let Some(row) = interface_row(interface_index) else {
            return LinkState::Unknown;
        };
        classify(row.MediaConnectState, row.OperStatus)
    }
}

/// Index of the interface the routing table would use to reach `peer`.
///
/// `None` when there is no route — which is itself a symptom of the outage
/// (a down interface takes its routes with it), but not proof of *which*
/// interface died, so it stays `Unknown` rather than being read as `Down`.
fn best_interface_for(peer: SocketAddr) -> Option<u32> {
    let mut index = 0u32;
    let error = match peer {
        SocketAddr::V4(v4) => {
            let sockaddr = SOCKADDR_IN {
                sin_family: ADDRESS_FAMILY(AF_INET.0),
                // Port is irrelevant to a route lookup, and left zero so no
                // byte-order subtlety can affect the answer.
                sin_port: 0,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(v4.ip().octets()),
                    },
                },
                sin_zero: [0; 8],
            };
            // SAFETY: `sockaddr` is a fully initialized SOCKADDR_IN, whose
            // layout is the AF_INET case of the SOCKADDR union the API
            // documents; it outlives the call, which only reads it, and
            // `index` is a valid out-pointer for one u32.
            unsafe {
                GetBestInterfaceEx(
                    std::ptr::from_ref(&sockaddr).cast::<SOCKADDR>(),
                    &raw mut index,
                )
            }
        }
        SocketAddr::V6(v6) => {
            let sockaddr = SOCKADDR_IN6 {
                sin6_family: ADDRESS_FAMILY(AF_INET6.0),
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: v6.ip().octets(),
                    },
                },
                // The scope id matters: a link-local peer is only routable
                // through the interface it was scoped to.
                Anonymous: SOCKADDR_IN6_0 {
                    sin6_scope_id: v6.scope_id(),
                },
            };
            // SAFETY: as above, for the AF_INET6 case.
            unsafe {
                GetBestInterfaceEx(
                    std::ptr::from_ref(&sockaddr).cast::<SOCKADDR>(),
                    &raw mut index,
                )
            }
        }
    };
    (error == NO_ERROR.0).then_some(index)
}

/// The interface table row for `index`, or `None` if it cannot be read.
fn interface_row(index: u32) -> Option<MIB_IF_ROW2> {
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    // SAFETY: `row` is a fully initialized MIB_IF_ROW2 with the input field
    // (`InterfaceIndex`) set and the rest zeroed, which is the contract
    // GetIfEntry2 documents; the pointer is valid and uniquely borrowed for
    // the duration of the call.
    let error = unsafe { GetIfEntry2(&raw mut row) };
    (error == NO_ERROR).then_some(row)
}

/// Turn one interface row's two status fields into a verdict.
///
/// Split out from the FFI so the mapping — the part with a policy in it —
/// is unit-tested with no adapter to unplug.
fn classify(media: NET_IF_MEDIA_CONNECT_STATE, oper: IF_OPER_STATUS) -> LinkState {
    // The physical view first: it is what moves during a link flap, and it
    // moves before OperStatus does.
    if media == MediaConnectStateDisconnected {
        return LinkState::Down;
    }
    // "Down" here means the adapter is disabled, absent, or sitting on a
    // dead lower layer — a local fault in every case.
    if oper == IfOperStatusDown
        || oper == IfOperStatusLowerLayerDown
        || oper == IfOperStatusNotPresent
    {
        return LinkState::Down;
    }
    // Up *and* media connected is the only combination that earns `Up`. A
    // media state of `Unknown` (some virtual adapters never report one)
    // leaves the question open rather than answering it.
    if oper == IfOperStatusUp && media == MediaConnectStateConnected {
        return LinkState::Up;
    }
    // Dormant, testing, unknown, and anything either enum gains later: not
    // an accusation, and not a clean bill of health either.
    LinkState::Unknown
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use crossover_platform::{LinkState, LinkStateProbe};
    use windows::Win32::NetworkManagement::Ndis::{
        IF_OPER_STATUS, IfOperStatusDormant, IfOperStatusDown, IfOperStatusLowerLayerDown,
        IfOperStatusNotPresent, IfOperStatusTesting, IfOperStatusUnknown, IfOperStatusUp,
        MediaConnectStateConnected, MediaConnectStateDisconnected, MediaConnectStateUnknown,
        NET_IF_MEDIA_CONNECT_STATE,
    };

    use super::{WindowsLinkStateProbe, classify};

    /// The mapping, exhaustively, with no adapter to unplug.
    #[test]
    fn only_a_connected_media_on_a_live_interface_counts_as_up() {
        assert_eq!(
            classify(MediaConnectStateConnected, IfOperStatusUp),
            LinkState::Up
        );

        // The incident: media disconnected while the adapter is still
        // present and enabled. Down whatever OperStatus says, because
        // OperStatus lags the physical link.
        for oper in [
            IfOperStatusUp,
            IfOperStatusDown,
            IfOperStatusUnknown,
            IfOperStatusDormant,
        ] {
            assert_eq!(
                classify(MediaConnectStateDisconnected, oper),
                LinkState::Down,
                "media disconnected but reported {oper:?}"
            );
        }

        // Administratively down, absent, or on a dead lower layer.
        for oper in [
            IfOperStatusDown,
            IfOperStatusLowerLayerDown,
            IfOperStatusNotPresent,
        ] {
            assert_eq!(classify(MediaConnectStateConnected, oper), LinkState::Down);
        }

        // Everything else declines to answer rather than guessing — an
        // unreported media state included.
        for oper in [
            IfOperStatusDormant,
            IfOperStatusTesting,
            IfOperStatusUnknown,
        ] {
            assert_eq!(
                classify(MediaConnectStateConnected, oper),
                LinkState::Unknown
            );
        }
        assert_eq!(
            classify(MediaConnectStateUnknown, IfOperStatusUp),
            LinkState::Unknown
        );
        // A value from outside both enums (a future Windows) is not a
        // verdict either.
        assert_eq!(
            classify(NET_IF_MEDIA_CONNECT_STATE(99), IF_OPER_STATUS(99)),
            LinkState::Unknown
        );
    }

    /// Smoke test of the real Win32 path: it answers, promptly, for every
    /// shape of address, and never panics or hangs.
    ///
    /// Deliberately weak about *which* answer, apart from loopback: the
    /// state of a CI runner's adapters is not this crate's business. What it
    /// pins is that the call sequence works on a real machine — the half a
    /// pure mapping test cannot reach.
    #[test]
    fn the_real_probe_answers_for_every_shape_of_address() {
        let probe = WindowsLinkStateProbe;
        let addresses: [SocketAddr; 4] = [
            // Routable, and a real public address so a default route exists.
            "1.1.1.1:443".parse().unwrap(),
            "[2606:4700:4700::1111]:443".parse().unwrap(),
            // Documentation ranges: usually no specific route, so this
            // exercises the "fell back to the default route or nothing"
            // path rather than the happy one.
            "192.0.2.1:27677".parse().unwrap(),
            "[2001:db8::1]:27677".parse().unwrap(),
        ];
        let started = std::time::Instant::now();
        for address in addresses {
            let state = probe.link_state(address);
            assert!(
                matches!(state, LinkState::Up | LinkState::Down | LinkState::Unknown),
                "{address}"
            );
        }
        // It runs on a failure path and must never delay a reconnect. Two
        // kernel table reads are microseconds; a second for four of them is
        // a ceiling loose enough never to flake and tight enough to catch a
        // call that started blocking.
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// The loopback interface is up on any machine able to run this test at
    /// all, so this is the one place a concrete answer can be demanded — and
    /// the only end-to-end proof that the two calls agree on an index.
    #[test]
    fn loopback_reports_up() {
        let probe = WindowsLinkStateProbe;
        assert_eq!(
            probe.link_state("127.0.0.1:27677".parse().unwrap()),
            LinkState::Up
        );
    }
}
