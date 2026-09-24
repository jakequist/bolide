//! The server half of the encodings: [`Screen`] → rect payloads.
//!
//! **Read `bolide_rfb::zrle`'s module docs for the ZRLE format; it is not restated
//! here.** This is the inverse of the decoder over there, and there is exactly one
//! written description of the format for both — subencoding bytes, packed-index widths
//! and end-of-row padding, run-length coding and the palette-RLE entry byte all live in
//! that doc. What lives here is only the *choices* a server makes: which subencoding to
//! spend on a tile, and the single zlib stream those tiles travel in.
//!
//! - `raw_rect` — pixels through [`PixelFormat::write_pixel`], row-major.
//! - `copy_rect` — the two `u16` source coordinates, nothing else.
//! - [`ZrleWriter`] — **one zlib stream per connection**, a [`flate2::Compress`] with
//!   [`FlushCompress::Sync`] after each rect so the client can inflate what it has.
//!   Resetting it between rects is the bug the decoder's test suite is built to catch,
//!   so the writer must not, and one is built per connection.
//!
//! The writer takes a [`ZrleTiling`] so a test can aim at one subencoding: a decoder is
//! only tested for the subencodings something actually produced, so the writer has to be
//! able to produce all of them on demand.

use bolide_rfb::proto::{PixelFormat, ZRLE_TILE};
use flate2::{Compress, FlushCompress};

use crate::{Screen, ZrleTiling};

const BLACK: [u8; 4] = [0, 0, 0, 255];

/// Raw rect payload for a region of `screen`: `w * h` pixels, row-major, each written
/// through the *client's* pixel format rather than assumed to be RGBA.
pub(crate) fn raw_rect(
    screen: &Screen,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    fmt: &PixelFormat,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(w as usize * h as usize * fmt.bytes_per_pixel());
    for row in 0..h {
        for col in 0..w {
            let rgba = screen
                .pixel(x.saturating_add(col), y.saturating_add(row))
                .unwrap_or(BLACK);
            fmt.write_pixel(fmt.from_rgba(rgba), &mut out);
        }
    }
    out
}

/// CopyRect payload: the source x/y the rect is copied *from*, two big-endian `u16`.
pub(crate) fn copy_rect(src_x: u16, src_y: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.extend_from_slice(&src_x.to_be_bytes());
    out.extend_from_slice(&src_y.to_be_bytes());
    out
}

/// One connection's ZRLE stream. See the module docs.
pub(crate) struct ZrleWriter {
    pub(crate) deflate: Compress,
    tiling: ZrleTiling,
}

impl ZrleWriter {
    /// A fresh stream that spends `tiling` on every tile it can.
    ///
    /// Call once per connection — never between rects. There is deliberately no other
    /// constructor: a `new()` sitting beside this one is a `new()` somebody reaches for
    /// mid-connection.
    pub(crate) fn with_tiling(tiling: ZrleTiling) -> ZrleWriter {
        ZrleWriter {
            deflate: Compress::new(flate2::Compression::default(), true),
            tiling,
        }
    }

    /// A whole ZRLE rect payload: the `u32` byte count followed by that many bytes of
    /// **this connection's** zlib stream.
    pub(crate) fn rect(
        &mut self,
        screen: &Screen,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        fmt: &PixelFormat,
    ) -> Vec<u8> {
        let tiles = self.tiles(screen, x, y, w, h, fmt);
        let compressed = self.deflate(&tiles);
        let mut out = Vec::with_capacity(compressed.len() + 4);
        out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
        out.extend_from_slice(&compressed);
        out
    }

    /// The *uncompressed* tile stream for a rect: 64×64 tiles left-to-right then
    /// top-to-bottom, clipped at the right and bottom edges.
    fn tiles(&self, screen: &Screen, x: u16, y: u16, w: u16, h: u16, fmt: &PixelFormat) -> Vec<u8> {
        let mut out = Vec::new();
        let (x0, y0) = (x as u32, y as u32);
        let (x1, y1) = (x0 + w as u32, y0 + h as u32);
        let mut ty = y0;
        while ty < y1 {
            let th = ZRLE_TILE.min(y1 - ty);
            let mut tx = x0;
            while tx < x1 {
                let tw = ZRLE_TILE.min(x1 - tx);
                let px = tile_pixels(screen, tx as u16, ty as u16, tw as u16, th as u16, fmt);
                encode_tile(&px, tw as usize, self.tiling, fmt, &mut out);
                tx += tw;
            }
            ty += th;
        }
        out
    }

