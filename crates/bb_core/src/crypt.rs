// crypt -- the keys between the machines; the relay holds none of them
//
//   Keys            a device's X25519 key pair (made once, kept in its config file)
//   pair_key        the key two paired devices share: X25519 of their static keys through HKDF
//   seal / open     a message under such a key: XChaCha20-Poly1305 with a random 24-byte nonce;
//                   the associated data names sender and channel, so a message put into another
//                   channel or sent back to its author does not open
//   code_*          the pairing code people type (60 random bits) and what Argon2id makes of it:
//                   the slot at the relay and the key of the two pairing messages
//   DataKey         the fleet's data key (made by the main machine, handed to each worker when it
//                   pairs): blob ids (keyed BLAKE3 of the content), blob encryption
//   token_hash      what the relay keeps of a device's access token
//
// What the relay could do with what it sees: drop, delay or replay sealed messages (jobs are
// keyed by their id, a replayed start is ignored), and count bytes.  It cannot read sources,
// logs or artifacts, forge a job, or learn a pairing code from its slot.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crypto::aead;
use crypto::argon2;
use crypto::blake3;
use crypto::x25519;

/// a device's key pair
#[derive(Clone)]
pub struct Keys {
    pub secret: [u8; 32],
    pub public: [u8; 32],
}

impl Keys {
    /// a new key pair from the system's random numbers
    pub fn generate() -> Keys {
        Keys::from_secret(plat::random_bytes())
    }

    pub fn from_secret(secret: [u8; 32]) -> Keys {
        Keys { public: x25519::public_key(&secret), secret }
    }

    /// from the hex text of the secret (a config file)
    pub fn from_hex(s: &str) -> Option<Keys> {
        key32(s).map(Keys::from_secret)
    }

    pub fn secret_hex(&self) -> String {
        crypto::hex(&self.secret)
    }

    pub fn public_hex(&self) -> String {
        crypto::hex(&self.public)
    }
}

/// 32 bytes from 64 hex characters
pub fn key32(s: &str) -> Option<[u8; 32]> {
    let b = crypto::unhex(s.trim())?;
    let mut k = [0u8; 32];
    (b.len() == 32).then(|| {
        k.copy_from_slice(&b);
        k
    })
}

/// The key two paired devices share.  Both sides get the same one whoever computes it (the
/// public keys go into the derivation in sorted order).  None: the other key is not a usable
/// point (all zero shared secret).
pub fn pair_key(mine: &Keys, theirs: &[u8; 32]) -> Option<[u8; 32]> {
    let shared = x25519::shared(&mine.secret, theirs)?;
    let (lo, hi) = if mine.public <= *theirs { (&mine.public, theirs) } else { (theirs, &mine.public) };
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(lo);
    info.extend_from_slice(hi);
    let mut out = [0u8; 32];
    crypto::hkdf_sha256(b"build-balancer/pair-key/v1", &shared, &info, &mut out);
    Some(out)
}

/// the associated data of a sealed message: who wrote it and where it goes
pub fn aad(sender: &str, channel: &str) -> Vec<u8> {
    format!("bb1|{}|{}", sender, channel).into_bytes()
}

/// nonce ‖ ciphertext ‖ tag
pub fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce: [u8; 24] = plat::random_bytes();
    let mut out = Vec::with_capacity(24 + plain.len() + 16);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&aead::x_seal(key, &nonce, aad, plain));
    out
}

/// the plaintext of seal() (None: cut short, forged, another key or other associated data)
pub fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 24 + 16 {
        return None;
    }
    let nonce: [u8; 24] = sealed[..24].try_into().ok()?;
    aead::x_open(key, &nonce, aad, &sealed[24..])
}

/// a new pairing code: "K7QF-2M9X-PD4T", 60 random bits
pub fn code_new() -> String {
    let c = crate::util::random_chars(12);
    format!("{}-{}-{}", &c[0..4], &c[4..8], &c[8..12])
}

/// a typed code in its plain form: upper case, without dashes and blanks (a mistyped code is
/// simply another code: it finds no slot and nothing pairs)
pub fn code_norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// whether a text has the form of a pairing code
pub fn code_ok(s: &str) -> bool {
    let n = code_norm(s);
    n.len() == 12 && n.bytes().all(|b| crate::util::B32.contains(&b))
}

/// What both sides make of a pairing code: the slot they meet in at the relay (hex) and the key
/// that seals their two messages.  Argon2id (32 MiB, 2 passes) with the relay's id as salt: the
/// relay sees the slot, and going from it back to the code costs that much for every guess --
/// far too slow to finish while the slot lives (minutes).
pub fn code_secrets(code: &str, relay_id: &str) -> (String, [u8; 32]) {
    let params = argon2::Params { m_kib: 32 * 1024, t: 2, p: 1, out_len: 48 };
    let salt = format!("build-balancer/pair/v1/{}", relay_id);
    let mut out = [0u8; 48];
    argon2::argon2id(code_norm(code).as_bytes(), salt.as_bytes(), &params, &mut out);
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[16..48]);
    (crypto::hex(&out[..16]), key)
}

/// what the relay keeps of an access token (hex of BLAKE3)
pub fn token_hash(token: &str) -> String {
    crypto::hex(&blake3::hash(token.as_bytes()))
}

/// a new access token
pub fn token_new() -> String {
    crypto::b64::encode_url(&plat::random_bytes::<24>())
}

