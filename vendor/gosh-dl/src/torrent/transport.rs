//! Transport Abstraction Layer
//!
//! This module provides a unified interface for different transport protocols
//! (TCP, uTP) used in BitTorrent peer connections.

use std::io;
use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::net::TcpStream;

use super::mse::{EncryptedStream, PeerStream};

/// Transport type identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    /// Standard TCP connection
    Tcp,
    /// uTP (Micro Transport Protocol) over UDP
    Utp,
}

impl std::fmt::Display for TransportType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "TCP"),
            Self::Utp => write!(f, "uTP"),
        }
    }
}

/// Trait for peer transport implementations
///
/// This trait abstracts over different transport protocols (TCP, uTP)
/// allowing the peer connection code to work with any transport.
#[async_trait]
pub trait PeerTransport: Send + Sync {
    /// Get the remote peer's address
    fn peer_addr(&self) -> io::Result<SocketAddr>;

    /// Get the local address
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Get the transport type
    fn transport_type(&self) -> TransportType;

    /// Check if the transport is encrypted (MSE/PE)
    fn is_encrypted(&self) -> bool;

    /// Read exactly `buf.len()` bytes from the transport
    async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;

    /// Read some bytes from the transport (may return less than buf.len())
    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Write all bytes to the transport
    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;

    /// Flush any buffered data
    async fn flush(&mut self) -> io::Result<()>;

    /// Shutdown the transport
    async fn shutdown(&mut self) -> io::Result<()>;
}

/// TCP transport implementation
pub struct TcpTransport {
    stream: PeerStream,
}

impl TcpTransport {
    /// Create a new TCP transport from a plain TcpStream
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream: PeerStream::Plain(stream),
        }
    }

    /// Create a new TCP transport from an encrypted stream
    pub fn encrypted(stream: EncryptedStream) -> Self {
        Self {
            stream: PeerStream::Encrypted(Box::new(stream)),
        }
    }

    /// Create from a PeerStream
    pub fn from_peer_stream(stream: PeerStream) -> Self {
        Self { stream }
    }
}

#[async_trait]
impl PeerTransport for TcpTransport {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.stream {
            PeerStream::Plain(s) => s.local_addr(),
            PeerStream::Encrypted(s) => s.local_addr(),
            PeerStream::Utp(s) => s.peer_addr(), // uTP doesn't go through TcpTransport
        }
    }

    fn transport_type(&self) -> TransportType {
        TransportType::Tcp
    }

    fn is_encrypted(&self) -> bool {
        self.stream.is_encrypted()
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.stream.read_exact(buf).await
    }

    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf).await
    }

    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.stream.write_all(buf).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.stream.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await
    }
}

/// uTP transport implementation
pub struct UtpTransport {
    socket: super::utp::UtpSocket,
    local_addr: SocketAddr,
}

impl UtpTransport {
    /// Create a new uTP transport from a UtpSocket
    pub fn new(socket: super::utp::UtpSocket, local_addr: SocketAddr) -> Self {
        Self { socket, local_addr }
    }
}

#[async_trait]
impl PeerTransport for UtpTransport {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.socket.peer_addr()
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    fn transport_type(&self) -> TransportType {
        TransportType::Utp
    }

    fn is_encrypted(&self) -> bool {
        false
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.socket.read_exact(buf).await
    }

    async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.read(buf).await
    }

    async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.socket.write_all(buf).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.socket.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.socket.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_type_display() {
        assert_eq!(format!("{}", TransportType::Tcp), "TCP");
        assert_eq!(format!("{}", TransportType::Utp), "uTP");
    }
}
