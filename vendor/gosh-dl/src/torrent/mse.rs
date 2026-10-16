//! Message Stream Encryption (MSE/PE) Implementation
//!
//! This module implements BitTorrent protocol encryption as defined in the
//! de facto MSE specification. It provides:
//! - Diffie-Hellman key exchange with 768-bit prime
//! - RC4 stream cipher for message encryption (with 1024 byte discard)
//! - Obfuscated handshake negotiation
//! - Transparent encrypted stream wrapper

use std::io;
use std::time::Duration;

use num_bigint::BigUint;
use rand::RngExt;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::metainfo::Sha1Hash;
use crate::error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result};

// ============================================================================
// Constants
// ============================================================================

/// The 768-bit prime P used for Diffie-Hellman key exchange (96 bytes)
/// This is the same prime used by most BitTorrent clients
pub const DH_PRIME: [u8; 96] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34,
    0xC4, 0xC6, 0x62, 0x8B, 0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74,
    0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22, 0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD,
    0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D, 0xF2, 0x5F, 0x14, 0x37,
    0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x37, 0xED, 0x6B, 0x0B, 0xFF, 0x5C, 0xB6, 0xF4, 0x06, 0xB7, 0xED,
];

/// Generator G = 2
pub const DH_GENERATOR: u64 = 2;

/// Verification constant: 8 zero bytes
pub const VC: [u8; 8] = [0u8; 8];

/// Maximum padding length
pub const MAX_PADDING: usize = 512;

/// RC4 discard count (first 1024 bytes discarded)
pub const RC4_DISCARD: usize = 1024;

/// Encryption method: plaintext
pub const CRYPTO_PLAINTEXT: u32 = 0x01;

/// Encryption method: RC4
pub const CRYPTO_RC4: u32 = 0x02;

/// Handshake timeout
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

// ============================================================================
// Configuration
// ============================================================================

/// Encryption policy for peer connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncryptionPolicy {
    /// Disable encryption entirely (plaintext only)
    Disabled,
    /// Allow encryption but don't require it (accept both)
    Allowed,
    /// Prefer encryption, fall back to plaintext if peer doesn't support
    #[default]
    Preferred,
    /// Require encryption (reject non-MSE peers)
    Required,
}

/// Configuration for MSE/PE
#[derive(Debug, Clone)]
pub struct MseConfig {
    /// Encryption policy
    pub policy: EncryptionPolicy,
    /// Allow plaintext in crypto_provide/crypto_select
    pub allow_plaintext: bool,
    /// Allow RC4 in crypto_provide/crypto_select
    pub allow_rc4: bool,
    /// Minimum padding to add (obfuscation)
    pub min_padding: usize,
    /// Maximum padding to add
    pub max_padding: usize,
}

impl Default for MseConfig {
    fn default() -> Self {
        Self {
            policy: EncryptionPolicy::Preferred,
            allow_plaintext: true,
            allow_rc4: true,
            min_padding: 0,
            max_padding: MAX_PADDING,
        }
    }
}

impl MseConfig {
    /// Get the crypto_provide bitfield based on configuration
    pub fn crypto_provide(&self) -> u32 {
        let mut provide = 0u32;
        if self.allow_plaintext {
            provide |= CRYPTO_PLAINTEXT;
        }
        if self.allow_rc4 {
            provide |= CRYPTO_RC4;
        }
        provide
    }
}

// ============================================================================
// RC4 Cipher
// ============================================================================

