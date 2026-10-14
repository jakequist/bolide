//! Apple Remote Desktop authentication — RFB security type 30.
//!
//! The transform every macOS Screen Sharing server offers, and the one bolide needs to
//! authenticate **as a named user** rather than to a shared screen. Classic VNC auth
//! (type 2) carries a password and no username, so it can only reach whatever session
//! owns the console; type 30 carries both, which is how a connection reaches a Mac
//! sitting at its login window, and how it names *which* user it is.
//!
//! The shape, measured against macOS (remotex's `apple-vnc-889.md`, checked against
//! this Mac's `screensharingd`):
//!
//! 1. The server sends a Diffie-Hellman challenge: a `u16` generator, a `u16` key
//!    length `L`, then the `L`-byte prime and the server's `L`-byte public key. macOS
//!    26 sends RFC 5054's 4096-bit group (`L == 512`, generator 5).
//! 2. The client picks a private exponent, sends back a **128-byte credential block**
//!    (the username at offset 0 and the password at offset 64, each NUL-terminated,
//!    the remainder random) encrypted with **AES-128 in ECB mode** under
//!    `MD5(shared secret)`, followed by its own `L`-byte public key.
//! 3. The server replies with a bare `u32` SecurityResult.
//!
//! This module is the **single statement** of that transform: `bolide-testkit`'s fake
//! server decrypts with these same functions rather than restating the cipher, so the
//! fake cannot quietly disagree with a real Mac. The primitives are pure — the private
//! exponent and the block's random filler come in as arguments — so a test drives a
//! full DH round trip and the known AES and MD5 vectors with no randomness at all.
//!
//! Like everything in `bolide-rfb`, this only ever *encrypts outgoing* credentials and
//! derives a key; the password is never logged, stored or returned.

use aes::Aes128;
use cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use md5::{Digest, Md5};
use num_bigint::BigUint;

/// The fixed size of the ARD credential block, in bytes: the username at offset 0 and
/// the password at offset [`PASSWORD_OFFSET`], each NUL-terminated, the rest random.
pub const CREDENTIAL_LEN: usize = 128;
/// Where the password begins inside the credential block.
pub const PASSWORD_OFFSET: usize = 64;
/// The longest a username or password may be: one byte is reserved for the NUL.
pub const FIELD_MAX: usize = PASSWORD_OFFSET - 1;

/// The key-length bounds Apple's own viewer accepts, in bytes. A server that asks for a
/// group outside this range is refused rather than trusted.
pub const MIN_KEY_LEN: usize = 64;
/// See [`MIN_KEY_LEN`].
pub const MAX_KEY_LEN: usize = 1024;

/// What went wrong building an ARD response. Kept separate from the wire so the caller
/// can turn each into the right [`crate::Error`] variant.
#[derive(Debug, PartialEq, Eq)]
pub enum ArdError {
    /// A username or password did not fit in [`FIELD_MAX`] bytes.
    FieldTooLong,
    /// The server's key length was outside [`MIN_KEY_LEN`]..=[`MAX_KEY_LEN`], or the
    /// prime and public key disagreed on it.
    BadKeyLength,
}

/// `generator ^ private mod prime`, left-padded to the prime's width — the client's
/// public value, and the same computation the server does for its own.
pub fn dh_public(generator: u32, prime: &[u8], private: &[u8]) -> Vec<u8> {
    let g = BigUint::from(generator);
    let p = BigUint::from_bytes_be(prime);
    let x = BigUint::from_bytes_be(private);
    pad_be(&g.modpow(&x, &p).to_bytes_be(), prime.len())
}

/// `peer_public ^ private mod prime`, left-padded to the prime's width — the shared
/// secret, identical on both ends of the exchange.
pub fn dh_shared(peer_public: &[u8], prime: &[u8], private: &[u8]) -> Vec<u8> {
    let peer = BigUint::from_bytes_be(peer_public);
    let p = BigUint::from_bytes_be(prime);
    let x = BigUint::from_bytes_be(private);
    pad_be(&peer.modpow(&x, &p).to_bytes_be(), prime.len())
}

/// The AES-128 key for the credential block: `MD5(shared secret)`.
pub fn aes_key(shared: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(shared);
    hasher.finalize().into()
}

