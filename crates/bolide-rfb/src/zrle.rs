//! ZRLE — the compact encoding bolide advertises first, and the only tricky decoder here.
//!
//! **This module doc is the specification both halves read.** `bolide-rfb` decodes ZRLE;
//! `bolide-testkit` encodes it so the decoder has something offline to chew on. Two
//! inverse implementations of a format written down in one place; if the format is ever
//! restated in the testkit instead of referenced from here, that is the drift to fix.
//!
//! # Why ZRLE and not Hextile
//!
//! The choice was between the two compact encodings every mainstream server implements.
//! Hextile is simpler — 16×16 tiles, five subencoding bits, no shared state — but it
//! does not compress: its tiles are raw or crudely run-coded and nothing is deflated, so
//! a photographic desktop lands within a small factor of Raw. ZRLE runs palette and RLE
//! coding *and then* zlib over the result, which is what makes "screenshot the whole
//! desktop" cheap enough to do on every model turn — the single operation bolide exists
//! to serve. TigerVNC, x11vnc, RealVNC and Apple's Screen Sharing all speak it. The cost
//! is the one thing this module has to get right: **one zlib stream per connection**,
//! whose state carries across rects and across updates. Raw stays advertised as the
//! floor under it — RFB requires every server to speak Raw — so a server that ignores
//! ZRLE costs bandwidth and nothing else.
//!
//! # The format (RFC 6143 §7.7.6)
//!
//! A ZRLE rect's payload is `u32 length` followed by `length` bytes. Those bytes are
//! **a slice of the connection's single zlib stream**, not a self-contained one: feed
//! them to the same inflater every time and never reset it. (The `u32` is on the wire
//! because a compressed payload has no other way to say where it ends.)
//!
//! The inflated output is tiles of [`crate::proto::ZRLE_TILE`] (64) pixels a side,
//! left-to-right then top-to-bottom, clipped at the rect's right and bottom edges. A
//! clipped tile carries only the pixels it actually has.
//!
//! Pixels inside tiles are **CPIXELs** — see [`crate::proto::PixelFormat::cpixel_bytes`]
//! and [`crate::proto::PixelFormat::cpixel_offset`]. For bolide's negotiated format
//! that is 3 bytes: R, G, B.
//!
//! Each tile opens with one subencoding byte:
//!
//! | byte | meaning |
//! |---|---|
//! | `0` | **Raw**: `w * h` CPIXELs, row-major. |
//! | `1` | **Solid**: one CPIXEL, fills the tile. |
//! | `2..=16` | **Packed palette**: `n = byte` CPIXELs of palette, then packed indices. |
//! | `17..=127` | invalid — `Error::Protocol`. |
//! | `128` | **Plain RLE**: `(CPIXEL, runlength)` pairs until the tile is full. |
//! | `129` | invalid — `Error::Protocol`. |
//! | `130..=255` | **Palette RLE**: `n = byte - 128` CPIXELs of palette, then runs. |
//!
//! **Packed indices** use the narrowest field that fits the palette: `n == 2` → 1 bit,
//! `n <= 4` → 2 bits, `n <= 16` → 4 bits. Fields are packed most-significant-bit first
//! and **every row restarts on a byte boundary** — the padding at the end of a row is
//! the detail that silently shears an image when it is forgotten.
//!
//! **Run lengths** are written as: zero or more `0xff` bytes, then one byte `< 0xff`.
//! The length is `sum_of_all_bytes + 1`. (So a byte `0x00` means a run of 1, and
//! `0xff 0x02` means 258.)
//!
//! **Palette RLE entries** are one byte: if bit 7 is set the low 7 bits are a palette
//! index and a run length follows; otherwise the whole byte is a palette index for a
//! run of exactly 1.
//!
//! A tile whose runs overflow its pixel count is `Error::Protocol`, not a panic and not
//! a silent truncation — a malformed server must not be able to corrupt the framebuffer
//! of the machine an agent is driving.
//!
//! # What the decoder must be tested on
//!
//! Every subencoding above, plus: a clipped right/bottom tile, a packed-palette row
//! whose width is not a multiple of the field-per-byte count (the padding case), a run
//! that spans a row boundary, a run length over 255, and **two rects in sequence whose
//! second only decodes if the zlib stream was not reset**. That last one is the bug
//! this design invites.