/// RC4 cipher state
#[derive(Clone)]
pub struct Rc4Cipher {
    state: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4Cipher {
    /// Create a new RC4 cipher with the given key
    pub fn new(key: &[u8]) -> Self {
        let mut state = [0u8; 256];
        for (i, byte) in state.iter_mut().enumerate() {
            *byte = i as u8;
        }

        let mut j: u8 = 0;
        for i in 0..256 {
            j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
            state.swap(i, j as usize);
        }

        let mut cipher = Self { state, i: 0, j: 0 };

        // Discard first 1024 bytes (RC4-drop1024)
        let mut discard = [0u8; RC4_DISCARD];
        cipher.process(&mut discard);

        cipher
    }

    /// Create RC4 cipher without discard (for testing)
    #[cfg(test)]
    pub fn new_no_discard(key: &[u8]) -> Self {
        let mut state = [0u8; 256];
        for (i, byte) in state.iter_mut().enumerate() {
            *byte = i as u8;
        }

        let mut j: u8 = 0;
        for i in 0..256 {
            j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
            state.swap(i, j as usize);
        }

        Self { state, i: 0, j: 0 }
    }

    /// Process data in-place (encrypt or decrypt - symmetric)
    pub fn process(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.state[self.i as usize]);
            self.state.swap(self.i as usize, self.j as usize);
            let k = self.state
                [(self.state[self.i as usize].wrapping_add(self.state[self.j as usize])) as usize];
            *byte ^= k;
        }
    }
}

// ============================================================================
// Diffie-Hellman Key Exchange
// ============================================================================

/// Diffie-Hellman key pair for MSE
pub struct DhKeyPair {
    /// Private key (160-bit random value)
    private: BigUint,
    /// Public key (768-bit value)
    public: [u8; 96],
}

impl DhKeyPair {
    /// Generate a new random key pair
    pub fn generate() -> Self {
        // Generate 160-bit random private key
        let mut private_bytes = [0u8; 20];
        rand::rng().fill(&mut private_bytes);
        let private = BigUint::from_bytes_be(&private_bytes);

        // Compute public = G^private mod P
        let g = BigUint::from(DH_GENERATOR);
        let p = BigUint::from_bytes_be(&DH_PRIME);
        let public_big = g.modpow(&private, &p);

        // Convert to 96-byte array (pad with leading zeros if needed)
        let public_bytes = public_big.to_bytes_be();
        let mut public = [0u8; 96];
        let offset = 96 - public_bytes.len().min(96);
        public[offset..].copy_from_slice(&public_bytes[..public_bytes.len().min(96)]);

        Self { private, public }
    }

    /// Compute shared secret: peer_public^private mod P
    pub fn compute_shared_secret(&self, peer_public: &[u8; 96]) -> [u8; 96] {
        let peer = BigUint::from_bytes_be(peer_public);
        let p = BigUint::from_bytes_be(&DH_PRIME);
        let secret = peer.modpow(&self.private, &p);

        // Convert to 96-byte array
        let secret_bytes = secret.to_bytes_be();
        let mut result = [0u8; 96];
        let offset = 96 - secret_bytes.len().min(96);
        result[offset..].copy_from_slice(&secret_bytes[..secret_bytes.len().min(96)]);

        result
    }

    /// Get the public key bytes
    pub fn public_bytes(&self) -> &[u8; 96] {
        &self.public
    }
}

// ============================================================================
// Key Derivation
// ============================================================================

/// Derive RC4 keys from shared secret and SKEY (info_hash)
pub fn derive_rc4_keys(
    shared_secret: &[u8; 96],
    info_hash: &Sha1Hash,
    is_initiator: bool,
) -> (Rc4Cipher, Rc4Cipher) {
    // keyA = SHA1("keyA" + S + SKEY)
    let mut hasher_a = Sha1::new();
    hasher_a.update(b"keyA");
    hasher_a.update(shared_secret);
    hasher_a.update(info_hash);
    let key_a: [u8; 20] = hasher_a.finalize().into();

    // keyB = SHA1("keyB" + S + SKEY)
    let mut hasher_b = Sha1::new();
    hasher_b.update(b"keyB");
    hasher_b.update(shared_secret);
    hasher_b.update(info_hash);
    let key_b: [u8; 20] = hasher_b.finalize().into();

    // Initiator uses keyA for encrypt, keyB for decrypt
    // Responder uses keyB for encrypt, keyA for decrypt
    if is_initiator {
        (Rc4Cipher::new(&key_a), Rc4Cipher::new(&key_b))
    } else {
        (Rc4Cipher::new(&key_b), Rc4Cipher::new(&key_a))
    }
}

