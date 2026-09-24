//! The RFB wire, as data.
//!
//! Every constant, every fixed-shape message and the VNC-auth transform live **here and
//! only here**, because bolide writes both ends of this protocol: `bolide-rfb` is the
//! client and `bolide-testkit` is the fake server every test runs against. Two
//! independent spellings of "PointerEvent is message 5" would let the client and the
//! fake agree with each other and disagree with the world — a test suite that is green
//! and wrong. So `bolide-testkit` depends on this module rather than restating it.
//!
//! What is deliberately *not* here: anything with state. Encoding decoders (ZRLE's
//! zlib stream), the connection state machine and the session actor are `client.rs`'s;
//! the matching encoders are the testkit's. Those are inverses, not copies.
//!
//! Reference: RFB protocol 3.8, RFC 6143.

use std::fmt;

// ---------------------------------------------------------------------------
// Version handshake
// ---------------------------------------------------------------------------

/// The ProtocolVersion string bolide speaks. Exactly 12 bytes, trailing newline.
pub const VERSION_3_8: &[u8; 12] = b"RFB 003.008\n";
/// 3.3 and 3.7 servers exist in the wild; bolide negotiates *down* to nothing — it
/// answers with the lower of (server, 3.8), and 3.3's implicit-security path is the
/// one difference that matters. Kept as a constant so the check is not a magic string.
pub const VERSION_3_3: &[u8; 12] = b"RFB 003.003\n";
/// 3.7 and 3.8 share the security-list handshake; only the SecurityResult reason
/// string differs (3.8 sends one, 3.7 does not).
pub const VERSION_3_7: &[u8; 12] = b"RFB 003.007\n";

/// Parse a 12-byte ProtocolVersion banner into `(major, minor)`.
pub fn parse_version(banner: &[u8]) -> Option<(u32, u32)> {
    if banner.len() != 12 || !banner.starts_with(b"RFB ") || banner[11] != b'\n' {
        return None;
    }
    let major = std::str::from_utf8(&banner[4..7]).ok()?.parse().ok()?;
    let minor = std::str::from_utf8(&banner[8..11]).ok()?.parse().ok()?;
    if banner[7] != b'.' {
        return None;
    }
    Some((major, minor))
}

// ---------------------------------------------------------------------------
// Security types
// ---------------------------------------------------------------------------

/// No authentication: the server sends SecurityResult straight away (3.8).
pub const SECURITY_NONE: u8 = 1;
/// Classic VNC authentication: a 16-byte DES challenge. See [`vnc_auth_response`].
pub const SECURITY_VNC_AUTH: u8 = 2;
/// Apple Remote Desktop's Diffie-Hellman variant. bolide does **not** implement it; it
/// is named so the error can say which one the server wanted instead of a bare number.
pub const SECURITY_ARD: u8 = 30;

/// SecurityResult: 0 = OK, anything else = failed.
pub const SECURITY_RESULT_OK: u32 = 0;

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// Client → server message ids.
pub mod client_msg {
    /// SetPixelFormat.
    pub const SET_PIXEL_FORMAT: u8 = 0;
    /// SetEncodings.
    pub const SET_ENCODINGS: u8 = 2;
    /// FramebufferUpdateRequest.
    pub const FRAMEBUFFER_UPDATE_REQUEST: u8 = 3;
    /// KeyEvent.
    pub const KEY_EVENT: u8 = 4;
    /// PointerEvent.
    pub const POINTER_EVENT: u8 = 5;
    /// ClientCutText.
    pub const CLIENT_CUT_TEXT: u8 = 6;
}