    /// Push `input` through the connection's deflater and take everything it will give
    /// back, ending on a sync flush so the client can inflate the rect it just got.
    ///
    /// The flush is its own pass on purpose: `Z_SYNC_FLUSH` writes an empty stored
    /// block **every time it is called**, so "keep flushing until it stops producing
    /// bytes" never terminates. The termination condition is a call that did not fill
    /// the output buffer.
    fn deflate(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 64);
        let mut consumed = 0usize;
        while consumed < input.len() {
            out.reserve(input.len() + 512);
            let before_in = self.deflate.total_in();
            self.deflate
                .compress_vec(&input[consumed..], &mut out, FlushCompress::None)
                .expect("deflate of an in-memory buffer cannot fail");
            let took = (self.deflate.total_in() - before_in) as usize;
            consumed += took;
            if took == 0 && out.len() < out.capacity() {
                break;
            }
        }
        loop {
            out.reserve(512);
            let spare = out.capacity() - out.len();
            let before_out = self.deflate.total_out();
            self.deflate
                .compress_vec(&[], &mut out, FlushCompress::Sync)
                .expect("deflate of an in-memory buffer cannot fail");
            if ((self.deflate.total_out() - before_out) as usize) < spare {
                return out;
            }
        }
    }
}

/// The pixel values of one tile, in the client's format, row-major.
fn tile_pixels(screen: &Screen, x: u16, y: u16, w: u16, h: u16, fmt: &PixelFormat) -> Vec<u32> {
    let mut px = Vec::with_capacity(w as usize * h as usize);
    for row in 0..h {
        for col in 0..w {
            let rgba = screen
                .pixel(x.saturating_add(col), y.saturating_add(row))
                .unwrap_or(BLACK);
            px.push(fmt.from_rgba(rgba));
        }
    }
    px
}

/// A CPIXEL: the pixel's wire bytes with the padding byte dropped when the format
/// allows it. Which bytes those are is [`PixelFormat::cpixel_offset`]'s to say.
fn write_cpixel(fmt: &PixelFormat, pixel: u32, out: &mut Vec<u8>) {
    let mut wire = Vec::with_capacity(4);
    fmt.write_pixel(pixel, &mut wire);
    let off = fmt.cpixel_offset();
    let n = fmt.cpixel_bytes().min(wire.len() - off);
    out.extend_from_slice(&wire[off..off + n]);
}

/// Colours in order of first appearance, given up on past 128 (nothing above that is
/// expressible as a palette anyway).
fn palette_of(px: &[u32]) -> Vec<u32> {
    let mut palette: Vec<u32> = Vec::new();
    for &p in px {
        if !palette.contains(&p) {
            palette.push(p);
            if palette.len() > 128 {
                break;
            }
        }
    }
    palette
}

/// `(value, length)` runs over the tile read as one linear sequence — runs cross row
/// boundaries, which is why the packed-palette padding rule does not apply here.
fn runs_of(px: &[u32]) -> Vec<(u32, usize)> {
    let mut out: Vec<(u32, usize)> = Vec::new();
    for &p in px {
        match out.last_mut() {
            Some((v, n)) if *v == p => *n += 1,
            _ => out.push((p, 1)),
        }
    }
    out
}

/// Bits per packed index for a palette of `n`, or `None` when the palette is too big
/// for the packed form at all.
fn index_bits(n: usize) -> Option<u32> {
    match n {
        2 => Some(1),
        3..=4 => Some(2),
        5..=16 => Some(4),
        _ => None,
    }
}