// ============================================================================
// Encrypted Stream
// ============================================================================

/// Encrypted stream wrapper that provides transparent encryption/decryption
pub struct EncryptedStream {
    inner: TcpStream,
    /// Cipher for outgoing data
    encrypt_cipher: Rc4Cipher,
    /// Cipher for incoming data
    decrypt_cipher: Rc4Cipher,
    /// Selected crypto method
    pub crypto_method: u32,
}

impl EncryptedStream {
    /// Create a new encrypted stream
    pub fn new(
        stream: TcpStream,
        encrypt_cipher: Rc4Cipher,
        decrypt_cipher: Rc4Cipher,
        crypto_method: u32,
    ) -> Self {
        Self {
            inner: stream,
            encrypt_cipher,
            decrypt_cipher,
            crypto_method,
        }
    }

    /// Read and decrypt data
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf).await?;
        if n > 0 && self.crypto_method == CRYPTO_RC4 {
            self.decrypt_cipher.process(&mut buf[..n]);
        }
        Ok(n)
    }

    /// Read exactly the requested amount of data
    pub async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_exact(buf).await?;
        if self.crypto_method == CRYPTO_RC4 {
            self.decrypt_cipher.process(buf);
        }
        Ok(())
    }

    /// Encrypt and write data
    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.crypto_method == CRYPTO_RC4 {
            let mut encrypted = buf.to_vec();
            self.encrypt_cipher.process(&mut encrypted);
            self.inner.write_all(&encrypted).await
        } else {
            self.inner.write_all(buf).await
        }
    }

    /// Flush the stream
    pub async fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().await
    }

    /// Get peer address
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.inner.peer_addr()
    }

    /// Get local address
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }

    /// Shutdown the stream
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}

// ============================================================================
// MSE Handshake Result
// ============================================================================

/// Result of MSE handshake
pub enum MseHandshakeResult {
    /// Encrypted connection established
    Encrypted(Box<EncryptedStream>),
    /// Fell back to plaintext (returns stream and any buffered data)
    Plaintext(TcpStream, Vec<u8>),
    /// Handshake failed
    Failed(EngineError),
}

// ============================================================================
// MSE Handshake (Initiator)
// ============================================================================