use crate::proto::{PixelFormat, ZRLE_TILE};
use crate::{Error, Result};

/// Decoder state for one connection's ZRLE stream.
///
/// Holds the `flate2::Decompress` whose state carries across every rect for the life of
/// the connection. There is deliberately no `reset`: the only correct number of resets
/// is zero, so the type does not offer one.
pub(crate) struct ZrleDecoder {
    pub(crate) inflate: flate2::Decompress,
}

impl ZrleDecoder {
    /// A fresh stream, to be created once per connection and never reset.
    pub(crate) fn new() -> ZrleDecoder {
        ZrleDecoder {
            inflate: flate2::Decompress::new(true),
        }
    }

    /// Decode one ZRLE rect's compressed payload into `w * h * 4` bytes of RGBA.
    ///
    /// `payload` is a *slice* of the connection's zlib stream, so this method is the
    /// only way the inflater is ever fed and it is never recreated.
    pub(crate) fn decode_rect(
        &mut self,
        payload: &[u8],
        fmt: &PixelFormat,
        w: u16,
        h: u16,
    ) -> Result<Vec<u8>> {
        let plain = self.inflate_slice(payload)?;
        decode_tiles(&plain, fmt, w, h)
    }

    /// Push one slice of the stream through the shared inflater and collect everything
    /// it yields. A server flushes (`Z_SYNC_FLUSH`) at the end of each rect, so the
    /// slice always inflates to a whole number of tiles — but only if this inflater is
    /// the same one the previous rect used.
    fn inflate_slice(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(payload.len() * 4);
        let mut buf = vec![0u8; 64 * 1024];
        let mut consumed = 0usize;
        loop {
            let before_in = self.inflate.total_in();
            let before_out = self.inflate.total_out();
            let status = self
                .inflate
                .decompress(
                    &payload[consumed..],
                    &mut buf,
                    flate2::FlushDecompress::None,
                )
                .map_err(|e| Error::Protocol(format!("ZRLE: zlib stream is corrupt: {e}")))?;
            let read = (self.inflate.total_in() - before_in) as usize;
            let produced = (self.inflate.total_out() - before_out) as usize;
            consumed += read;
            out.extend_from_slice(&buf[..produced]);
            if status == flate2::Status::StreamEnd {
                break;
            }
            // Nothing moved in either direction: the inflater wants more input than
            // this rect carries, which is all we are ever going to give it now.
            if read == 0 && produced == 0 {
                break;
            }
        }
        Ok(out)
    }
}

/// Split the inflated bytes into 64×64 tiles and decode each one.
fn decode_tiles(data: &[u8], fmt: &PixelFormat, w: u16, h: u16) -> Result<Vec<u8>> {
    let (rw, rh) = (w as usize, h as usize);
    let mut out = vec![0u8; rw * rh * 4];
    let mut cur = Cursor { data, pos: 0 };
    let tile = ZRLE_TILE as usize;
    let mut ty = 0usize;
    while ty < rh {
        let th = tile.min(rh - ty);
        let mut tx = 0usize;
        while tx < rw {
            let tw = tile.min(rw - tx);
            let pixels = decode_tile(&mut cur, fmt, tw, th)?;
            for row in 0..th {
                let dst = ((ty + row) * rw + tx) * 4;
                let src = row * tw * 4;
                out[dst..dst + tw * 4].copy_from_slice(&pixels[src..src + tw * 4]);
            }
            tx += tile;
        }
        ty += tile;
    }
    Ok(out)
}