/// "AB:CD:.." of a certificate's SHA-256, for people to compare
pub fn fingerprint_text(d: &[u8]) -> String {
    let h = crypto::hex(d).to_uppercase();
    h.as_bytes().chunks(2).map(|c| core::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(":")
}

/// how a blob's content is packed before it is sealed
const PACK_NONE: u8 = 0;
const PACK_LZ4: u8 = 1;
const PACK_ZSTD: u8 = 2;

/// The fleet's data key and what is derived from it.  Every machine of a fleet has it, so a
/// blob one of them uploaded serves all of them, and equal content is stored once.
#[derive(Clone)]
pub struct DataKey {
    id_key: [u8; 32],
    enc_key: [u8; 32],
    nonce_key: [u8; 32],
}

impl DataKey {
    pub fn new(master: &[u8; 32]) -> DataKey {
        DataKey {
            id_key: blake3::derive_key("build-balancer 2026 blob id", master),
            enc_key: blake3::derive_key("build-balancer 2026 blob encryption", master),
            nonce_key: blake3::derive_key("build-balancer 2026 blob nonce", master),
        }
    }

    /// the id of a piece of content: keyed, so the relay cannot test a guess of a file against it
    pub fn id(&self, plain: &[u8]) -> [u8; 32] {
        blake3::keyed_hash(&self.id_key, plain)
    }

    /// An incremental id over pieces (a file read chunk by chunk has one id per chunk; this is
    /// for content that arrives in parts).
    pub fn hasher(&self) -> blake3::Blake3 {
        blake3::Blake3::new_keyed(&self.id_key)
    }

    /// The blob of a piece of content: a pack byte, then the sealed (packed) content.  The
    /// nonce comes from the id, so equal content gives the equal blob and an upload can be
    /// repeated; that is safe because key, nonce and plaintext then repeat together.
    pub fn seal(&self, id: &[u8; 32], plain: &[u8]) -> Vec<u8> {
        let (pack, body) = pack(plain);
        let nonce = self.nonce(id);
        let mut out = Vec::with_capacity(1 + body.len() + 16);
        out.push(pack);
        out.extend_from_slice(&aead::x_seal(&self.enc_key, &nonce, &blob_aad(id, pack), &body));
        out
    }

    /// The content of a blob, checked twice: the seal, and that the content has the id it was
    /// asked for by.  None for anything else, also content longer than `max`.
    pub fn open(&self, id: &[u8; 32], blob: &[u8], max: usize) -> Option<Vec<u8>> {
        let (&pack, sealed) = blob.split_first()?;
        let body = aead::x_open(&self.enc_key, &self.nonce(id), &blob_aad(id, pack), sealed)?;
        let plain = match pack {
            PACK_NONE => (body.len() <= max).then_some(body)?,
            PACK_LZ4 => compress::lz4::decompress(&body, max)?,
            PACK_ZSTD => compress::zstd::decompress(&body, max)?,
            _ => return None,
        };
        crypto::ct_eq(&self.id(&plain), id).then_some(plain)
    }

    fn nonce(&self, id: &[u8; 32]) -> [u8; 24] {
        let h = blake3::keyed_hash(&self.nonce_key, id);
        let mut n = [0u8; 24];
        n.copy_from_slice(&h[..24]);
        n
    }
}

fn blob_aad(id: &[u8; 32], pack: u8) -> Vec<u8> {
    let mut a = Vec::with_capacity(40);
    a.extend_from_slice(b"bb1blob");
    a.push(pack);
    a.extend_from_slice(id);
    a
}

/// Pack content: zstd for small pieces (sources: the best ratio, little time), LZ4 for large
/// ones (speed matters more than the last percent), stored as it is when packing does not help
/// (archives, images, most of a compiler's output compress badly a second time).
fn pack(plain: &[u8]) -> (u8, Vec<u8>) {
    if plain.len() < 64 {
        return (PACK_NONE, plain.to_vec());
    }
    let (kind, packed) = if plain.len() <= 256 << 10 { (PACK_ZSTD, compress::zstd::compress(plain, 3)) } else { (PACK_LZ4, compress::lz4::compress(plain)) };
    if packed.len() + packed.len() / 16 < plain.len() { (kind, packed) } else { (PACK_NONE, plain.to_vec()) }
}

/// The setup key a fresh relay prints for its main machine: the one-time token and the
/// relay's certificate fingerprint in one string to copy (base64url of 16 + 32 bytes; the
/// fingerprint is all zero for a relay without TLS).
pub fn setup_key_encode(token: &[u8; 16], fingerprint: Option<&[u8; 32]>) -> String {
    let mut b = Vec::with_capacity(48);
    b.extend_from_slice(token);
    b.extend_from_slice(fingerprint.unwrap_or(&[0u8; 32]));
    crypto::b64::encode_url(&b)
}

/// the token of a setup key as the relay hashes it
pub fn setup_token_text(token: &[u8; 16]) -> String {
    crypto::b64::encode_url(token)
}

/// a setup key -> (the token's text, the fingerprint as hex or "" for none)
pub fn setup_key_decode(key: &str) -> Option<(String, String)> {
    let b = crypto::b64::decode_url(key.trim())?;
    if b.len() != 48 {
        return None;
    }
    let token: [u8; 16] = b[..16].try_into().ok()?;
    let fp = if b[16..].iter().all(|x| *x == 0) { String::new() } else { crypto::hex(&b[16..]) };
    Some((setup_token_text(&token), fp))
}