/// Perform MSE handshake as initiator (outgoing connection)
pub async fn mse_handshake_outgoing(
    mut stream: TcpStream,
    info_hash: Sha1Hash,
    config: &MseConfig,
) -> MseHandshakeResult {
    // Generate our DH key pair
    let key_pair = DhKeyPair::generate();

    // Step 1: Send Ya + random padding
    let padding_len = rand::rng().random_range(config.min_padding..=config.max_padding);
    let mut padding = vec![0u8; padding_len];
    rand::RngExt::fill(&mut rand::rng(), &mut padding[..]);

    let mut send_buf = Vec::with_capacity(96 + padding_len);
    send_buf.extend_from_slice(key_pair.public_bytes());
    send_buf.extend_from_slice(&padding);

    if let Err(e) = timeout(HANDSHAKE_TIMEOUT, stream.write_all(&send_buf)).await {
        return match config.policy {
            EncryptionPolicy::Required => MseHandshakeResult::Failed(EngineError::network(
                NetworkErrorKind::Timeout,
                format!("MSE handshake timeout: {}", e),
            )),
            _ => MseHandshakeResult::Plaintext(stream, vec![]),
        };
    }

    // Step 2: Receive Yb (96 bytes) + possible padding
    let mut yb = [0u8; 96];
    match timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut yb)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return match config.policy {
                EncryptionPolicy::Required => MseHandshakeResult::Failed(EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Failed to receive Yb: {}", e),
                )),
                _ => MseHandshakeResult::Plaintext(stream, vec![]),
            };
        }
        Err(_) => {
            return match config.policy {
                EncryptionPolicy::Required => MseHandshakeResult::Failed(EngineError::network(
                    NetworkErrorKind::Timeout,
                    "Timeout receiving Yb",
                )),
                _ => MseHandshakeResult::Plaintext(stream, vec![]),
            };
        }
    }

    // Step 3: Compute shared secret S
    let shared_secret = key_pair.compute_shared_secret(&yb);

    // Step 4: Send encrypted handshake
    // HASH("req1" + S) + (HASH("req2" + SKEY) XOR HASH("req3" + S))
    // + RC4(VC + crypto_provide + len(PadC) + PadC + len(IA) + IA)

    // HASH("req1" + S)
    let mut hasher = Sha1::new();
    hasher.update(b"req1");
    hasher.update(shared_secret);
    let req1_hash: [u8; 20] = hasher.finalize().into();

    // HASH("req2" + SKEY)
    let mut hasher = Sha1::new();
    hasher.update(b"req2");
    hasher.update(info_hash);
    let req2_hash: [u8; 20] = hasher.finalize().into();

    // HASH("req3" + S)
    let mut hasher = Sha1::new();
    hasher.update(b"req3");
    hasher.update(shared_secret);
    let req3_hash: [u8; 20] = hasher.finalize().into();

    // XOR req2 and req3
    let mut skey_hash = [0u8; 20];
    for i in 0..20 {
        skey_hash[i] = req2_hash[i] ^ req3_hash[i];
    }

    // Build crypto_provide (which methods we support)
    let mut crypto_provide: u32 = 0;
    if config.allow_rc4 {
        crypto_provide |= CRYPTO_RC4;
    }
    if config.allow_plaintext {
        crypto_provide |= CRYPTO_PLAINTEXT;
    }

    // Create encryption keys for the encrypted part
    let (mut encrypt_cipher, _) = derive_rc4_keys(&shared_secret, &info_hash, true);

    // Build the encrypted payload
    // VC (8) + crypto_provide (4) + len(PadC) (2) + PadC + len(IA) (2) + IA
    let padc_len: u16 = rand::rng().random_range(0..512);
    let mut padc = vec![0u8; padc_len as usize];
    rand::RngExt::fill(&mut rand::rng(), &mut padc[..]);

    // IA = Initial payload (we send nothing initially)
    let ia_len: u16 = 0;

    let mut encrypted_part = Vec::new();
    encrypted_part.extend_from_slice(&VC);
    encrypted_part.extend_from_slice(&crypto_provide.to_be_bytes());
    encrypted_part.extend_from_slice(&padc_len.to_be_bytes());
    encrypted_part.extend_from_slice(&padc);
    encrypted_part.extend_from_slice(&ia_len.to_be_bytes());

    // Encrypt the payload
    encrypt_cipher.process(&mut encrypted_part);

    // Send: req1_hash + skey_hash + encrypted_part
    let mut send_buf = Vec::new();
    send_buf.extend_from_slice(&req1_hash);
    send_buf.extend_from_slice(&skey_hash);
    send_buf.extend_from_slice(&encrypted_part);

    if let Err(e) = timeout(HANDSHAKE_TIMEOUT, stream.write_all(&send_buf)).await {
        return MseHandshakeResult::Failed(EngineError::network(
            NetworkErrorKind::Timeout,
            format!("Failed to send crypto handshake: {}", e),
        ));
    }

    // Step 5: Receive response
    // Need to find VC in the stream, then read crypto_select.
    // Only derive the decrypt cipher here -- the encrypt cipher from step 4
    // must be reused so its keystream position reflects the handshake payload
    // already encrypted above.
    let (_, decrypt_cipher) = derive_rc4_keys(&shared_secret, &info_hash, true);

    match receive_crypto_response(&mut stream, decrypt_cipher, config).await {
        Ok((crypto_method, final_decrypt)) => {
            // Reuse encrypt_cipher from step 4: its state has advanced past
            // the handshake payload. A fresh cipher would desync with the peer.
            MseHandshakeResult::Encrypted(Box::new(EncryptedStream::new(
                stream,
                encrypt_cipher,
                final_decrypt,
                crypto_method,
            )))
        }
        Err(e) => MseHandshakeResult::Failed(e),
    }
}