/// One tile, as `tw * th * 4` bytes of RGBA.
fn decode_tile(cur: &mut Cursor<'_>, fmt: &PixelFormat, tw: usize, th: usize) -> Result<Vec<u8>> {
    let count = tw * th;
    let sub = cur.u8()?;
    let mut px: Vec<u8> = Vec::with_capacity(count * 4);
    match sub {
        0 => {
            for _ in 0..count {
                px.extend_from_slice(&read_cpixel(cur, fmt)?);
            }
        }
        1 => {
            let colour = read_cpixel(cur, fmt)?;
            for _ in 0..count {
                px.extend_from_slice(&colour);
            }
        }
        2..=16 => {
            let palette = read_palette(cur, fmt, sub as usize)?;
            let bits = packed_bits(palette.len());
            for _ in 0..th {
                // Every row restarts on a byte boundary; the tail bits of the last
                // byte in a row are padding, and reading through them shears the tile.
                let row_bytes = (tw * bits).div_ceil(8);
                let raw = cur.take(row_bytes)?;
                for col in 0..tw {
                    let bit = col * bits;
                    let shift = 8 - bits - (bit % 8);
                    let mask = ((1u16 << bits) - 1) as u8;
                    let idx = ((raw[bit / 8] >> shift) & mask) as usize;
                    let entry = palette.get(idx).ok_or_else(|| {
                        Error::Protocol(format!(
                            "ZRLE: packed index {idx} is outside a palette of {}",
                            palette.len()
                        ))
                    })?;
                    px.extend_from_slice(entry);
                }
            }
        }
        128 => {
            while px.len() < count * 4 {
                let colour = read_cpixel(cur, fmt)?;
                let run = read_run_length(cur)?;
                push_run(&mut px, &colour, run, count)?;
            }
        }
        130..=255 => {
            let palette = read_palette(cur, fmt, sub as usize - 128)?;
            while px.len() < count * 4 {
                let b = cur.u8()?;
                let (idx, run) = if b & 0x80 != 0 {
                    ((b & 0x7f) as usize, read_run_length(cur)?)
                } else {
                    (b as usize, 1)
                };
                let entry = palette.get(idx).ok_or_else(|| {
                    Error::Protocol(format!(
                        "ZRLE: RLE palette index {idx} is outside a palette of {}",
                        palette.len()
                    ))
                })?;
                push_run(&mut px, entry, run, count)?;
            }
        }
        other => {
            return Err(Error::Protocol(format!(
                "ZRLE: subencoding {other} is not a thing"
            )))
        }
    }
    if px.len() != count * 4 {
        return Err(Error::Protocol(format!(
            "ZRLE: tile carried {} of {count} pixels",
            px.len() / 4
        )));
    }
    Ok(px)
}

/// Append `run` copies of `colour`, refusing to overflow the tile.
fn push_run(px: &mut Vec<u8>, colour: &[u8; 4], run: usize, count: usize) -> Result<()> {
    if px.len() / 4 + run > count {
        return Err(Error::Protocol(format!(
            "ZRLE: a run of {run} overflows a tile of {count} pixels"
        )));
    }
    for _ in 0..run {
        px.extend_from_slice(colour);
    }
    Ok(())
}

/// The narrowest index field that fits a palette of `n`.
fn packed_bits(n: usize) -> usize {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    }
}

fn read_palette(cur: &mut Cursor<'_>, fmt: &PixelFormat, n: usize) -> Result<Vec<[u8; 4]>> {
    (0..n).map(|_| read_cpixel(cur, fmt)).collect()
}

/// Zero or more `0xff`, then one byte below it; the length is their sum plus one.
fn read_run_length(cur: &mut Cursor<'_>) -> Result<usize> {
    let mut len = 1usize;
    loop {
        let b = cur.u8()?;
        len += b as usize;
        if b != 0xff {
            return Ok(len);
        }
    }
}

/// One CPIXEL → RGBA, through the format's own `to_rgba` so a server that answered
/// SetPixelFormat with something else still decodes.
fn read_cpixel(cur: &mut Cursor<'_>, fmt: &PixelFormat) -> Result<[u8; 4]> {
    let cb = fmt.cpixel_bytes();
    let bytes = cur.take(cb)?;
    let bpp = fmt.bytes_per_pixel();
    if cb == bpp {
        return Ok(fmt.to_rgba(fmt.read_pixel(bytes)));
    }
    // A 3-byte CPIXEL is the full pixel with one byte dropped; put it back where the
    // format says it belongs, zero-filled, before reading the value.
    let mut wire = vec![0u8; bpp];
    let off = fmt.cpixel_offset();
    wire[off..off + cb].copy_from_slice(bytes);
    Ok(fmt.to_rgba(fmt.read_pixel(&wire)))
}

