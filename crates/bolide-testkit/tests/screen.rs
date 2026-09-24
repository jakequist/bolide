//! `Screen`'s two constructors that do real work: the PNG loader and the pattern.

use bolide_testkit::{Screen, TestkitError};

const RED: [u8; 4] = [255, 0, 0, 255];

/// Encode a PNG with the same crate the loader reads, so the test states an image
/// rather than a byte blob nobody can check.
fn encode_png(width: u32, height: u32, colour_type: png::ColorType, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(colour_type);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("a header");
        writer.write_image_data(data).expect("image data");
    }
    out
}

#[test]
fn from_png_round_trips_an_rgba_image() {
    let mut original = Screen::solid(3, 2, RED);
    original.fill_rect(2, 1, 1, 1, [0, 0, 255, 128]);
    let png = encode_png(3, 2, png::ColorType::Rgba, &original.rgba);

    assert_eq!(Screen::from_png(&png).unwrap(), original);
}

#[test]
fn from_png_fills_alpha_for_an_rgb_image() {
    // Three-channel source: RFB has no alpha, so an opaque screen is the only honest
    // reading of a PNG that does not carry one.
    let png = encode_png(2, 1, png::ColorType::Rgb, &[255, 0, 0, 0, 255, 0]);
    let screen = Screen::from_png(&png).unwrap();

    assert_eq!((screen.width, screen.height), (2, 1));
    assert_eq!(screen.pixel(0, 0), Some(RED));
    assert_eq!(screen.pixel(1, 0), Some([0, 255, 0, 255]));
}

#[test]
fn from_png_normalises_greyscale() {
    let png = encode_png(2, 1, png::ColorType::Grayscale, &[0x00, 0x80]);
    let screen = Screen::from_png(&png).unwrap();

    assert_eq!(screen.pixel(0, 0), Some([0, 0, 0, 255]));
    assert_eq!(screen.pixel(1, 0), Some([0x80, 0x80, 0x80, 255]));
}

#[test]
fn from_png_says_so_rather_than_panicking_on_rubbish() {
    let err = Screen::from_png(b"this is not a png").unwrap_err();
    assert!(
        matches!(err, TestkitError::Png(_)),
        "expected a Png error, got {err:?}"
    );
    // The message has to name the problem; it is what a test author reads.
    assert!(!err.to_string().is_empty());
}

#[test]
fn checkerboard_repeats_four_colours_in_cell_sized_squares() {
    let screen = Screen::checkerboard(8, 8, 2);
    assert_eq!((screen.width, screen.height), (8, 8));

    // A cell is uniform …
    assert_eq!(screen.pixel(0, 0), screen.pixel(1, 1));
    // … its neighbours differ …
    assert_ne!(screen.pixel(0, 0), screen.pixel(2, 0));
    assert_ne!(screen.pixel(0, 0), screen.pixel(0, 2));
    assert_ne!(screen.pixel(2, 0), screen.pixel(0, 2));
    // … and the 2×2 arrangement of cells repeats.
    assert_eq!(screen.pixel(0, 0), screen.pixel(4, 4));

    // Four colours exactly: enough for a packed palette, not enough for a raw tile.
    let mut colours: Vec<[u8; 4]> = Vec::new();
    for y in 0..8 {
        for x in 0..8 {
            let p = screen.pixel(x, y).unwrap();
            if !colours.contains(&p) {
                colours.push(p);
            }
        }
    }
    assert_eq!(colours.len(), 4);
}

#[test]
fn a_checkerboard_cell_of_zero_reads_as_one() {
    // Rather than dividing by zero, which is how a test helper becomes a panic.
    let screen = Screen::checkerboard(4, 4, 0);
    assert_eq!((screen.width, screen.height), (4, 4));
    assert_ne!(screen.pixel(0, 0), screen.pixel(1, 0));
}