/// Receive and parse crypto_select response
async fn receive_crypto_response(
    stream: &mut TcpStream,
    mut decrypt_cipher: Rc4Cipher,
    _config: &MseConfig,
) -> Result<(u32, Rc4Cipher)> {
    // Format on the wire: PadB (0..=512 plaintext bytes, sent by mainstream
    // clients) followed by encrypted(VC (8) + crypto_select (4) + len(PadD)
    // (2) + PadD). The responder's RC4 keystream starts at VC, so the
    // encrypted VC is a fixed 8-byte pattern we can scan for to skip PadB.
    const MAX_PADB: usize = 512;
    let expected_vc = {
        let mut probe = decrypt_cipher.clone();
        let mut vc = VC;
        probe.process(&mut vc);
        vc
    };

    // Scan byte-by-byte (bounded, once per connection) so no bytes beyond
    // the VC marker are consumed from the stream.
    let mut window: Vec<u8> = Vec::with_capacity(expected_vc.len());
    let mut skipped = 0usize;
    loop {
        if window.len() == expected_vc.len() && window == expected_vc {
            break;
        }
        if skipped > MAX_PADB {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                "Invalid VC in response (not found within PadB window)",
            ));
        }
        let mut byte = [0u8; 1];
        timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut byte))
            .await
            .map_err(|_| {
                EngineError::network(NetworkErrorKind::Timeout, "Timeout reading response")
            })?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Failed to read response: {}", e),
                )
            })?;
        if window.len() == expected_vc.len() {
            window.remove(0);
            skipped += 1;
        }
        window.push(byte[0]);
    }

    // Advance our cipher past the VC bytes we just matched.
    decrypt_cipher.process(&mut window);

    // Read and decrypt crypto_select (4) + len(PadD) (2)
    let mut buf = [0u8; 6];
    timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .map_err(|_| EngineError::network(NetworkErrorKind::Timeout, "Timeout reading response"))?
        .map_err(|e| {
            EngineError::network(
                NetworkErrorKind::ConnectionReset,
                format!("Failed to read response: {}", e),
            )
        })?;
    decrypt_cipher.process(&mut buf);

    // Parse crypto_select
    let crypto_select = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);

    // Parse PadD length
    let padd_len = u16::from_be_bytes([buf[4], buf[5]]) as usize;

    // Read and discard PadD
    if padd_len > 0 {
        let mut padd = vec![0u8; padd_len];
        timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut padd))
            .await
            .map_err(|_| EngineError::network(NetworkErrorKind::Timeout, "Timeout reading PadD"))?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Failed to read PadD: {}", e),
                )
            })?;
        decrypt_cipher.process(&mut padd);
    }

    Ok((crypto_select, decrypt_cipher))
}

// ============================================================================
// Peer Stream Abstraction
// ============================================================================

/// Stream that can be either plaintext or encrypted
pub enum PeerStream {
    /// Plain TCP stream
    Plain(TcpStream),
    /// Encrypted stream (boxed to reduce enum size)
    Encrypted(Box<EncryptedStream>),
    /// uTP stream (micro Transport Protocol over UDP)
    Utp(super::utp::UtpSocket),
}

