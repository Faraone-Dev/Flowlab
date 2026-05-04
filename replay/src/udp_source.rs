// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! UDP multicast ingress for MoldUDP64 feeds.
//!
//! Opens a socket, optionally joins a multicast group, drives a
//! [`crate::moldudp::GapTracker`] for in-order delivery, feeds delivered
//! messages to a user closure (forward to ring/WAL/strategy).
//!
//! Tuning: SO_RCVBUF default 8 MiB (NASDAQ TotalView open burst absorber);
//! socket non-blocking so the loop can service retransmit requests.
//! Threading: one handler = one thread. Shard by group for parallelism.

use crate::moldudp::{Delivered, GapTracker, MoldFrame};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

/// Configuration for a MoldUDP64 listener.
#[derive(Debug, Clone)]
pub struct MulticastConfig {
    /// Local bind address. Use `0.0.0.0:<port>` to receive multicast,
    /// or the multicast group itself on some OSes.
    pub bind: SocketAddrV4,
    /// Optional multicast group to join. When `None` the socket only
    /// receives unicast / broadcast traffic on `bind`.
    pub group: Option<Ipv4Addr>,
    /// Interface to use for multicast reception.
    /// `Ipv4Addr::UNSPECIFIED` defers to the OS default route.
    pub interface: Ipv4Addr,
    /// Target receive buffer size (bytes). Kernel may cap lower.
    pub recv_buf_bytes: usize,
    /// Per-read timeout. `None` = blocking.
    pub read_timeout: Option<Duration>,
    /// Expected starting sequence number (usually 1 after session start).
    pub start_sequence: u64,
    /// Max out-of-order buffered frames before dropping.
    pub max_forward_buffer: usize,
}

impl Default for MulticastConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
            group: None,
            interface: Ipv4Addr::UNSPECIFIED,
            recv_buf_bytes: 8 * 1024 * 1024,
            read_timeout: Some(Duration::from_millis(100)),
            start_sequence: 1,
            max_forward_buffer: 16 * 1024,
        }
    }
}

/// Opens a UDP socket against a MoldUDP64 producer and delivers
/// in-order messages to a sink. Retransmit handling is out of scope
/// for this struct — the caller inspects [`UdpFeedHandler::gap`]
/// after each [`UdpFeedHandler::step`] and asks a rewinder server.
pub struct UdpFeedHandler {
    socket: UdpSocket,
    tracker: GapTracker,
    scratch: Vec<u8>,
}

