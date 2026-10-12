//! Encrypted, mutually authenticated channel between devices and the server.
//!
//! Uses the Noise protocol's XX pattern: both sides prove they hold a
//! long-term key and learn the other's public key, then every message is
//! encrypted. There are no certificates. Instead each side pins the other's
//! key at adoption ("trust on first use"), and from then on a fake device or
//! fake server is rejected.
//!
//! The device is always the initiator:
//!
//! ```text
//! device -> server   handshake message 1
//! server -> device   handshake message 2   (server's key)
//! device -> server   handshake message 3   (device's key)
//! ...encrypted messages in both directions...
//! ```

use std::{fs, io, path::Path, sync::Arc};

use snow::{params::NoiseParams, Builder, HandshakeState, StatelessTransportState};

pub use snow::Error;

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// Mixed into the handshake, so a peer speaking another protocol version
/// fails the handshake instead of misreading messages.
const PROLOGUE: &[u8] = b"alosa-device/1";
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const MAX_MESSAGE: usize = 65535;

/// Largest plaintext that fits in one encrypted message. Bulk data (firmware,
/// e-ink images) is fetched separately and verified by a hash sent here.
pub const MAX_PAYLOAD: usize = MAX_MESSAGE - TAG_LEN;

fn params() -> NoiseParams {
    PATTERN.parse().expect("valid Noise pattern")
}

/// A long-term X25519 keypair identifying a device or the server.
pub struct Identity {
    private: Vec<u8>,
    public: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Result<Self, Error> {
        let keypair = Builder::new(params()).generate_keypair()?;
        Ok(Self {
            private: keypair.private,
            public: keypair.public,
        })
    }

    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Loads the identity from `path`, creating it (readable only by the
    /// owner) on first run.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match Self::load(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate().map_err(io::Error::other)?;
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                write_private(
                    path,
                    &[identity.private.as_slice(), &identity.public].concat(),
                )?;
                Ok(identity)
            }
            result => result,
        }
    }

    /// Loads an existing identity. Fails with `NotFound` if there is none.
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        if bytes.len() != KEY_LEN * 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a valid identity key", path.display()),
            ));
        }
        Ok(Self {
            private: bytes[..KEY_LEN].to_vec(),
            public: bytes[KEY_LEN..].to_vec(),
        })
    }
}

/// Writes a file only the owner can read. Used for keys and tokens.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(contents)
}

/// Lowercase hex encoding of a key.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Short, human-checkable form of a public key, e.g. `3f2a-91c0-7b4e-d815`.
/// Shown on the device and in Home Assistant so the user can compare them.
pub fn fingerprint(public_key: &[u8]) -> String {
    let hex = to_hex(&public_key[..public_key.len().min(8)]);
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("hex is ascii"))
        .collect::<Vec<_>>()
        .join("-")
}

/// A handshake in progress. Call `write`/`read` in the order shown in the
/// module docs, then `finish`.
pub struct Handshake {
    state: HandshakeState,
}

impl Handshake {
    pub fn initiator(identity: &Identity) -> Result<Self, Error> {
        let state = Builder::new(params())
            .local_private_key(&identity.private)?
            .prologue(PROLOGUE)?
            .build_initiator()?;
        Ok(Self { state })
    }

    pub fn responder(identity: &Identity) -> Result<Self, Error> {
        let state = Builder::new(params())
            .local_private_key(&identity.private)?
            .prologue(PROLOGUE)?
            .build_responder()?;
        Ok(Self { state })
    }