impl PeerStream {
    /// Read data from the stream
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf).await,
            Self::Encrypted(s) => s.as_mut().read(buf).await,
            Self::Utp(s) => s.read(buf).await,
        }
    }

    /// Read exact amount of data
    pub async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.read_exact(buf).await.map(|_| ()),
            Self::Encrypted(s) => s.as_mut().read_exact(buf).await,
            Self::Utp(s) => s.read_exact(buf).await,
        }
    }

    /// Write data to the stream
    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.write_all(buf).await,
            Self::Encrypted(s) => s.as_mut().write_all(buf).await,
            Self::Utp(s) => s.write_all(buf).await,
        }
    }

    /// Flush the stream
    pub async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush().await,
            Self::Encrypted(s) => s.as_mut().flush().await,
            Self::Utp(s) => s.flush().await,
        }
    }

    /// Check if stream is encrypted
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted(_))
    }

    /// Get peer address
    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            Self::Plain(s) => s.peer_addr(),
            Self::Encrypted(s) => s.as_ref().peer_addr(),
            Self::Utp(s) => s.peer_addr(),
        }
    }

    /// Shutdown the stream
    pub async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.shutdown().await,
            Self::Encrypted(s) => s.as_mut().shutdown().await,
            Self::Utp(s) => s.shutdown().await,
        }
    }
}

// ============================================================================
// Helper: Connect with MSE negotiation
// ============================================================================