fn write_run_length(len: usize, out: &mut Vec<u8>) {
    let mut n = len - 1;
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

/// Encode one tile, spending `tiling` where the tile allows it.
///
/// A forced tiling the tile cannot express — Solid on a tile of two colours, a packed
/// palette on a photograph — falls back to what [`ZrleTiling::Auto`] would have chosen,
/// because a lie about the tile's contents would be a corrupt framebuffer rather than a
/// failed test.
fn encode_tile(px: &[u32], w: usize, tiling: ZrleTiling, fmt: &PixelFormat, out: &mut Vec<u8>) {
    let palette = palette_of(px);
    let runs = runs_of(px);
    let chosen = match tiling {
        ZrleTiling::Raw => ZrleTiling::Raw,
        ZrleTiling::Solid if palette.len() == 1 => ZrleTiling::Solid,
        ZrleTiling::PackedPalette if index_bits(palette.len()).is_some() => {
            ZrleTiling::PackedPalette
        }
        ZrleTiling::PlainRle => ZrleTiling::PlainRle,
        ZrleTiling::PaletteRle if (2..=127).contains(&palette.len()) => ZrleTiling::PaletteRle,
        _ => auto_tiling(palette.len(), runs.len(), px.len()),
    };

    match chosen {
        ZrleTiling::Auto | ZrleTiling::Raw => {
            out.push(0);
            for &p in px {
                write_cpixel(fmt, p, out);
            }
        }
        ZrleTiling::Solid => {
            out.push(1);
            write_cpixel(fmt, palette[0], out);
        }
        ZrleTiling::PackedPalette => {
            let bits = index_bits(palette.len()).expect("checked above");
            out.push(palette.len() as u8);
            for &p in &palette {
                write_cpixel(fmt, p, out);
            }
            for row in px.chunks(w) {
                let mut acc: u8 = 0;
                let mut used = 0u32;
                for &p in row {
                    let idx = palette.iter().position(|&c| c == p).expect("in palette") as u8;
                    acc = (acc << bits) | idx;
                    used += bits;
                    if used == 8 {
                        out.push(acc);
                        acc = 0;
                        used = 0;
                    }
                }
                // Every row restarts on a byte boundary; the tail is padded with zeroes.
                if used > 0 {
                    out.push(acc << (8 - used));
                }
            }
        }
        ZrleTiling::PlainRle => {
            out.push(128);
            for &(value, len) in &runs {
                write_cpixel(fmt, value, out);
                write_run_length(len, out);
            }
        }
        ZrleTiling::PaletteRle => {
            out.push(128 + palette.len() as u8);
            for &p in &palette {
                write_cpixel(fmt, p, out);
            }
            for &(value, len) in &runs {
                let idx = palette
                    .iter()
                    .position(|&c| c == value)
                    .expect("in palette") as u8;
                if len == 1 {
                    out.push(idx);
                } else {
                    out.push(idx | 0x80);
                    write_run_length(len, out);
                }
            }
        }
    }
}

/// What a server would pick left to itself: the cheapest form the tile fits into.
fn auto_tiling(palette: usize, runs: usize, pixels: usize) -> ZrleTiling {
    if palette == 1 {
        ZrleTiling::Solid
    } else if index_bits(palette).is_some() {
        ZrleTiling::PackedPalette
    } else if palette <= 127 {
        ZrleTiling::PaletteRle
    } else if runs * 2 < pixels {
        ZrleTiling::PlainRle
    } else {
        ZrleTiling::Raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress};

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    fn rgb565() -> PixelFormat {
        PixelFormat {
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
        }
    }

    /// X,R,G,B big-endian: the same colours, the other way round on the wire.
    fn big_endian_xrgb() -> PixelFormat {
        PixelFormat {
            big_endian: true,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            ..PixelFormat::RGBX_32
        }
    }

    fn four_colours() -> Screen {
        let mut s = Screen::solid(2, 2, RED);
        s.fill_rect(1, 0, 1, 1, GREEN);
        s.fill_rect(0, 1, 1, 1, BLUE);
        s.fill_rect(1, 1, 1, 1, WHITE);
        s
    }

    #[test]
    fn raw_rect_writes_rgbx_little_endian_row_major() {
        let s = four_colours();
        assert_eq!(
            raw_rect(&s, 0, 0, 2, 2, &PixelFormat::RGBX_32),
            vec![
                255, 0, 0, 0, // red
                0, 255, 0, 0, // green
                0, 0, 255, 0, // blue
                255, 255, 255, 0, // white
            ]
        );
    }

    #[test]
    fn raw_rect_goes_through_the_clients_format_not_rgba() {
        let s = four_colours();
        // 16bpp 565 little-endian: red is 31<<11 = 0xf800, green 63<<5 = 0x07e0,
        // blue 31 = 0x001f, white all bits.
        assert_eq!(
            raw_rect(&s, 0, 0, 2, 2, &rgb565()),
            vec![0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00, 0xff, 0xff]
        );
        // 32bpp big-endian X,R,G,B: the pad byte leads and the channels reverse.
        assert_eq!(
            raw_rect(&s, 0, 0, 2, 2, &big_endian_xrgb()),
            vec![
                0x00, 0xff, 0x00, 0x00, // red
                0x00, 0x00, 0xff, 0x00, // green
                0x00, 0x00, 0x00, 0xff, // blue
                0x00, 0xff, 0xff, 0xff, // white
            ]
        );
    }

    #[test]
    fn raw_rect_takes_only_the_region_it_was_asked_for() {
        let mut s = Screen::solid(4, 4, RED);
        s.fill_rect(2, 2, 2, 2, GREEN);
        assert_eq!(
            raw_rect(&s, 2, 2, 2, 2, &PixelFormat::RGBX_32),
            vec![0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0]
        );
    }

    #[test]
    fn copy_rect_is_exactly_two_big_endian_u16s() {
        assert_eq!(copy_rect(0x1234, 0x0056), vec![0x12, 0x34, 0x00, 0x56]);
        assert_eq!(copy_rect(0, 0), vec![0, 0, 0, 0]);
    }

    /// Inflate a ZRLE rect payload (a `u32` length and that many stream bytes) through
    /// `inf`, the way a client's single per-connection inflater would.
    fn inflate(inf: &mut Decompress, payload: &[u8]) -> Vec<u8> {
        let len = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        assert_eq!(payload.len(), 4 + len, "the u32 must describe the payload");
        let mut input = &payload[4..];
        let mut out = Vec::new();
        loop {
            let before_in = inf.total_in();
            let before_out = inf.total_out();
            out.reserve(4096);
            inf.decompress_vec(input, &mut out, FlushDecompress::Sync)
                .expect("the stream must still be a stream");
            let consumed = (inf.total_in() - before_in) as usize;
            input = &input[consumed..];
            if input.is_empty() && inf.total_out() == before_out {
                return out;
            }
        }
    }

    fn zrle_tiles(screen: &Screen, tiling: ZrleTiling, fmt: &PixelFormat) -> Vec<u8> {
        let mut w = ZrleWriter::with_tiling(tiling);
        let payload = w.rect(screen, 0, 0, screen.width, screen.height, fmt);
        inflate(&mut Decompress::new(true), &payload)
    }

    #[test]
    fn zrle_solid_tile_is_the_byte_and_one_cpixel() {
        let s = Screen::solid(4, 4, RED);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::Solid, &PixelFormat::RGBX_32),
            vec![1, 255, 0, 0]
        );
    }

    #[test]
    fn zrle_raw_tile_is_the_byte_and_every_cpixel() {
        let s = four_colours();
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::Raw, &PixelFormat::RGBX_32),
            vec![0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]
        );
    }

    #[test]
    fn zrle_raw_tile_cpixels_follow_the_formats_offset() {
        // X,R,G,B big-endian: the CPIXEL is the LAST three wire bytes, so red is
        // ff 00 00 again but only because the pad was dropped from the front.
        let s = four_colours();
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::Raw, &big_endian_xrgb()),
            vec![0, 0xff, 0, 0, 0, 0xff, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]
        );
        // 16bpp has no CPIXEL shortcut: two bytes a pixel, little-endian.
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::Raw, &rgb565()),
            vec![0, 0x00, 0xf8, 0xe0, 0x07, 0x1f, 0x00, 0xff, 0xff]
        );
    }

    #[test]
    fn zrle_packed_palette_uses_one_bit_for_two_colours() {
        // 4×2: R R G G / G R G R. Palette is [R, G] by first appearance.
        let mut s = Screen::solid(4, 2, RED);
        s.fill_rect(2, 0, 2, 1, GREEN);
        s.fill_rect(0, 1, 1, 1, GREEN);
        s.fill_rect(2, 1, 1, 1, GREEN);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::PackedPalette, &PixelFormat::RGBX_32),
            vec![
                2, // palette of two
                255,
                0,
                0, // index 0 = red
                0,
                255,
                0,           // index 1 = green
                0b0011_0000, // row 0: 0,0,1,1 then four bits of padding
                0b1010_0000, // row 1: 1,0,1,0 then four bits of padding
            ]
        );
    }

    #[test]
    fn zrle_packed_palette_uses_two_bits_and_pads_the_row() {
        // 3×1 of three colours: three 2-bit fields, then two bits of padding.
        let mut s = Screen::solid(3, 1, RED);
        s.fill_rect(1, 0, 1, 1, GREEN);
        s.fill_rect(2, 0, 1, 1, BLUE);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::PackedPalette, &PixelFormat::RGBX_32),
            vec![3, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0b00_01_10_00]
        );
    }

    #[test]
    fn zrle_packed_palette_uses_four_bits_for_five_colours() {
        let mut s = Screen::solid(5, 1, RED);
        s.fill_rect(1, 0, 1, 1, GREEN);
        s.fill_rect(2, 0, 1, 1, BLUE);
        s.fill_rect(3, 0, 1, 1, WHITE);
        s.fill_rect(4, 0, 1, 1, [1, 2, 3, 255]);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::PackedPalette, &PixelFormat::RGBX_32),
            vec![
                5, // palette of five
                255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 1, 2, 3, 0x01, // indices 0,1
                0x23, // indices 2,3
                0x40, // index 4, then a nibble of padding
            ]
        );
    }

    #[test]
    fn zrle_plain_rle_writes_a_cpixel_and_a_length_per_run() {
        // 6×1: R R R G G B.
        let mut s = Screen::solid(6, 1, RED);
        s.fill_rect(3, 0, 2, 1, GREEN);
        s.fill_rect(5, 0, 1, 1, BLUE);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::PlainRle, &PixelFormat::RGBX_32),
            vec![
                128, // plain RLE
                255, 0, 0, 0x02, // red, run of 3
                0, 255, 0, 0x01, // green, run of 2
                0, 0, 255, 0x00, // blue, run of 1
            ]
        );
    }

    #[test]
    fn zrle_run_lengths_over_255_carry_in_ff_bytes() {
        // 64×5 is one tile of 320 identical pixels: 320 = 255 + 64 + 1.
        let s = Screen::solid(64, 5, RED);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::PlainRle, &PixelFormat::RGBX_32),
            vec![128, 255, 0, 0, 0xff, 0x40]
        );
    }

    #[test]
    fn zrle_palette_rle_marks_runs_with_bit_seven() {
        let mut s = Screen::solid(6, 1, RED);
        s.fill_rect(3, 0, 2, 1, GREEN);
        s.fill_rect(5, 0, 1, 1, BLUE);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::PaletteRle, &PixelFormat::RGBX_32),
            vec![
                131, // 128 + a palette of three
                255, 0, 0, 0, 255, 0, 0, 0, 255, // the palette
                0x80, 0x02, // index 0, run of 3
                0x81, 0x01, // index 1, run of 2
                0x02, // index 2, a run of one — no length byte
            ]
        );
    }

    #[test]
    fn zrle_runs_cross_row_boundaries() {
        // Two rows of the same colour are ONE run, not one per row.
        let s = Screen::solid(3, 2, RED);
        assert_eq!(
            zrle_tiles(&s, ZrleTiling::PlainRle, &PixelFormat::RGBX_32),
            vec![128, 255, 0, 0, 0x05]
        );
    }

    #[test]
    fn a_clipped_right_tile_carries_only_its_own_pixels() {
        // 70 wide: tile 0 is 64×2, tile 1 is 6×2 and must hold exactly the right edge.
        let mut s = Screen::solid(70, 2, RED);
        s.fill_rect(64, 0, 6, 2, GREEN);
        let tiles = zrle_tiles(&s, ZrleTiling::Raw, &PixelFormat::RGBX_32);
        let first = 1 + 64 * 2 * 3;
        assert_eq!(
            tiles.len(),
            first + 1 + 6 * 2 * 3,
            "two tiles, the second clipped"
        );
        assert_eq!(tiles[first], 0, "the clipped tile is raw too");
        assert_eq!(
            &tiles[first + 1..],
            &[0, 255, 0].repeat(12)[..],
            "only the six green columns, both rows"
        );
    }

    #[test]
    fn a_clipped_bottom_tile_carries_only_its_own_rows() {
        let mut s = Screen::solid(2, 70, RED);
        s.fill_rect(0, 64, 2, 6, GREEN);
        let tiles = zrle_tiles(&s, ZrleTiling::Raw, &PixelFormat::RGBX_32);
        let first = 1 + 2 * 64 * 3;
        assert_eq!(tiles.len(), first + 1 + 2 * 6 * 3);
        assert_eq!(&tiles[first + 1..], &[0, 255, 0].repeat(12)[..]);
    }

    #[test]
    fn the_zlib_stream_is_never_reset_between_rects() {
        // Two rects through ONE writer, inflated through ONE inflater. If the writer
        // had started a fresh deflate for the second rect, the second payload would
        // begin with a zlib header in the middle of the inflater's stream and this
        // would blow up (or decode to nonsense) rather than yielding the second tile.
        let first = Screen::solid(4, 4, RED);
        let second = Screen::solid(4, 4, BLUE);
        let mut w = ZrleWriter::with_tiling(ZrleTiling::Solid);
        let a = w.rect(&first, 0, 0, 4, 4, &PixelFormat::RGBX_32);
        let b = w.rect(&second, 0, 0, 4, 4, &PixelFormat::RGBX_32);

        let mut inf = Decompress::new(true);
        assert_eq!(inflate(&mut inf, &a), vec![1, 255, 0, 0]);
        assert_eq!(inflate(&mut inf, &b), vec![1, 0, 0, 255]);

        // And the proof it is a continuation rather than two independent streams: a
        // fresh inflater cannot read the second payload at all.
        let mut fresh = Decompress::new(true);
        let mut out = Vec::with_capacity(64);
        assert!(
            fresh
                .decompress_vec(&b[4..], &mut out, FlushDecompress::Sync)
                .is_err(),
            "the second rect is a slice of the first's stream, not a stream"
        );
    }

    #[test]
    fn auto_picks_the_cheapest_form_the_tile_fits() {
        let solid = Screen::solid(4, 4, RED);
        assert_eq!(
            zrle_tiles(&solid, ZrleTiling::Auto, &PixelFormat::RGBX_32),
            vec![1, 255, 0, 0]
        );
        // Two colours → a packed palette, not raw.
        let mut two = Screen::solid(4, 1, RED);
        two.fill_rect(2, 0, 2, 1, GREEN);
        assert_eq!(
            zrle_tiles(&two, ZrleTiling::Auto, &PixelFormat::RGBX_32),
            vec![2, 255, 0, 0, 0, 255, 0, 0b0011_0000]
        );
    }

    #[test]
    fn a_forced_tiling_the_tile_cannot_express_falls_back() {
        // Solid on a two-colour tile would be a corrupt framebuffer; it degrades to
        // what Auto would have picked instead.
        let mut two = Screen::solid(4, 1, RED);
        two.fill_rect(2, 0, 2, 1, GREEN);
        assert_eq!(
            zrle_tiles(&two, ZrleTiling::Solid, &PixelFormat::RGBX_32),
            vec![2, 255, 0, 0, 0, 255, 0, 0b0011_0000]
        );
    }
}
