//! TCP telemetry server.
//!
//! Single-client at a time (Go API is the only consumer). On accept, we drain
//! the bounded mpsc and write framed bytes to the socket. Disconnect = drop
//! the consumer-side iterator and wait for the next accept; engine keeps
//! running so back-loaded buffers reset cleanly.
//!
//! Frame layout — see `wire.rs`:
//!
//! ```text
//! [u32 len LE][u16 version LE][payload]
//! ```
//!
//! Length is `version + payload` (2 + N bytes), NOT including the u32 itself.

use crate::backpressure::Consumer;
use crate::wire::{Codec, TelemetryFrame, WIRE_VERSION};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use tracing::{info, warn};

pub struct TelemetryServer {
    listener: TcpListener,
    codec: Codec,
}

impl TelemetryServer {
    pub fn bind(addr: &str, codec: Codec) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        info!(target: "engine", addr, ?codec, "telemetry server listening");
        Ok(Self { listener, codec })
    }

    /// Block forever, accepting one client at a time and forwarding frames
    /// from `consumer` until the socket closes.
    pub fn serve(&self, consumer: Arc<Consumer<TelemetryFrame>>) -> std::io::Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = self.handle_client(s, &consumer) {
                        warn!(target: "engine", error = %e, "client loop ended");
                    }
                }
                Err(e) => warn!(target: "engine", error = %e, "accept failed"),
            }
        }
        Ok(())
    }

    fn handle_client(
        &self,
        mut stream: TcpStream,
        consumer: &Consumer<TelemetryFrame>,
    ) -> std::io::Result<()> {
        // Disable Nagle: telemetry is many small writes that need to land fast.
        let _ = stream.set_nodelay(true);
        info!(target: "engine", peer = %stream.peer_addr()?, "client connected");

        let mut buf = Vec::with_capacity(2048);

        // Drain forever. recv() blocks until a frame is available; this thread
        // exists solely for IO so blocking is fine.
        while let Some(frame) = consumer.recv() {
            buf.clear();
            // Reserve length prefix; rewritten after payload encoding.
            buf.extend_from_slice(&[0u8; 4]);
            buf.extend_from_slice(&WIRE_VERSION.to_le_bytes());

            let payload = match self.codec.encode(&frame) {
                Ok(b) => b,
                Err(e) => {
                    warn!(target: "engine", error = %e, "encode failed");
                    continue;
                }
            };
            buf.extend_from_slice(&payload);

            // Patch length: bytes after the u32 = 2 (version) + payload.len()
            let len = (2u32 + payload.len() as u32).to_le_bytes();
            buf[..4].copy_from_slice(&len);

            if let Err(e) = stream.write_all(&buf) {
                warn!(target: "engine", error = %e, "write failed, closing client");
                return Err(e);
            }
        }
        Ok(())
    }
}