/// Connect to peer with MSE negotiation based on policy
pub async fn connect_with_mse(
    stream: TcpStream,
    info_hash: Sha1Hash,
    config: &MseConfig,
) -> Result<PeerStream> {
    match config.policy {
        EncryptionPolicy::Disabled => Ok(PeerStream::Plain(stream)),
        EncryptionPolicy::Allowed | EncryptionPolicy::Preferred | EncryptionPolicy::Required => {
            match mse_handshake_outgoing(stream, info_hash, config).await {
                MseHandshakeResult::Encrypted(enc) => Ok(PeerStream::Encrypted(enc)),
                MseHandshakeResult::Plaintext(stream, _) => {
                    if config.policy == EncryptionPolicy::Required {
                        Err(EngineError::protocol(
                            ProtocolErrorKind::PeerProtocol,
                            "Encryption required but peer doesn't support it",
                        ))
                    } else {
                        Ok(PeerStream::Plain(stream))
                    }
                }
                MseHandshakeResult::Failed(e) => {
                    // Non-Required policies fall back to a fresh plaintext
                    // connection one level up (PeerConnection::connect).
                    Err(e)
                }
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rc4_cipher() {
        // Test that encryption followed by decryption returns original
        let key = b"test_key_1234567";
        let mut cipher1 = Rc4Cipher::new_no_discard(key);
        let mut cipher2 = Rc4Cipher::new_no_discard(key);

        let original = b"Hello, World! This is a test message.";
        let mut data = original.to_vec();

        // Encrypt
        cipher1.process(&mut data);
        assert_ne!(&data[..], &original[..]);

        // Decrypt
        cipher2.process(&mut data);
        assert_eq!(&data[..], &original[..]);
    }

    #[test]
    fn test_dh_key_exchange() {
        let alice = DhKeyPair::generate();
        let bob = DhKeyPair::generate();

        let secret_a = alice.compute_shared_secret(bob.public_bytes());
        let secret_b = bob.compute_shared_secret(alice.public_bytes());

        assert_eq!(secret_a, secret_b, "Shared secrets should match");
    }

    #[test]
    fn test_key_derivation() {
        let shared_secret = [0x42u8; 96];
        let info_hash = [0x12u8; 20];

        let (enc_a, dec_a) = derive_rc4_keys(&shared_secret, &info_hash, true);
        let (enc_b, dec_b) = derive_rc4_keys(&shared_secret, &info_hash, false);

        // Test that A's encrypt matches B's decrypt
        let original = b"test data for encryption";
        let mut data = original.to_vec();

        let mut enc_a = enc_a;
        let mut dec_b = dec_b;

        enc_a.process(&mut data);
        dec_b.process(&mut data);
        assert_eq!(&data[..], &original[..]);

        // Test reverse direction
        let mut data = original.to_vec();
        let mut enc_b = enc_b;
        let mut dec_a = dec_a;

        enc_b.process(&mut data);
        dec_a.process(&mut data);
        assert_eq!(&data[..], &original[..]);
    }

    #[test]
    fn test_mse_config_default() {
        let config = MseConfig::default();
        assert_eq!(config.policy, EncryptionPolicy::Preferred);
        assert!(config.allow_plaintext);
        assert!(config.allow_rc4);
    }

    // ========================================================================
    // Encryption Policy Tests
    // ========================================================================

    #[test]
    fn test_encryption_policy_disabled() {
        let policy = EncryptionPolicy::Disabled;
        assert_eq!(policy, EncryptionPolicy::Disabled);
    }

    #[test]
    fn test_encryption_policy_default_is_preferred() {
        let policy = EncryptionPolicy::default();
        assert_eq!(policy, EncryptionPolicy::Preferred);
    }

    #[test]
    fn test_mse_config_crypto_provide() {
        // Test that crypto_provide reflects config options
        let mut config = MseConfig::default();

        // Both allowed
        config.allow_plaintext = true;
        config.allow_rc4 = true;
        let provide = config.crypto_provide();
        assert_eq!(provide, CRYPTO_PLAINTEXT | CRYPTO_RC4);

        // Only RC4
        config.allow_plaintext = false;
        config.allow_rc4 = true;
        let provide = config.crypto_provide();
        assert_eq!(provide, CRYPTO_RC4);

        // Only plaintext
        config.allow_plaintext = true;
        config.allow_rc4 = false;
        let provide = config.crypto_provide();
        assert_eq!(provide, CRYPTO_PLAINTEXT);

        // Neither (edge case)
        config.allow_plaintext = false;
        config.allow_rc4 = false;
        let provide = config.crypto_provide();
        assert_eq!(provide, 0);
    }

    #[test]
    fn test_crypto_constants() {
        // Verify the crypto method constants are correct
        assert_eq!(CRYPTO_PLAINTEXT, 0x01);
        assert_eq!(CRYPTO_RC4, 0x02);

        // They should be different bits for OR'ing
        assert_eq!(CRYPTO_PLAINTEXT & CRYPTO_RC4, 0);
    }

    #[test]
    fn test_dh_prime_length() {
        // DH prime should be 768 bits = 96 bytes
        assert_eq!(DH_PRIME.len(), 96);
    }

    #[test]
    fn test_rc4_discard_constant() {
        // MSE spec requires discarding first 1024 bytes
        assert_eq!(RC4_DISCARD, 1024);
    }

    #[test]
    fn test_vc_constant() {
        // Verification constant should be 8 zero bytes
        assert_eq!(VC.len(), 8);
        assert!(VC.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_max_padding() {
        // Max padding per MSE spec
        assert_eq!(MAX_PADDING, 512);
    }

    #[test]
    fn test_rc4_discard_security() {
        // Test that RC4 with discard differs from without
        let key = b"security_test_key";

        let mut with_discard = Rc4Cipher::new(key);
        let mut without_discard = Rc4Cipher::new_no_discard(key);

        let mut data1 = vec![0u8; 32];
        let mut data2 = vec![0u8; 32];

        with_discard.process(&mut data1);
        without_discard.process(&mut data2);

        // Output should differ due to 1024-byte discard
        assert_ne!(
            data1, data2,
            "RC4 with discard should produce different output"
        );
    }

    #[test]
    fn test_dh_generates_unique_keys() {
        // Each key pair should be unique
        let kp1 = DhKeyPair::generate();
        let kp2 = DhKeyPair::generate();

        assert_ne!(
            kp1.public_bytes(),
            kp2.public_bytes(),
            "Generated key pairs should be unique"
        );
    }

    #[test]
    fn test_bidirectional_encryption() {
        let shared_secret = [0xABu8; 96];
        let info_hash = [0xCDu8; 20];

        // Simulate two peers
        let (mut enc_a, mut dec_a) = derive_rc4_keys(&shared_secret, &info_hash, true);
        let (mut enc_b, mut dec_b) = derive_rc4_keys(&shared_secret, &info_hash, false);

        // A sends to B
        let msg_a_to_b = b"Hello from A";
        let mut encrypted = msg_a_to_b.to_vec();
        enc_a.process(&mut encrypted);
        dec_b.process(&mut encrypted);
        assert_eq!(&encrypted[..], msg_a_to_b);

        // B sends to A
        let msg_b_to_a = b"Hello from B";
        let mut encrypted = msg_b_to_a.to_vec();
        enc_b.process(&mut encrypted);
        dec_a.process(&mut encrypted);
        assert_eq!(&encrypted[..], msg_b_to_a);
    }

    /// Full outgoing handshake against a mock responder that sends PadB —
    /// the random plaintext padding mainstream clients put between Yb and
    /// the encrypted response, which the initiator previously choked on
    /// with "Invalid VC".
    #[tokio::test]
    async fn test_outgoing_handshake_with_padb() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let info_hash: Sha1Hash = [0x42u8; 20];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let responder = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();

            // Read Ya (initiator configured with zero padding below)
            let mut ya = [0u8; 96];
            sock.read_exact(&mut ya).await.unwrap();

            let keypair = DhKeyPair::generate();
            let shared = keypair.compute_shared_secret(&ya);

            // Send Yb followed by PadB (137 arbitrary plaintext bytes)
            sock.write_all(keypair.public_bytes()).await.unwrap();
            let padb = vec![0x5Au8; 137];
            sock.write_all(&padb).await.unwrap();

            // Read req1/skey hashes and the initiator's encrypted request
            let mut hashes = [0u8; 40];
            sock.read_exact(&mut hashes).await.unwrap();
            let (mut enc_b, mut dec_a) = derive_rc4_keys(&shared, &info_hash, false);
            let mut head = [0u8; 14]; // VC + crypto_provide + len(PadC)
            sock.read_exact(&mut head).await.unwrap();
            dec_a.process(&mut head);
            assert_eq!(&head[..8], &VC, "responder saw bad VC from initiator");
            let padc_len = u16::from_be_bytes([head[12], head[13]]) as usize;
            let mut padc = vec![0u8; padc_len];
            sock.read_exact(&mut padc).await.unwrap();
            dec_a.process(&mut padc);
            let mut ia_len = [0u8; 2];
            sock.read_exact(&mut ia_len).await.unwrap();
            dec_a.process(&mut ia_len);

            // Reply: ENCRYPTED(VC + crypto_select(RC4) + len(PadD) + PadD)
            let mut reply = Vec::new();
            reply.extend_from_slice(&VC);
            reply.extend_from_slice(&CRYPTO_RC4.to_be_bytes());
            let padd = [0x77u8; 21];
            reply.extend_from_slice(&(padd.len() as u16).to_be_bytes());
            reply.extend_from_slice(&padd);
            enc_b.process(&mut reply);
            sock.write_all(&reply).await.unwrap();

            // Send one encrypted payload so the test proves stream sync
            let mut payload = b"ping".to_vec();
            enc_b.process(&mut payload);
            sock.write_all(&payload).await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let config = MseConfig {
            policy: EncryptionPolicy::Required,
            allow_rc4: true,
            allow_plaintext: false,
            min_padding: 0,
            max_padding: 0,
        };
        let result = mse_handshake_outgoing(stream, info_hash, &config).await;
        let MseHandshakeResult::Encrypted(mut enc_stream) = result else {
            panic!("handshake did not produce an encrypted stream");
        };

        let mut buf = [0u8; 4];
        enc_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping", "encrypted stream out of sync after PadB");

        responder.await.unwrap();
    }
}