/// Lay out the 128-byte credential block: the username at 0, the password at
/// [`PASSWORD_OFFSET`], each NUL-terminated, every other byte taken from `filler`.
///
/// `filler` supplies the random bytes so the plaintext is unguessable; a test passes
/// zeros and gets a deterministic block. It must be exactly [`CREDENTIAL_LEN`] bytes.
pub fn pack_credentials(
    username: &str,
    password: &str,
    filler: &[u8; CREDENTIAL_LEN],
) -> Result<[u8; CREDENTIAL_LEN], ArdError> {
    if username.len() > FIELD_MAX || password.len() > FIELD_MAX {
        return Err(ArdError::FieldTooLong);
    }
    // Start from the random filler, then overwrite each field and its NUL terminator.
    let mut block = *filler;
    block[..username.len()].copy_from_slice(username.as_bytes());
    block[username.len()] = 0;
    block[PASSWORD_OFFSET..PASSWORD_OFFSET + password.len()].copy_from_slice(password.as_bytes());
    block[PASSWORD_OFFSET + password.len()] = 0;
    Ok(block)
}

/// Read a username and password back out of a decrypted credential block — the inverse
/// of [`pack_credentials`], for the fake server and its tests.
pub fn unpack_credentials(block: &[u8; CREDENTIAL_LEN]) -> (String, String) {
    (
        read_field(&block[..PASSWORD_OFFSET]),
        read_field(&block[PASSWORD_OFFSET..]),
    )
}

fn read_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Encrypt `data` in place with AES-128 in ECB mode. `data` must be a whole number of
/// 16-byte blocks.
pub fn aes_ecb_encrypt(key: &[u8; 16], data: &mut [u8]) {
    let cipher = Aes128::new(key.into());
    for block in data.chunks_exact_mut(16) {
        cipher.encrypt_block(block.into());
    }
}

/// Decrypt `data` in place with AES-128 in ECB mode — the inverse of
/// [`aes_ecb_encrypt`], for the fake server.
pub fn aes_ecb_decrypt(key: &[u8; 16], data: &mut [u8]) {
    let cipher = Aes128::new(key.into());
    for block in data.chunks_exact_mut(16) {
        cipher.decrypt_block(block.into());
    }
}

/// The complete client response to the server's DH challenge: the encrypted 128-byte
/// credential block followed by the client's public key.
///
/// `private` is the client's private exponent (the caller draws it at random — Apple's
/// viewer uses twice the key length in bytes) and `filler` randomises the block. Both
/// are arguments so the whole payload is a pure function of them, which is what lets a
/// test assert it byte for byte.
pub fn client_response(
    generator: u32,
    prime: &[u8],
    server_public: &[u8],
    username: &str,
    password: &str,
    private: &[u8],
    filler: &[u8; CREDENTIAL_LEN],
) -> Result<Vec<u8>, ArdError> {
    let len = prime.len();
    if !(MIN_KEY_LEN..=MAX_KEY_LEN).contains(&len) || server_public.len() != len {
        return Err(ArdError::BadKeyLength);
    }
    let shared = dh_shared(server_public, prime, private);
    let key = aes_key(&shared);
    let mut block = pack_credentials(username, password, filler)?;
    aes_ecb_encrypt(&key, &mut block);

    let mut out = Vec::with_capacity(CREDENTIAL_LEN + len);
    out.extend_from_slice(&block);
    out.extend_from_slice(&dh_public(generator, prime, private));
    Ok(out)
}