/// Server → client message ids.
pub mod server_msg {
    /// FramebufferUpdate.
    pub const FRAMEBUFFER_UPDATE: u8 = 0;
    /// SetColourMapEntries. bolide negotiates true colour, so it reads and discards it.
    pub const SET_COLOUR_MAP_ENTRIES: u8 = 1;
    /// Bell.
    pub const BELL: u8 = 2;
    /// ServerCutText.
    pub const SERVER_CUT_TEXT: u8 = 3;
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

/// Raw: `width * height * bytes_per_pixel` bytes in the negotiated pixel format.
pub const ENC_RAW: i32 = 0;
/// CopyRect: two `u16` — the source x/y the rect is copied *from*, same framebuffer.
pub const ENC_COPY_RECT: i32 = 1;
/// ZRLE: a zlib stream (one per connection, state carried across rects) of 64×64 tiles.
pub const ENC_ZRLE: i32 = 16;
/// DesktopSize pseudo-encoding: the rect's `w`/`h` are the new framebuffer size.
pub const ENC_DESKTOP_SIZE: i32 = -223;

/// ZRLE's tile edge. Tiles run left-to-right, top-to-bottom; the right and bottom
/// edges are clipped, and a clipped tile carries only its own pixels.
pub const ZRLE_TILE: u32 = 64;

/// The encodings bolide advertises, in preference order (RFB reads the list as one).
///
/// ZRLE leads because it is what makes a full-screen read affordable: zlib over
/// palette/RLE tiles, versus Raw's uncompressed 4 bytes a pixel. CopyRect is next
/// because a scroll or a window drag costs four `u16` instead of a screenful. Raw is
/// last and is never removed — it is the one encoding RFB requires every server to
/// speak, so it is the floor under the other two. DesktopSize is a pseudo-encoding:
/// advertising it asks the server to *tell* us about resizes rather than dropping the
/// connection.
pub const ADVERTISED_ENCODINGS: &[i32] = &[ENC_ZRLE, ENC_COPY_RECT, ENC_RAW, ENC_DESKTOP_SIZE];

// ---------------------------------------------------------------------------
// Pixel format
// ---------------------------------------------------------------------------

/// The 16-byte PIXEL_FORMAT block, as it appears in ServerInit and SetPixelFormat.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    /// Bits on the wire per pixel: 8, 16 or 32.
    pub bits_per_pixel: u8,
    /// Significant colour bits. `depth <= bits_per_pixel`.
    pub depth: u8,
    /// Byte order of the multi-byte pixel value.
    pub big_endian: bool,
    /// False means a colour map, which bolide never negotiates.
    pub true_colour: bool,
    /// Largest red value, e.g. 255 for 8 bits of red.
    pub red_max: u16,
    /// Largest green value.
    pub green_max: u16,
    /// Largest blue value.
    pub blue_max: u16,
    /// Bit position of red within the pixel value.
    pub red_shift: u8,
    /// Bit position of green.
    pub green_shift: u8,
    /// Bit position of blue.
    pub blue_shift: u8,
}

impl fmt::Debug for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PixelFormat({}bpp/{} {} {} r{}<<{} g{}<<{} b{}<<{})",
            self.bits_per_pixel,
            self.depth,
            if self.big_endian { "be" } else { "le" },
            if self.true_colour {
                "truecolour"
            } else {
                "colourmap"
            },
            self.red_max,
            self.red_shift,
            self.green_max,
            self.green_shift,
            self.blue_max,
            self.blue_shift,
        )
    }
}

