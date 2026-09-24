//! Framebuffer → PNG.
//!
//! CONTRACT — `bolide-server`'s to finish. RGBA8888 in, PNG bytes out, no resizing and
//! no colour management: what the desktop sent is what the agent sees.
//!
//! Tests it owes: a known framebuffer encodes to bytes that begin with the PNG
//! signature; decoding the output (the `png` crate reads as well as it writes) returns
//! the same pixels; a zero-sized framebuffer is an error rather than a panic.

use bolide_rfb::Framebuffer;

/// Encode a framebuffer as a PNG.
pub fn encode_png(fb: &Framebuffer) -> std::io::Result<Vec<u8>> {
    let expected = fb.width as usize * fb.height as usize * 4;
    if fb.width == 0 || fb.height == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "a {}x{} framebuffer has no pixels to encode",
                fb.width, fb.height
            ),
        ));
    }
    if fb.rgba.len() != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "a {}x{} framebuffer needs {expected} bytes of RGBA, got {}",
                fb.width,
                fb.height,
                fb.rgba.len()
            ),
        ));
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, u32::from(fb.width), u32::from(fb.height));
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
        writer
            .write_image_data(&fb.rgba)
            .map_err(std::io::Error::other)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eight bytes every PNG starts with.
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

    fn gradient(width: u16, height: u16) -> Framebuffer {
        let mut fb = Framebuffer::black(width, height);
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                fb.rgba[i] = (x * 7) as u8;
                fb.rgba[i + 1] = (y * 11) as u8;
                fb.rgba[i + 2] = ((x + y) * 3) as u8;
                fb.rgba[i + 3] = 255;
            }
        }
        fb
    }

    #[test]
    fn the_output_is_a_png() {
        let bytes = encode_png(&gradient(4, 3)).expect("encoded");
        assert_eq!(&bytes[..8], &SIGNATURE, "not a PNG: {:?}", &bytes[..8]);
    }

    /// The `png` crate reads as well as it writes, so the round trip is a real
    /// assertion about the pixels rather than about our own encoder's opinion.
    #[test]
    fn decoding_the_output_returns_the_same_pixels() {
        let fb = gradient(9, 5);
        let bytes = encode_png(&fb).expect("encoded");

        let decoder = png::Decoder::new(std::io::Cursor::new(&bytes));
        let mut reader = decoder.read_info().expect("read_info");
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("next_frame");

        assert_eq!((info.width, info.height), (9, 5), "dimensions");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        assert_eq!(&buf[..info.buffer_size()], &fb.rgba[..], "pixels");
    }

    #[test]
    fn a_zero_sized_framebuffer_is_an_error_not_a_panic() {
        for (width, height) in [(0u16, 4u16), (4, 0), (0, 0)] {
            let err = encode_png(&Framebuffer::black(width, height))
                .expect_err("a {width}x{height} screen cannot be a PNG");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    /// A framebuffer whose buffer does not match its dimensions is a bug upstream;
    /// saying so beats handing the `png` crate a short slice and panicking there.
    #[test]
    fn a_framebuffer_that_lies_about_its_size_is_an_error() {
        let mut fb = Framebuffer::black(4, 4);
        fb.rgba.truncate(8);
        let err = encode_png(&fb).expect_err("a short buffer cannot be a PNG");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