impl UdpFeedHandler {
    pub fn bind(cfg: &MulticastConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind(cfg.bind)?;
        if let Some(group) = cfg.group {
            socket.join_multicast_v4(&group, &cfg.interface)?;
        }
        // SO_RCVBUF tuning is platform-specific and deliberately left
        // out of this portable handler; production deployments should
        // size the kernel buffer via `sysctl` / `setsockopt` or swap
        // this for a `socket2::Socket` wrapper.
        let _ = cfg.recv_buf_bytes; // documented; not applied here
        socket.set_read_timeout(cfg.read_timeout)?;
        Ok(Self {
            socket,
            tracker: GapTracker::with_capacity(cfg.start_sequence, cfg.max_forward_buffer),
            // MoldUDP64 datagrams are almost always <1500 B (MTU) but
            // the spec permits up to 64 KiB — size the scratch for the
            // worst case.
            scratch: vec![0u8; 65_536],
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn tracker(&self) -> &GapTracker {
        &self.tracker
    }

    pub fn open_gap(&self) -> Option<(u64, u64)> {
        self.tracker.open_gap()
    }

    /// Read one datagram, advance the tracker, and pass every in-order
    /// message to `sink`. Returns `Ok(None)` on a read timeout, which
    /// lets the caller do background work (gap recovery, housekeeping).
    pub fn step<F>(&mut self, mut sink: F) -> io::Result<Option<usize>>
    where
        F: FnMut(&Delivered),
    {
        let (n, _addr) = match self.socket.recv_from(&mut self.scratch) {
            Ok(v) => v,
            Err(e) if would_block(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let delivered = match MoldFrame::parse(&self.scratch[..n]) {
            Ok(frame) => self.tracker.ingest(&frame),
            Err(_) => Vec::new(), // silently drop malformed datagrams
        };
        for d in &delivered {
            sink(d);
        }
        Ok(Some(delivered.len()))
    }

    /// Inject a retransmitted message directly (bypasses the socket).
    /// Caller obtained the `(seq, payload)` pair from a rewinder.
    pub fn ingest_retransmit<F>(&mut self, seq: u64, payload: &[u8], mut sink: F)
    where
        F: FnMut(&Delivered),
    {
        for d in &self.tracker.ingest_single(seq, payload) {
            sink(d);
        }
    }
}

#[inline]
fn would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moldudp::MoldHeader;

    fn encode(session: &[u8; 10], first_seq: u64, msgs: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::with_capacity(20);
        out.extend_from_slice(session);
        out.extend_from_slice(&first_seq.to_be_bytes());
        out.extend_from_slice(&(msgs.len() as u16).to_be_bytes());
        for m in msgs {
            out.extend_from_slice(&(m.len() as u16).to_be_bytes());
            out.extend_from_slice(m);
        }
        out
    }

    const SESSION: &[u8; 10] = b"ITCH50TEST";

    /// Loopback UDP interop: sender → UdpFeedHandler. Guaranteed portable
    /// across Linux/macOS/Windows using 127.0.0.1 on an ephemeral port.
    #[test]
    fn loopback_delivers_in_order() {
        let cfg = MulticastConfig {
            bind: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            read_timeout: Some(Duration::from_secs(2)),
            ..Default::default()
        };
        let mut handler = UdpFeedHandler::bind(&cfg).unwrap();
        let recv_addr = match handler.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            SocketAddr::V6(_) => unreachable!(),
        };

        let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        sender
            .send_to(&encode(SESSION, 1, &[b"msg1", b"msg2"]), recv_addr)
            .unwrap();
        sender
            .send_to(&encode(SESSION, 3, &[b"msg3"]), recv_addr)
            .unwrap();

        let mut received: Vec<(u64, Vec<u8>)> = Vec::new();
        // Two step() calls should cover both datagrams.
        for _ in 0..4 {
            let _ = handler.step(|d| received.push((d.sequence, d.payload.clone())));
            if received.len() >= 3 {
                break;
            }
        }
        assert_eq!(received.len(), 3);
        assert_eq!(received[0], (1, b"msg1".to_vec()));
        assert_eq!(received[2], (3, b"msg3".to_vec()));
        assert!(handler.open_gap().is_none());
    }

    #[test]
    fn loopback_with_gap_then_retransmit() {
        let cfg = MulticastConfig {
            bind: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            read_timeout: Some(Duration::from_millis(500)),
            ..Default::default()
        };
        let mut handler = UdpFeedHandler::bind(&cfg).unwrap();
        let recv_addr = match handler.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            SocketAddr::V6(_) => unreachable!(),
        };

        let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        // Send 1, skip 2, send 3,4.
        sender.send_to(&encode(SESSION, 1, &[b"a"]), recv_addr).unwrap();
        sender.send_to(&encode(SESSION, 3, &[b"c", b"d"]), recv_addr).unwrap();

        let mut received = Vec::<(u64, Vec<u8>)>::new();
        for _ in 0..4 {
            let _ = handler.step(|d| received.push((d.sequence, d.payload.clone())));
        }
        assert_eq!(received.len(), 1, "gap must hold back 3,4");
        assert_eq!(handler.open_gap(), Some((2, 2)));

        // Simulate retransmit arrival.
        handler.ingest_retransmit(2, b"b", |d| {
            received.push((d.sequence, d.payload.clone()))
        });
        let seqs: Vec<u64> = received.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert!(handler.open_gap().is_none());
    }

    #[test]
    fn heartbeat_does_not_advance_sequence() {
        let cfg = MulticastConfig {
            bind: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            read_timeout: Some(Duration::from_millis(500)),
            ..Default::default()
        };
        let mut handler = UdpFeedHandler::bind(&cfg).unwrap();
        let recv_addr = match handler.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            SocketAddr::V6(_) => unreachable!(),
        };
        let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();

        // Heartbeat frame (count=0) followed by a real frame.
        let mut hb = vec![0u8; 20];
        hb[..10].copy_from_slice(SESSION);
        hb[10..18].copy_from_slice(&1u64.to_be_bytes());
        hb[18..20].copy_from_slice(&MoldHeader::HEARTBEAT.to_be_bytes());
        sender.send_to(&hb, recv_addr).unwrap();
        sender
            .send_to(&encode(SESSION, 1, &[b"real"]), recv_addr)
            .unwrap();

        let mut out = Vec::new();
        for _ in 0..4 {
            let _ = handler.step(|d| out.push((d.sequence, d.payload.clone())));
            if !out.is_empty() {
                break;
            }
        }
        assert_eq!(out, vec![(1, b"real".to_vec())]);
    }
}