/// Left-pad a big-endian byte string to `width`, or return it unchanged when already at
/// least that wide. `BigUint::to_bytes_be` drops leading zeros, and the wire wants the
/// full key-length field.
fn pad_be(bytes: &[u8], width: usize) -> Vec<u8> {
    if bytes.len() >= width {
        return bytes.to_vec();
    }
    let mut out = vec![0u8; width - bytes.len()];
    out.extend_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn aes_128_ecb_matches_the_fips_197_vector() {
        // FIPS-197 Appendix C.1.
        let key: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut data = hex("00112233445566778899aabbccddeeff");
        aes_ecb_encrypt(&key, &mut data);
        assert_eq!(data, hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes_ecb_decrypt(&key, &mut data);
        assert_eq!(data, hex("00112233445566778899aabbccddeeff"));
    }

    #[test]
    fn aes_key_is_the_md5_of_the_shared_secret() {
        // RFC 1321's MD5("abc").
        assert_eq!(
            aes_key(b"abc").to_vec(),
            hex("900150983cd24fb0d6963f7d28e17f72")
        );
    }

    #[test]
    fn diffie_hellman_round_trips() {
        // The textbook small group: p = 23, g = 5.
        let prime = [23u8];
        let alice = [6u8];
        let bob = [15u8];
        let alice_pub = dh_public(5, &prime, &alice); // 5^6 mod 23 = 8
        let bob_pub = dh_public(5, &prime, &bob); // 5^15 mod 23 = 19
        assert_eq!(alice_pub, [8]);
        assert_eq!(bob_pub, [19]);
        // Both sides reach the same secret: 2.
        let a = dh_shared(&bob_pub, &prime, &alice);
        let b = dh_shared(&alice_pub, &prime, &bob);
        assert_eq!(a, [2]);
        assert_eq!(a, b);
    }

    #[test]
    fn dh_public_is_padded_to_the_prime_width() {
        // A result with leading zero bytes still fills the key-length field.
        let prime = [0x00, 0x17]; // 23, two bytes wide
        let pubkey = dh_public(5, &prime, &[6]); // 8, but two bytes out
        assert_eq!(pubkey, [0x00, 0x08]);
    }

    #[test]
    fn credentials_round_trip_through_the_block() {
        let filler = [0u8; CREDENTIAL_LEN];
        let block = pack_credentials("meteor", "hunter2", &filler).unwrap();
        // Username at 0, NUL-terminated; password at 64, NUL-terminated.
        assert_eq!(&block[..6], b"meteor");
        assert_eq!(block[6], 0);
        assert_eq!(&block[64..71], b"hunter2");
        assert_eq!(block[71], 0);
        assert_eq!(
            unpack_credentials(&block),
            ("meteor".into(), "hunter2".into())
        );
    }

    #[test]
    fn credential_filler_fills_the_unused_bytes() {
        let filler = [0xABu8; CREDENTIAL_LEN];
        let block = pack_credentials("a", "b", &filler).unwrap();
        // Byte after "a\0" in the username field is still filler.
        assert_eq!(block[2], 0xAB);
        // Byte after "b\0" in the password field is still filler.
        assert_eq!(block[PASSWORD_OFFSET + 2], 0xAB);
    }

    #[test]
    fn an_overlong_field_is_rejected() {
        let filler = [0u8; CREDENTIAL_LEN];
        let long = "x".repeat(FIELD_MAX + 1);
        assert_eq!(
            pack_credentials(&long, "p", &filler),
            Err(ArdError::FieldTooLong)
        );
    }

    #[test]
    fn a_response_server_can_undo_recovers_the_credentials() {
        // A full exchange over the small group, both ends using these functions.
        let prime = [23u8];
        let server_priv = [9u8];
        let server_pub = dh_public(5, &prime, &server_priv);
        let client_priv = [4u8];
        let filler = [0x55u8; CREDENTIAL_LEN];

        // The key length guard rejects this toy group, so exercise the pieces directly.
        let shared = dh_shared(&server_pub, &prime, &client_priv);
        let key = aes_key(&shared);
        let mut block = pack_credentials("meteor", "meteor", &filler).unwrap();
        aes_ecb_encrypt(&key, &mut block);

        // Server side: same shared secret from the client's public key.
        let client_pub = dh_public(5, &prime, &client_priv);
        let server_shared = dh_shared(&client_pub, &prime, &server_priv);
        assert_eq!(shared, server_shared);
        let mut recovered = block;
        aes_ecb_decrypt(&aes_key(&server_shared), &mut recovered);
        assert_eq!(
            unpack_credentials(&recovered),
            ("meteor".into(), "meteor".into())
        );
    }

    #[test]
    fn client_response_rejects_a_key_length_apple_would_not() {
        let prime = [23u8]; // one byte: below MIN_KEY_LEN
        let filler = [0u8; CREDENTIAL_LEN];
        assert_eq!(
            client_response(5, &prime, &[8], "u", "p", &[6], &filler),
            Err(ArdError::BadKeyLength)
        );
    }

    #[test]
    fn client_response_lays_out_block_then_public_key() {
        // A prime at the minimum width so the guard passes.
        let mut prime = vec![0u8; MIN_KEY_LEN];
        *prime.last_mut().unwrap() = 23;
        prime[0] = 0x80; // make it genuinely MIN_KEY_LEN bytes wide and odd/large
        let server_pub = dh_public(5, &prime, &[7]);
        let filler = [0u8; CREDENTIAL_LEN];
        let out = client_response(5, &prime, &server_pub, "meteor", "meteor", &[3], &filler)
            .expect("valid key length");
        assert_eq!(out.len(), CREDENTIAL_LEN + MIN_KEY_LEN);
        // The trailing key-length bytes are the client's public value.
        assert_eq!(&out[CREDENTIAL_LEN..], &dh_public(5, &prime, &[3])[..]);
    }
}
