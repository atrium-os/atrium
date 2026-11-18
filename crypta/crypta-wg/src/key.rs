//! The static key: used, never read (vpn.md §4.3).

use zeroize::Zeroizing;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// The key's holder refused or could not be reached.
    Unavailable(String),
    /// The result was all zeros: the point was of small order.
    LowOrder,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Unavailable(why) => write!(f, "the static key is unavailable: {why}"),
            KeyError::LowOrder => write!(f, "a low-order point"),
        }
    }
}

/// Our static X25519 key, as something that does Diffie-Hellman for us.
/// The engine's implementation asks arcad (`DH_X25519`); the private half
/// stays there.
pub trait StaticKey {
    fn public(&self) -> [u8; 32];
    /// X25519(private, `point`). An all-zero result is an error.
    fn dh(&self, point: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, KeyError>;
}

/// A static key held in memory: tests and host tools only.
pub struct LocalKey {
    secret: x25519_dalek::StaticSecret,
    public: [u8; 32],
}

impl LocalKey {
    pub fn from_bytes(b: [u8; 32]) -> LocalKey {
        let secret = x25519_dalek::StaticSecret::from(b);
        let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
        LocalKey { secret, public }
    }

    pub fn generate() -> LocalKey {
        let mut b = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(&mut b[..]).expect("random");
        LocalKey::from_bytes(*b)
    }

    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes())
    }
}

impl StaticKey for LocalKey {
    fn public(&self) -> [u8; 32] {
        self.public
    }

    fn dh(&self, point: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, KeyError> {
        dh(&self.secret, point)
    }
}

impl<K: StaticKey + ?Sized> StaticKey for std::sync::Arc<K> {
    fn public(&self) -> [u8; 32] {
        (**self).public()
    }
    fn dh(&self, point: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, KeyError> {
        (**self).dh(point)
    }
}

/// X25519 with a secret held here (ephemeral keys, `LocalKey`).
pub fn dh(secret: &x25519_dalek::StaticSecret, point: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, KeyError> {
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(*point));
    if !shared.was_contributory() {
        return Err(KeyError::LowOrder);
    }
    Ok(Zeroizing::new(shared.to_bytes()))
}