/// A bounds-checked read head over the inflated tile bytes. Running off the end is a
/// protocol error, never a panic: the bytes come from a machine we do not control.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            Error::Protocol("ZRLE: absurd read length in the tile stream".to_string())
        })?;
        if end > self.data.len() {
            return Err(Error::Protocol(format!(
                "ZRLE: tile stream ended {} bytes early",
                end - self.data.len()
            )));
        }
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    // ---- a hand-rolled encoder, written from the module docs above -----------------
    //
    // Deliberately *not* the testkit's encoder: two independent readings of the same
    // paragraphs catch a misreading that a shared helper would hide.

    fn fmt() -> PixelFormat {
        PixelFormat::RGBX_32
    }

    fn cpixel(f: &PixelFormat, rgba: [u8; 4]) -> Vec<u8> {
        let mut wire = Vec::new();
        f.write_pixel(f.from_rgba(rgba), &mut wire);
        let cb = f.cpixel_bytes();
        if cb == wire.len() {
            wire
        } else {
            let off = f.cpixel_offset();
            wire[off..off + cb].to_vec()
        }
    }

    fn run_length(n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = n - 1;
        while rest >= 255 {
            out.push(0xff);
            rest -= 255;
        }
        out.push(rest as u8);
        out
    }

    /// Wrap tile bytes in **one** zlib stream, one output slice per chunk. `Sync` is
    /// what a real server flushes with between rects, and it is what leaves the second
    /// slice dependent on the first.
    fn deflate(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut c = Compress::new(Compression::default(), true);
        chunks
            .iter()
            .map(|chunk| {
                // `compress_vec` only writes into spare capacity; a `Vec::new()` here
                // silently produces nothing.
                let mut out = Vec::with_capacity(chunk.len() * 2 + 1024);
                let before = c.total_in();
                c.compress_vec(chunk, &mut out, FlushCompress::Sync)
                    .unwrap();
                assert_eq!(
                    (c.total_in() - before) as usize,
                    chunk.len(),
                    "test encoder ran out of output capacity"
                );
                out
            })
            .collect()
    }

    fn rgba_at(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * w + x) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    fn decode_one(tiles: &[u8], w: u16, h: u16) -> Result<Vec<u8>> {
        let mut d = ZrleDecoder::new();
        let payload = deflate(&[tiles]).remove(0);
        d.decode_rect(&payload, &fmt(), w, h)
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[test]
    fn raw_subencoding_zero() {
        let f = fmt();
        let mut tile = vec![0u8];
        for c in [RED, GREEN, BLUE, WHITE] {
            tile.extend_from_slice(&cpixel(&f, c));
        }
        let out = decode_one(&tile, 2, 2).unwrap();
        assert_eq!(rgba_at(&out, 2, 0, 0), RED);
        assert_eq!(rgba_at(&out, 2, 1, 0), GREEN);
        assert_eq!(rgba_at(&out, 2, 0, 1), BLUE);
        assert_eq!(rgba_at(&out, 2, 1, 1), WHITE);
    }

    #[test]
    fn solid_subencoding_one_fills_the_tile() {
        let f = fmt();
        let mut tile = vec![1u8];
        tile.extend_from_slice(&cpixel(&f, BLUE));
        let out = decode_one(&tile, 3, 2).unwrap();
        for y in 0..2 {
            for x in 0..3 {
                assert_eq!(rgba_at(&out, 3, x, y), BLUE, "({x},{y})");
            }
        }
    }

    /// A palette of two is one bit per index — the narrowest field, and the one an
    /// implementation that always reaches for four bits gets wrong.
    #[test]
    fn packed_palette_of_two_is_one_bit_per_index() {
        let f = fmt();
        let mut tile = vec![2u8];
        tile.extend_from_slice(&cpixel(&f, RED));
        tile.extend_from_slice(&cpixel(&f, GREEN));
        // 8 wide: 0,1,1,0,0,0,0,1 → 0b0110_0001
        tile.push(0b0110_0001);
        let out = decode_one(&tile, 8, 1).unwrap();
        let want = [RED, GREEN, GREEN, RED, RED, RED, RED, GREEN];
        for (x, c) in want.iter().enumerate() {
            assert_eq!(rgba_at(&out, 8, x, 0), *c, "x={x}");
        }
    }

    #[test]
    fn packed_palette_of_four_is_two_bits_per_index() {
        let f = fmt();
        let mut tile = vec![4u8];
        for c in [RED, GREEN, BLUE, WHITE] {
            tile.extend_from_slice(&cpixel(&f, c));
        }
        // 4 wide: 3,2,1,0 → 0b11_10_01_00
        tile.push(0b1110_0100);
        let out = decode_one(&tile, 4, 1).unwrap();
        for (x, c) in [WHITE, BLUE, GREEN, RED].iter().enumerate() {
            assert_eq!(rgba_at(&out, 4, x, 0), *c, "x={x}");
        }
    }

    #[test]
    fn packed_palette_of_sixteen_is_four_bits_per_index() {
        let f = fmt();
        let mut tile = vec![16u8];
        let palette: Vec<[u8; 4]> = (0..16).map(|i| [i * 16, 0, 0, 255]).collect();
        for c in &palette {
            tile.extend_from_slice(&cpixel(&f, *c));
        }
        // 4 wide: 15, 0, 7, 8
        tile.push(0xf0);
        tile.push(0x78);
        let out = decode_one(&tile, 4, 1).unwrap();
        for (x, i) in [15usize, 0, 7, 8].iter().enumerate() {
            assert_eq!(rgba_at(&out, 4, x, 0), palette[*i], "x={x}");
        }
    }

    /// The padding case: 3 pixels at 2 bits each is 6 bits, so the last two bits of
    /// the row byte are padding and the next row starts on a fresh byte. Forgetting
    /// that shears the image by one pixel per row.
    #[test]
    fn a_packed_palette_row_restarts_on_a_byte_boundary() {
        let f = fmt();
        let mut tile = vec![3u8];
        for c in [RED, GREEN, BLUE] {
            tile.extend_from_slice(&cpixel(&f, c));
        }
        // row 0: 0,1,2 → 0b00_01_10 + 2 bits of padding (set, to prove they are ignored)
        tile.push(0b0001_1011);
        // row 1: 2,1,0 → 0b10_01_00 + padding
        tile.push(0b1001_0011);
        let out = decode_one(&tile, 3, 2).unwrap();
        for (x, c) in [RED, GREEN, BLUE].iter().enumerate() {
            assert_eq!(rgba_at(&out, 3, x, 0), *c, "row 0 x={x}");
        }
        for (x, c) in [BLUE, GREEN, RED].iter().enumerate() {
            assert_eq!(rgba_at(&out, 3, x, 1), *c, "row 1 x={x}");
        }
    }

    #[test]
    fn plain_rle_subencoding_128() {
        let f = fmt();
        let mut tile = vec![128u8];
        tile.extend_from_slice(&cpixel(&f, RED));
        tile.extend_from_slice(&run_length(3));
        tile.extend_from_slice(&cpixel(&f, BLUE));
        tile.extend_from_slice(&run_length(1));
        let out = decode_one(&tile, 4, 1).unwrap();
        for x in 0..3 {
            assert_eq!(rgba_at(&out, 4, x, 0), RED, "x={x}");
        }
        assert_eq!(rgba_at(&out, 4, 3, 0), BLUE);
    }

    /// RLE runs know nothing about rows: a run that starts on one row and ends on the
    /// next is ordinary, and a decoder that re-syncs per row loses it.
    #[test]
    fn an_rle_run_spans_a_row_boundary() {
        let f = fmt();
        let mut tile = vec![128u8];
        tile.extend_from_slice(&cpixel(&f, RED));
        tile.extend_from_slice(&run_length(5)); // 3 wide → row 0 plus two of row 1
        tile.extend_from_slice(&cpixel(&f, GREEN));
        tile.extend_from_slice(&run_length(1));
        let out = decode_one(&tile, 3, 2).unwrap();
        assert_eq!(rgba_at(&out, 3, 2, 0), RED);
        assert_eq!(rgba_at(&out, 3, 0, 1), RED);
        assert_eq!(rgba_at(&out, 3, 1, 1), RED);
        assert_eq!(rgba_at(&out, 3, 2, 1), GREEN);
    }

    /// A run longer than 255 is `0xff` bytes plus a remainder; reading only one byte
    /// truncates the run and shifts everything after it.
    #[test]
    fn a_run_longer_than_255_needs_every_ff_byte() {
        assert_eq!(run_length(258), vec![0xff, 0x02]);
        assert_eq!(run_length(1), vec![0x00]);
        assert_eq!(run_length(256), vec![0xff, 0x00]);
        // One whole 64x64 tile — 4096 pixels, so a 300-long run fits inside it and
        // spans four row boundaries on the way.
        let f = fmt();
        let mut tile = vec![128u8];
        tile.extend_from_slice(&cpixel(&f, RED));
        tile.extend_from_slice(&run_length(300));
        tile.extend_from_slice(&cpixel(&f, GREEN));
        tile.extend_from_slice(&run_length(20));
        tile.extend_from_slice(&cpixel(&f, BLUE));
        tile.extend_from_slice(&run_length(4096 - 320));
        let out = decode_one(&tile, 64, 64).unwrap();
        // Pixel 299 is (43, 4); pixel 300 is (44, 4).
        assert_eq!(rgba_at(&out, 64, 43, 4), RED);
        assert_eq!(rgba_at(&out, 64, 44, 4), GREEN);
        assert_eq!(rgba_at(&out, 64, 63, 4), GREEN);
        assert_eq!(rgba_at(&out, 64, 0, 5), BLUE);
        assert_eq!(rgba_at(&out, 64, 63, 63), BLUE);
    }

    #[test]
    fn palette_rle_subencoding_130_and_up() {
        let f = fmt();
        let mut tile = vec![130u8]; // 130 - 128 = 2 palette entries
        tile.extend_from_slice(&cpixel(&f, RED));
        tile.extend_from_slice(&cpixel(&f, GREEN));
        // index 0, run of 3 (bit 7 set)
        tile.push(0x80);
        tile.extend_from_slice(&run_length(3));
        // index 1, bare byte → a run of exactly one
        tile.push(0x01);
        let out = decode_one(&tile, 4, 1).unwrap();
        for x in 0..3 {
            assert_eq!(rgba_at(&out, 4, x, 0), RED, "x={x}");
        }
        assert_eq!(rgba_at(&out, 4, 3, 0), GREEN);
    }

    /// The right and bottom edges are clipped, and a clipped tile carries only the
    /// pixels it has — a decoder that always reads 64×64 desynchronises on tile two.
    #[test]
    fn the_right_and_bottom_tiles_are_clipped() {
        let f = fmt();
        // 70x70: four tiles — 64x64, 6x64, 64x6, 6x6.
        let mut tiles = Vec::new();
        for c in [RED, GREEN, BLUE, WHITE] {
            tiles.push(1u8);
            tiles.extend_from_slice(&cpixel(&f, c));
        }
        let out = decode_one(&tiles, 70, 70).unwrap();
        assert_eq!(rgba_at(&out, 70, 0, 0), RED);
        assert_eq!(rgba_at(&out, 70, 63, 63), RED);
        assert_eq!(rgba_at(&out, 70, 64, 0), GREEN);
        assert_eq!(rgba_at(&out, 70, 69, 63), GREEN);
        assert_eq!(rgba_at(&out, 70, 0, 64), BLUE);
        assert_eq!(rgba_at(&out, 70, 63, 69), BLUE);
        assert_eq!(rgba_at(&out, 70, 64, 64), WHITE);
        assert_eq!(rgba_at(&out, 70, 69, 69), WHITE);
    }

    /// **The bug this design invites.** A server deflates every rect into one stream
    /// and flushes between them; the second rect's bytes are meaningless to a fresh
    /// inflater, because they lean on the first rect's window.
    #[test]
    fn the_zlib_stream_is_not_reset_between_rects() {
        let f = fmt();
        let mut first = vec![1u8];
        first.extend_from_slice(&cpixel(&f, RED));
        let mut second = vec![1u8];
        second.extend_from_slice(&cpixel(&f, GREEN));

        let parts = deflate(&[&first, &second]);
        let mut d = ZrleDecoder::new();
        let a = d.decode_rect(&parts[0], &f, 2, 2).unwrap();
        assert_eq!(rgba_at(&a, 2, 0, 0), RED);
        let b = d.decode_rect(&parts[1], &f, 2, 2).unwrap();
        assert_eq!(rgba_at(&b, 2, 1, 1), GREEN);

        // And the proof that it is really one stream: a fresh decoder handed only the
        // second slice cannot make sense of it.
        let mut fresh = ZrleDecoder::new();
        let lone = fresh.decode_rect(&parts[1], &f, 2, 2);
        assert!(
            lone.is_err() || lone.unwrap() != b,
            "rect two decoded standalone, so the test is not exercising stream continuity"
        );
    }

    #[test]
    fn an_invalid_subencoding_is_a_protocol_error_not_a_panic() {
        for bad in [17u8, 100, 127, 129] {
            let err = decode_one(&[bad], 2, 2).unwrap_err();
            assert!(
                matches!(err, Error::Protocol(_)),
                "subencoding {bad} gave {err:?}"
            );
        }
    }

    #[test]
    fn a_tile_whose_runs_overflow_its_pixel_count_is_a_protocol_error() {
        let f = fmt();
        let mut tile = vec![128u8];
        tile.extend_from_slice(&cpixel(&f, RED));
        tile.extend_from_slice(&run_length(9)); // the tile only holds 4
        let err = decode_one(&tile, 2, 2).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        assert!(format!("{err}").contains("overflows"), "{err}");
    }

    #[test]
    fn a_truncated_tile_stream_is_a_protocol_error() {
        let f = fmt();
        let mut tile = vec![0u8];
        tile.extend_from_slice(&cpixel(&f, RED)); // one pixel, four promised
        let err = decode_one(&tile, 2, 2).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    }

    #[test]
    fn a_palette_index_outside_the_palette_is_a_protocol_error() {
        let f = fmt();
        let mut tile = vec![130u8];
        tile.extend_from_slice(&cpixel(&f, RED));
        tile.extend_from_slice(&cpixel(&f, GREEN));
        tile.push(0x05); // index 5 into a palette of 2
        let err = decode_one(&tile, 2, 1).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
    }

    /// CPIXELs are not always three bytes. A 16bpp server has no compressible byte to
    /// drop, so the decoder has to read two and scale each channel from its own max.
    #[test]
    fn cpixels_follow_the_negotiated_pixel_format() {
        let f565 = PixelFormat {
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
        assert_eq!(f565.cpixel_bytes(), 2);
        let mut tile = vec![1u8];
        tile.extend_from_slice(&cpixel(&f565, WHITE));
        let mut d = ZrleDecoder::new();
        let payload = deflate(&[&tile]).remove(0);
        let out = d.decode_rect(&payload, &f565, 2, 2).unwrap();
        assert_eq!(rgba_at(&out, 2, 1, 1), WHITE);
    }

    /// A big-endian 32bpp format still has a 3-byte CPIXEL, but the dropped byte is at
    /// the other end — that is what `cpixel_offset` is for.
    fn big_endian_xrgb() -> PixelFormat {
        PixelFormat {
            big_endian: true,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            ..PixelFormat::RGBX_32
        }
    }

    #[test]
    fn a_big_endian_cpixel_drops_the_other_byte() {
        let f = big_endian_xrgb();
        assert_eq!(f.cpixel_bytes(), 3);
        assert_eq!(f.cpixel_offset(), 1);
        let mut tile = vec![0u8];
        for c in [RED, GREEN, BLUE, WHITE] {
            tile.extend_from_slice(&cpixel(&f, c));
        }
        let mut d = ZrleDecoder::new();
        let payload = deflate(&[&tile]).remove(0);
        let out = d.decode_rect(&payload, &f, 2, 2).unwrap();
        assert_eq!(rgba_at(&out, 2, 0, 0), RED);
        assert_eq!(rgba_at(&out, 2, 1, 1), WHITE);
    }

    #[test]
    fn packed_bits_picks_the_narrowest_field() {
        assert_eq!(packed_bits(2), 1);
        assert_eq!(packed_bits(3), 2);
        assert_eq!(packed_bits(4), 2);
        assert_eq!(packed_bits(5), 4);
        assert_eq!(packed_bits(16), 4);
    }
}