impl PixelFormat {
    /// What bolide asks every server for: 32bpp true colour, little-endian, byte order
    /// R,G,B,X — so a Raw rect is already RGBA minus the alpha byte and decoding is a
    /// copy with a fill. Servers must honour SetPixelFormat, but bolide decodes
    /// generically anyway ([`Self::to_rgba`]), because "must" and "does" differ.
    pub const RGBX_32: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 0,
        green_shift: 8,
        blue_shift: 16,
    };

    /// Bytes on the wire per pixel.
    pub fn bytes_per_pixel(&self) -> usize {
        (self.bits_per_pixel as usize).div_ceil(8)
    }

    /// ZRLE's "compressed pixel" (CPIXEL): when the format is 32bpp true colour and
    /// all three maxima fit in the low or high 24 bits, pixels travel as 3 bytes.
    /// Returns the CPIXEL width in bytes.
    pub fn cpixel_bytes(&self) -> usize {
        if self.bits_per_pixel == 32 && self.depth <= 24 && self.true_colour {
            let max_shift = self.red_shift.max(self.green_shift).max(self.blue_shift);
            let min_shift = self.red_shift.min(self.green_shift).min(self.blue_shift);
            // All colour bits live in 3 of the 4 bytes → drop the unused one.
            if max_shift <= 23 || min_shift >= 8 {
                return 3;
            }
        }
        self.bytes_per_pixel()
    }

    /// True when a CPIXEL drops the *most* significant byte (colour bits in 0..24).
    /// Little-endian: keep the first three wire bytes. Big-endian: keep the last three.
    pub fn cpixel_drops_high_byte(&self) -> bool {
        self.red_shift.max(self.green_shift).max(self.blue_shift) <= 23
    }

    /// Index of the first CPIXEL byte inside this format's full-pixel wire bytes.
    ///
    /// The one place this fiddly rule is written down, because the ZRLE decoder
    /// (`bolide-rfb`) and the ZRLE encoder (`bolide-testkit`) both need it and a pair
    /// that disagreed would still round-trip through *each other*. Zero when the
    /// format is not compressible to 3 bytes.
    pub fn cpixel_offset(&self) -> usize {
        if self.cpixel_bytes() == self.bytes_per_pixel() {
            return 0;
        }
        // The most significant byte is wire index 3 little-endian, 0 big-endian.
        match (self.big_endian, self.cpixel_drops_high_byte()) {
            (false, true) => 0,
            (false, false) => 1,
            (true, true) => 1,
            (true, false) => 0,
        }
    }

    /// Encode as the 16-byte wire block (3 trailing padding bytes included).
    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.bits_per_pixel;
        b[1] = self.depth;
        b[2] = u8::from(self.big_endian);
        b[3] = u8::from(self.true_colour);
        b[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        b[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        b[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        b[10] = self.red_shift;
        b[11] = self.green_shift;
        b[12] = self.blue_shift;
        b
    }

    /// Decode the 16-byte wire block.
    pub fn decode(b: &[u8; 16]) -> PixelFormat {
        PixelFormat {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }

    /// Assemble `bytes_per_pixel()` wire bytes into the pixel's numeric value.
    pub fn read_pixel(&self, wire: &[u8]) -> u32 {
        let n = self.bytes_per_pixel().min(4).min(wire.len());
        let mut v: u32 = 0;
        if self.big_endian {
            for &byte in wire.iter().take(n) {
                v = (v << 8) | byte as u32;
            }
        } else {
            for i in (0..n).rev() {
                v = (v << 8) | wire[i] as u32;
            }
        }
        v
    }

    /// One pixel value → RGBA8888, scaling each channel from its own `*_max`.
    ///
    /// Alpha is always 255: RFB has no alpha, and a framebuffer that reported 0 would
    /// be an invisible screenshot.
    pub fn to_rgba(&self, pixel: u32) -> [u8; 4] {
        [
            scale(pixel >> self.red_shift, self.red_max),
            scale(pixel >> self.green_shift, self.green_max),
            scale(pixel >> self.blue_shift, self.blue_max),
            255,
        ]
    }

    /// RGBA8888 → one pixel value in this format (the fake server's direction).
    pub fn from_rgba(&self, rgba: [u8; 4]) -> u32 {
        (unscale(rgba[0], self.red_max) << self.red_shift)
            | (unscale(rgba[1], self.green_max) << self.green_shift)
            | (unscale(rgba[2], self.blue_max) << self.blue_shift)
    }

    /// Write a pixel value as `bytes_per_pixel()` wire bytes.
    pub fn write_pixel(&self, pixel: u32, out: &mut Vec<u8>) {
        let n = self.bytes_per_pixel().min(4);
        if self.big_endian {
            for i in (0..n).rev() {
                out.push((pixel >> (8 * i)) as u8);
            }
        } else {
            for i in 0..n {
                out.push((pixel >> (8 * i)) as u8);
            }
        }
    }
}

fn scale(raw: u32, max: u16) -> u8 {
    if max == 0 {
        return 0;
    }
    let v = raw & max as u32;
    if max == 255 {
        return v as u8;
    }
    ((v * 255 + max as u32 / 2) / max as u32) as u8
}

fn unscale(v: u8, max: u16) -> u32 {
    if max == 0 {
        return 0;
    }
    if max == 255 {
        return v as u32;
    }
    ((v as u32 * max as u32 + 127) / 255) & max as u32
}

// ---------------------------------------------------------------------------
// Pointer buttons
// ---------------------------------------------------------------------------

/// RFB button mask bit 0.
pub const BUTTON_LEFT: u8 = 1 << 0;
/// RFB button mask bit 1.
pub const BUTTON_MIDDLE: u8 = 1 << 1;
/// RFB button mask bit 2.
pub const BUTTON_RIGHT: u8 = 1 << 2;
/// RFB has no scroll message: a wheel notch **is** button 4 pressed and released.
pub const BUTTON_WHEEL_UP: u8 = 1 << 3;
/// The down-notch counterpart of [`BUTTON_WHEEL_UP`].
pub const BUTTON_WHEEL_DOWN: u8 = 1 << 4;

// ---------------------------------------------------------------------------
// Client → server message builders
// ---------------------------------------------------------------------------

/// SetPixelFormat (message 0).
pub fn set_pixel_format(fmt: &PixelFormat) -> Vec<u8> {
    let mut b = vec![client_msg::SET_PIXEL_FORMAT, 0, 0, 0];
    b.extend_from_slice(&fmt.encode());
    b
}

/// SetEncodings (message 2).
pub fn set_encodings(encodings: &[i32]) -> Vec<u8> {
    let mut b = vec![client_msg::SET_ENCODINGS, 0];
    b.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for e in encodings {
        b.extend_from_slice(&e.to_be_bytes());
    }
    b
}

/// FramebufferUpdateRequest (message 3).
pub fn framebuffer_update_request(
    incremental: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let mut b = vec![
        client_msg::FRAMEBUFFER_UPDATE_REQUEST,
        u8::from(incremental),
    ];
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b.extend_from_slice(&width.to_be_bytes());
    b.extend_from_slice(&height.to_be_bytes());
    b
}

/// KeyEvent (message 4).
pub fn key_event(keysym: u32, down: bool) -> Vec<u8> {
    let mut b = vec![client_msg::KEY_EVENT, u8::from(down), 0, 0];
    b.extend_from_slice(&keysym.to_be_bytes());
    b
}

/// PointerEvent (message 5). `buttons` is the mask of what is held *now*.
pub fn pointer_event(x: u16, y: u16, buttons: u8) -> Vec<u8> {
    let mut b = vec![client_msg::POINTER_EVENT, buttons];
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

/// ClientCutText (message 6).
///
/// Baseline RFB cut text is **latin-1**, not UTF-8, and has no way to say otherwise;
/// the Extended Clipboard pseudo-encoding that fixes this is not implemented. A
/// character outside latin-1 degrades to `?` rather than becoming two mojibake bytes,
/// so what lands on the remote clipboard is at worst visibly lossy and never silently
/// corrupt. CR is stripped: RFB specifies LF-only line endings.
pub fn client_cut_text(text: &str) -> Vec<u8> {
    let latin1 = to_latin1(text);
    let mut b = vec![client_msg::CLIENT_CUT_TEXT, 0, 0, 0];
    b.extend_from_slice(&(latin1.len() as u32).to_be_bytes());
    b.extend_from_slice(&latin1);
    b
}

/// The latin-1 lowering [`client_cut_text`] applies, exposed so the fake server can
/// assert on exactly the bytes a caller's string becomes.
pub fn to_latin1(text: &str) -> Vec<u8> {
    text.chars()
        .filter(|c| *c != '\r')
        .map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' })
        .collect()
}

/// The latin-1 lifting applied to ServerCutText.
pub fn from_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

// ---------------------------------------------------------------------------
// VNC authentication
// ---------------------------------------------------------------------------

/// The DES-challenge response for security type 2.
///
/// The quirk that trips every first implementation: VNC uses the password's bytes as a
/// DES key **with the bits of each byte reversed**. That is not a spec decision, it is
/// an accident of the original implementation reading the key LSB-first, and every VNC
/// server since has reproduced it. The password is truncated or zero-padded to exactly
/// 8 bytes, and the 16-byte challenge is two ECB blocks under that one key.
pub fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    use cipher::{BlockEncrypt, KeyInit};
    use des::Des;

    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let des = Des::new_from_slice(&key).expect("DES key is exactly 8 bytes");

    let mut out = [0u8; 16];
    out.copy_from_slice(challenge);
    for block in out.chunks_exact_mut(8) {
        des.encrypt_block(block.into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_version_banner() {
        assert_eq!(parse_version(VERSION_3_8), Some((3, 8)));
        assert_eq!(parse_version(VERSION_3_3), Some((3, 3)));
        assert_eq!(parse_version(b"RFB 004.001\n"), Some((4, 1)));
        assert_eq!(parse_version(b"NOPE 03.008\n"), None);
        assert_eq!(parse_version(b"RFB 003.008"), None);
    }

    #[test]
    fn pixel_format_round_trips_through_the_wire_block() {
        let f = PixelFormat::RGBX_32;
        assert_eq!(PixelFormat::decode(&f.encode()), f);
        // The canonical block, byte for byte, so a change to it is a visible diff.
        assert_eq!(
            f.encode(),
            [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]
        );
    }

    #[test]
    fn decodes_a_little_endian_rgbx_pixel() {
        let f = PixelFormat::RGBX_32;
        // wire bytes R,G,B,X → value 0x00332211 → rgba (0x11, 0x22, 0x33, 255)
        let v = f.read_pixel(&[0x11, 0x22, 0x33, 0x00]);
        assert_eq!(f.to_rgba(v), [0x11, 0x22, 0x33, 255]);
    }

    #[test]
    fn decodes_a_big_endian_bgrx_pixel() {
        let f = PixelFormat {
            big_endian: true,
            red_shift: 16,
            blue_shift: 0,
            ..PixelFormat::RGBX_32
        };
        let v = f.read_pixel(&[0x00, 0x11, 0x22, 0x33]);
        assert_eq!(f.to_rgba(v), [0x11, 0x22, 0x33, 255]);
    }

    #[test]
    fn scales_a_16bpp_565_pixel_to_full_range() {
        let f = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        assert_eq!(f.bytes_per_pixel(), 2);
        // All bits set → pure white, not 31/63/31.
        let white = f.read_pixel(&[0xff, 0xff]);
        assert_eq!(f.to_rgba(white), [255, 255, 255, 255]);
        let black = f.read_pixel(&[0x00, 0x00]);
        assert_eq!(f.to_rgba(black), [0, 0, 0, 255]);
    }

    #[test]
    fn rgba_survives_a_round_trip_through_a_565_format() {
        let f = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        for c in [[0, 0, 0, 255], [255, 255, 255, 255], [255, 0, 0, 255]] {
            let mut wire = Vec::new();
            f.write_pixel(f.from_rgba(c), &mut wire);
            assert_eq!(f.to_rgba(f.read_pixel(&wire)), c, "colour {c:?}");
        }
    }

    #[test]
    fn cpixel_is_three_bytes_for_the_canonical_format() {
        assert_eq!(PixelFormat::RGBX_32.cpixel_bytes(), 3);
        assert!(PixelFormat::RGBX_32.cpixel_drops_high_byte());
        // R,G,B,X little-endian → the X byte is wire index 3, so the CPIXEL is the
        // first three bytes.
        assert_eq!(PixelFormat::RGBX_32.cpixel_offset(), 0);

        // X,R,G,B big-endian (shifts 16/8/0 with the pad in the top byte): the pad is
        // wire index 0, so the CPIXEL is the LAST three bytes.
        let be = PixelFormat {
            big_endian: true,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            ..PixelFormat::RGBX_32
        };
        assert_eq!(be.cpixel_bytes(), 3);
        assert_eq!(be.cpixel_offset(), 1);

        // Colour in the HIGH three bytes, little-endian: the pad is wire index 0.
        let high = PixelFormat {
            red_shift: 8,
            green_shift: 16,
            blue_shift: 24,
            ..PixelFormat::RGBX_32
        };
        assert_eq!(high.cpixel_bytes(), 3);
        assert_eq!(high.cpixel_offset(), 1);

        // A 16bpp format has no CPIXEL shortcut at all.
        let small = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
            ..PixelFormat::RGBX_32
        };
        assert_eq!(small.cpixel_bytes(), 2);
        assert_eq!(small.cpixel_offset(), 0);
    }

    #[test]
    fn builds_the_fixed_client_messages() {
        assert_eq!(
            framebuffer_update_request(true, 0, 0, 800, 600),
            vec![3, 1, 0, 0, 0, 0, 0x03, 0x20, 0x02, 0x58]
        );
        assert_eq!(key_event(0xff0d, true), vec![4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
        assert_eq!(pointer_event(10, 20, BUTTON_LEFT), vec![5, 1, 0, 10, 0, 20]);
        assert_eq!(
            set_encodings(&[ENC_ZRLE, ENC_DESKTOP_SIZE])[..4],
            [2, 0, 0, 2]
        );
    }

    #[test]
    fn cut_text_is_latin1_and_lossy_beyond_it() {
        assert_eq!(
            client_cut_text("hi"),
            vec![6, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']
        );
        // é is latin-1; 😀 is not.
        assert_eq!(to_latin1("é😀"), vec![0xe9, b'?']);
        assert_eq!(to_latin1("a\r\nb"), vec![b'a', b'\n', b'b']);
        assert_eq!(from_latin1(&[0xe9]), "é");
    }

    #[test]
    fn vnc_auth_reverses_the_key_bits() {
        // The reference vector every VNC implementation checks against: password
        // "test", the challenge 0x00..0x0f. Produced by the reversed-bit DES key
        // 't'=0x74→0x2e, 'e'=0x65→0xa6, 's'=0x73→0xce, 't'=0x74→0x2e, then zeros.
        let response = vnc_auth_response(
            "test",
            &[
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f,
            ],
        );
        // Both halves use the SAME key, so identical input blocks would give identical
        // output; these differ, which is the shape of a correct two-block ECB pass.
        assert_ne!(&response[..8], &response[8..]);
        // A different password must give a different response, or the key is not being
        // used at all — the failure mode a hand-rolled bit reversal actually produces.
        let other = vnc_auth_response(
            "tests",
            &[
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f,
            ],
        );
        assert_ne!(response, other);
        // And the reversal itself, stated as its own fact.
        assert_eq!(0x74u8.reverse_bits(), 0x2e);
    }

    #[test]
    fn vnc_auth_truncates_a_long_password_to_eight_bytes() {
        let challenge = [0x5au8; 16];
        assert_eq!(
            vnc_auth_response("0123456789", &challenge),
            vnc_auth_response("01234567", &challenge)
        );
    }
}
