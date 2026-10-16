//! uTP (Micro Transport Protocol) Implementation (BEP 29)
//!
//! This module implements the uTP protocol, a UDP-based transport layer
//! designed for BitTorrent. uTP provides:
//!
//! - Reliable, ordered delivery over UDP
//! - LEDBAT congestion control (yields to other traffic)
//! - Lower latency than TCP in many network conditions
//!
//! # Architecture
//!
//! - `packet`: Packet encoding/decoding (20-byte header + extensions + payload)
//! - `congestion`: LEDBAT congestion control algorithm
//! - `state`: Connection state machine
//! - `socket`: Single uTP connection
//! - `multiplexer`: Shared UDP socket for multiple connections
//!
//! # Usage
//!
//! ```ignore
//! use gosh_dl::torrent::utp::{UtpMux, UtpConfig};
//!
//! // Create multiplexer
//! let mux = UtpMux::bind("0.0.0.0:6881".parse()?).await?;
//!
//! // Connect to peer
//! let socket = mux.connect("192.168.1.100:6881".parse()?).await?;
//!
//! // Send/receive data
//! socket.write_all(b"hello").await?;
//! let mut buf = [0u8; 1024];
//! let n = socket.read(&mut buf).await?;
//! ```

pub mod congestion;
pub mod multiplexer;
pub mod packet;
pub mod socket;
pub mod state;

// Re-export commonly used types
pub use multiplexer::UtpMux;
pub use socket::{UtpConfig, UtpSocket};