    pub fn write(&mut self) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0u8; MAX_MESSAGE];
        let len = self.state.write_message(&[], &mut buf)?;
        buf.truncate(len);
        Ok(buf)
    }

    pub fn read(&mut self, message: &[u8]) -> Result<(), Error> {
        let mut buf = vec![0u8; MAX_MESSAGE];
        self.state.read_message(message, &mut buf)?;
        Ok(())
    }

    /// Completes the handshake, returning the peer's public key and the two
    /// halves of the encrypted channel.
    pub fn finish(self) -> Result<(Vec<u8>, Sender, Receiver), Error> {
        let remote = self
            .state
            .get_remote_static()
            .ok_or(Error::State(
                snow::error::StateProblem::HandshakeNotFinished,
            ))?
            .to_vec();
        let state = Arc::new(self.state.into_stateless_transport_mode()?);
        Ok((
            remote,
            Sender {
                state: state.clone(),
                nonce: 0,
            },
            Receiver { state, nonce: 0 },
        ))
    }
}

/// Encrypts outgoing messages. Separate from [`Receiver`] so a writer task
/// and a reader task can each own one half.
pub struct Sender {
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

impl Sender {
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0u8; plaintext.len() + TAG_LEN];
        let len = self.state.write_message(self.nonce, plaintext, &mut buf)?;
        self.nonce += 1;
        buf.truncate(len);
        Ok(buf)
    }
}

/// Decrypts incoming messages. Messages must arrive in order, which a
/// WebSocket guarantees; a replayed, dropped or tampered message fails.
pub struct Receiver {
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

impl Receiver {
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0u8; ciphertext.len()];
        let len = self.state.read_message(self.nonce, ciphertext, &mut buf)?;
        self.nonce += 1;
        buf.truncate(len);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Peer = (Vec<u8>, Sender, Receiver);

    fn connect(device: &Identity, server: &Identity) -> (Peer, Peer) {
        let mut d = Handshake::initiator(device).unwrap();
        let mut s = Handshake::responder(server).unwrap();
        s.read(&d.write().unwrap()).unwrap();
        d.read(&s.write().unwrap()).unwrap();
        s.read(&d.write().unwrap()).unwrap();
        (d.finish().unwrap(), s.finish().unwrap())
    }

    #[test]
    fn handshake_exchanges_keys_and_encrypts() {
        let device = Identity::generate().unwrap();
        let server = Identity::generate().unwrap();
        let ((server_key, mut d_tx, mut d_rx), (device_key, mut s_tx, mut s_rx)) =
            connect(&device, &server);

        assert_eq!(server_key, server.public_key());
        assert_eq!(device_key, device.public_key());

        let sealed = d_tx.encrypt(b"hello").unwrap();
        assert_ne!(&sealed[..5], b"hello");
        assert_eq!(s_rx.decrypt(&sealed).unwrap(), b"hello");
        assert_eq!(
            d_rx.decrypt(&s_tx.encrypt(b"welcome").unwrap()).unwrap(),
            b"welcome"
        );
    }

    #[test]
    fn tampered_or_replayed_messages_fail() {
        let ((_, mut d_tx, _), (_, _, mut s_rx)) = connect(
            &Identity::generate().unwrap(),
            &Identity::generate().unwrap(),
        );

        let mut sealed = d_tx.encrypt(b"turn on").unwrap();
        sealed[0] ^= 1;
        assert!(s_rx.decrypt(&sealed).is_err());

        let ((_, mut d_tx, _), (_, _, mut s_rx)) = connect(
            &Identity::generate().unwrap(),
            &Identity::generate().unwrap(),
        );
        let sealed = d_tx.encrypt(b"turn on").unwrap();
        s_rx.decrypt(&sealed).unwrap();
        assert!(s_rx.decrypt(&sealed).is_err(), "replay must be rejected");
    }

    #[test]
    fn identity_persists() {
        let dir = std::env::temp_dir().join(format!("alosa-test-{}", std::process::id()));
        let path = dir.join("identity.key");
        let first = Identity::load_or_create(&path).unwrap();
        let second = Identity::load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fingerprint_format() {
        assert_eq!(
            fingerprint(&[0x3f, 0x2a, 0x91, 0xc0, 0x7b, 0x4e, 0xd8, 0x15, 0xff]),
            "3f2a-91c0-7b4e-d815"
        );
    }
}
