use super::*;
use png::{BitDepth, ColorType};

const JPEG: &[u8] = include_bytes!("../tests/data/blocks.jpg");
const COLORS: [[u8; 4]; 6] = [
    [0, 0, 255, 255],
    [0, 255, 0, 255],
    [255, 0, 0, 255],
    [255, 255, 255, 255],
    [0, 0, 0, 255],
    [0, 255, 255, 255],
];

fn png(width: u32, height: u32, color: ColorType, depth: BitDepth, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
        writer.finish().unwrap();
    }
    bytes
}

fn assert_frame(bytes: &[u8], width: u32, height: u32, pixels: &[u8]) {
    let frame = decode_image(bytes, Limits::IMAGE).unwrap();
    assert_eq!((frame.width(), frame.height()), (width, height));
    assert_eq!(frame.format(), PixelFormat::Bgra8);
    assert_eq!(frame.stride(), width as usize * 4);
    assert_eq!(frame.pts(), None);
    assert_eq!(frame.data(), pixels);
}

#[test]
fn png_rgba_premultiplies_with_exact_rounding() {
    let bytes = png(
        4,
        1,
        ColorType::Rgba,
        BitDepth::Eight,
        &[
            200, 100, 50, 128, 1, 2, 3, 0, 17, 83, 255, 255, 127, 128, 255, 1,
        ],
    );
    assert_frame(
        &bytes,
        4,
        1,
        &[25, 50, 100, 128, 0, 0, 0, 0, 255, 83, 17, 255, 1, 1, 0, 1],
    );
}

#[test]
fn png_rgb_and_sixteen_bit_rgba() {
    assert_frame(
        &png(1, 1, ColorType::Rgb, BitDepth::Eight, &[1, 128, 255]),
        1,
        1,
        &[255, 128, 1, 255],
    );
    assert_frame(
        &png(
            2,
            1,
            ColorType::Rgba,
            BitDepth::Sixteen,
            &[
                200, 99, 100, 88, 50, 77, 128, 66, 17, 255, 83, 255, 255, 0, 255, 255,
            ],
        ),
        2,
        1,
        &[25, 50, 100, 128, 255, 83, 17, 255],
    );
}

#[test]
fn png_gray_at_every_depth_and_gray_alpha() {
    for (depth, input, expected) in [
        (BitDepth::One, vec![0b0100_0000], vec![0, 255, 0]),
        (BitDepth::Two, vec![0b0001_1000], vec![0, 85, 170]),
        (BitDepth::Four, vec![0x08, 0xf0], vec![0, 136, 255]),
        (BitDepth::Eight, vec![0, 127, 255], vec![0, 127, 255]),
        (
            BitDepth::Sixteen,
            vec![0, 255, 127, 128, 255, 0],
            vec![0, 127, 255],
        ),
    ] {
        let pixels: Vec<_> = expected.into_iter().flat_map(|v| [v, v, v, 255]).collect();
        assert_frame(
            &png(3, 1, ColorType::Grayscale, depth, &input),
            3,
            1,
            &pixels,
        );
    }
    assert_frame(
        &png(
            2,
            1,
            ColorType::GrayscaleAlpha,
            BitDepth::Eight,
            &[100, 128, 255, 0],
        ),
        2,
        1,
        &[50, 50, 50, 128, 0, 0, 0, 0],
    );
    assert_frame(
        &png(
            1,
            1,
            ColorType::GrayscaleAlpha,
            BitDepth::Sixteen,
            &[100, 255, 128, 255],
        ),
        1,
        1,
        &[50, 50, 50, 128],
    );
    assert_frame(
        &png(
            1,
            1,
            ColorType::Rgb,
            BitDepth::Sixteen,
            &[17, 255, 83, 255, 255, 0],
        ),
        1,
        1,
        &[255, 83, 17, 255],
    );
}

#[test]
fn png_palette_expands_transparency() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 3, 1);
        encoder.set_color(ColorType::Indexed);
        encoder.set_depth(BitDepth::Two);
        encoder.set_palette(vec![200, 100, 50, 255, 255, 255, 17, 83, 255]);
        encoder.set_trns(vec![128, 0]); // Missing entries are opaque.
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0b0001_1000]).unwrap();
        writer.finish().unwrap();
    }
    assert_frame(
        &bytes,
        3,
        1,
        &[25, 50, 100, 128, 0, 0, 0, 0, 255, 83, 17, 255],
    );
}

#[test]
fn png_adam7_is_pixel_exact() {
    let bytes = include_bytes!("../tests/data/adam7.png");
    assert_eq!(bytes[28], 1, "fixture uses Adam7");
    let expected: Vec<_> = (0..16)
        .flat_map(|y| (0..24).flat_map(move |x| COLORS[(y / 8) * 3 + x / 8]))
        .collect();
    assert_frame(bytes, 24, 16, &expected);
}

#[test]
fn apng_uses_only_the_default_image() {
    for separate_default in [false, true] {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(ColorType::Rgba);
            encoder
                .set_animated(if separate_default { 1 } else { 2 }, 0)
                .unwrap();
            encoder.set_sep_def_img(separate_default).unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[200, 100, 50, 128]).unwrap();
            writer.write_image_data(&[0, 255, 0, 255]).unwrap();
            writer.finish().unwrap();
        }
        assert_frame(&bytes, 1, 1, &[25, 50, 100, 128]);
    }
}

fn tiff(orientation: u16, little: bool) -> Vec<u8> {
    let short = |value: u16| {
        if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        }
    };
    let long = |value: u32| {
        if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        }
    };
    let mut bytes = if little {
        b"II".to_vec()
    } else {
        b"MM".to_vec()
    };
    bytes.extend(short(42));
    bytes.extend(long(8));
    bytes.extend(short(1));
    bytes.extend(short(0x0112));
    bytes.extend(short(3));
    bytes.extend(long(1));
    bytes.extend(short(orientation));
    bytes.extend([0; 6]);
    bytes
}

fn with_exif(jpeg: &[u8], tiff: &[u8]) -> Vec<u8> {
    let mut bytes = jpeg[..2].to_vec();
    bytes.extend([0xff, 0xe1]);
    bytes.extend(u16::try_from(tiff.len() + 8).unwrap().to_be_bytes());
    bytes.extend(b"Exif\0\0");
    bytes.extend(tiff);
    bytes.extend(&jpeg[2..]);
    bytes
}

fn assert_near(actual: [u8; 4], expected: [u8; 4], tolerance: u8) {
    assert_eq!(actual[3], 255);
    for channel in 0..3 {
        assert!(
            actual[channel].abs_diff(expected[channel]) <= tolerance,
            "{actual:?} differs from {expected:?} by more than {tolerance}"
        );
    }
}

#[test]
fn jpeg_baseline_progressive_and_subsampling() {
    for bytes in [
        JPEG,
        include_bytes!("../tests/data/progressive.jpg"),
        include_bytes!("../tests/data/420.jpg"),
        include_bytes!("../tests/data/422.jpg"),
        include_bytes!("../tests/data/cmyk.jpg"),
        include_bytes!("../tests/data/rgb.jpg"),
    ] {
        let frame = decode_image(bytes, Limits::IMAGE).unwrap();
        assert_eq!(
            (frame.width(), frame.height(), frame.stride()),
            (24, 16, 96)
        );
        assert_eq!(frame.format(), PixelFormat::Bgra8);
        assert_eq!(frame.pts(), None);
        for (index, color) in COLORS.iter().enumerate() {
            assert_near(
                frame.pixel((index % 3 * 8 + 4) as u32, (index / 3 * 8 + 4) as u32),
                *color,
                3,
            );
        }
    }
}

#[test]
fn jpeg_greyscale_becomes_opaque_bgra() {
    let frame = decode_image(include_bytes!("../tests/data/gray.jpg"), Limits::IMAGE).unwrap();
    for (index, luma) in [76, 150, 29, 255, 0, 226].into_iter().enumerate() {
        assert_near(
            frame.pixel((index % 3 * 8 + 4) as u32, (index / 3 * 8 + 4) as u32),
            [luma, luma, luma, 255],
            1,
        );
    }
}

#[test]
fn jpeg_all_orientations_and_both_byte_orders() {
    // Independent destination block maps, row by row, for the six source blocks.
    let maps: [&[usize]; 8] = [
        &[0, 1, 2, 3, 4, 5],
        &[2, 1, 0, 5, 4, 3],
        &[5, 4, 3, 2, 1, 0],
        &[3, 4, 5, 0, 1, 2],
        &[0, 3, 1, 4, 2, 5],
        &[3, 0, 4, 1, 5, 2],
        &[5, 2, 4, 1, 3, 0],
        &[2, 5, 1, 4, 0, 3],
    ];
    for orientation in 1..=8 {
        for little in [false, true] {
            let bytes = with_exif(JPEG, &tiff(orientation, little));
            let frame = decode_image(&bytes, Limits::IMAGE).unwrap();
            let (width, height) = if orientation >= 5 { (16, 24) } else { (24, 16) };
            assert_eq!((frame.width(), frame.height()), (width, height));
            assert_eq!(
                probe(&bytes).unwrap(),
                ImageInfo {
                    format: ImageFormat::Jpeg,
                    width,
                    height
                }
            );
            for (index, source) in maps[orientation as usize - 1].iter().enumerate() {
                let x = index as u32 % (width / 8) * 8 + 4;
                let y = index as u32 / (width / 8) * 8 + 4;
                assert_near(frame.pixel(x, y), COLORS[*source], 1);
            }
        }
    }
}

#[test]
fn orientation_transforms_every_pixel() {
    let pixels: Vec<_> = (0..6).flat_map(|i| [i, i, i, 255]).collect();
    for (orientation, expected) in [
        [0, 1, 2, 3, 4, 5],
        [2, 1, 0, 5, 4, 3],
        [5, 4, 3, 2, 1, 0],
        [3, 4, 5, 0, 1, 2],
        [0, 3, 1, 4, 2, 5],
        [3, 0, 4, 1, 5, 2],
        [5, 2, 4, 1, 3, 0],
        [2, 5, 1, 4, 0, 3],
    ]
    .into_iter()
    .enumerate()
    {
        let frame = orient(pixels.clone(), 3, 2, orientation as u8 + 1).unwrap();
        assert_eq!(
            frame
                .data()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn malformed_exif_defaults_to_identity() {
    let valid = tiff(6, true);
    let mut malformed = vec![vec![], b"Exif\0\0".to_vec(), tiff(0, true), tiff(9, true)];
    for len in 0..valid.len() {
        malformed.push(valid[..len].to_vec());
    }
    for (offset, values) in [
        (0, vec![0, 0]),
        (2, vec![0, 0]),
        (4, vec![255; 4]),
        (4, vec![0; 4]),
        (8, vec![255; 2]),
        (10, vec![0; 2]),
        (12, vec![4, 0]),
        (14, vec![2, 0, 0, 0]),
    ] {
        let mut bad = valid.clone();
        bad[offset..offset + values.len()].copy_from_slice(&values);
        malformed.push(bad);
    }
    let reference = decode_image(JPEG, Limits::IMAGE).unwrap();
    for tiff in malformed {
        assert_eq!(exif::orientation(&tiff), 1, "{tiff:?}");
        let bytes = with_exif(JPEG, &tiff);
        assert_eq!(probe(&bytes).unwrap().width, 24);
        let frame = decode_image(&bytes, Limits::IMAGE).unwrap();
        assert_eq!((frame.width(), frame.height()), (24, 16));
        assert_eq!(frame.data(), reference.data());
    }
}

fn sof_end(bytes: &[u8]) -> usize {
    let offset = bytes.windows(2).position(|p| p == [0xff, 0xc0]).unwrap();
    offset + 2 + usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]))
}

#[test]
fn probe_reads_prefixes_without_pixel_data_or_limits() {
    let png = png(2, 1, ColorType::Rgb, BitDepth::Eight, &[1; 6]);
    assert_eq!(
        probe(&png[..33]).unwrap(),
        ImageInfo {
            format: ImageFormat::Png,
            width: 2,
            height: 1
        }
    );
    let bytes = with_exif(JPEG, &tiff(6, false));
    assert_eq!(
        probe(&bytes[..sof_end(&bytes)]).unwrap(),
        ImageInfo {
            format: ImageFormat::Jpeg,
            width: 16,
            height: 24
        }
    );
    let huge = png_header(8193, 1);
    assert_eq!(probe(&huge).unwrap().width, 8193);
}

fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.write_header().unwrap().finish().unwrap();
    bytes.truncate(33);
    bytes
}

fn assert_too_large(bytes: &[u8], width: u32, height: u32, limits: Limits) {
    assert_eq!(
        decode_image(bytes, limits).unwrap_err(),
        DecodeError::TooLarge {
            width,
            height,
            limits
        }
    );
}

#[test]
fn dimensions_are_refused_from_headers_before_decoding() {
    assert_too_large(&png_header(8193, 1), 8193, 1, Limits::IMAGE);
    assert_too_large(&png_header(8192, 8192), 8192, 8192, Limits::IMAGE);
    assert_too_large(
        &png_header(3, 1),
        3,
        1,
        Limits {
            max_side: 2,
            max_pixels: 10,
        },
    );
    assert_too_large(
        &png_header(2, 2),
        2,
        2,
        Limits {
            max_side: 2,
            max_pixels: 3,
        },
    );
    let jpeg = &JPEG[..sof_end(JPEG)];
    assert_too_large(
        jpeg,
        24,
        16,
        Limits {
            max_side: 23,
            max_pixels: 1000,
        },
    );
    assert_too_large(
        jpeg,
        24,
        16,
        Limits {
            max_side: 24,
            max_pixels: 383,
        },
    );
    assert_too_large(
        &with_exif(jpeg, &tiff(6, true)),
        16,
        24,
        Limits {
            max_side: 24,
            max_pixels: 383,
        },
    );
    assert!(matches!(
        decode_image(&png_header(4096, 4096), Limits::IMAGE),
        Err(DecodeError::Invalid(_))
    ));
    assert!(
        decode_image(
            JPEG,
            Limits {
                max_side: 24,
                max_pixels: 384
            }
        )
        .is_ok()
    );
}

#[test]
fn signatures_and_unsupported_jpeg_are_explicit() {
    for bytes in [b"".as_slice(), b"garbage", b"\xff\xd8", b"GIF89a"] {
        assert!(matches!(probe(bytes), Err(DecodeError::Unsupported(_))));
        assert!(matches!(
            decode_image(bytes, Limits::IMAGE),
            Err(DecodeError::Unsupported(_))
        ));
    }
    for bytes in [b"\x89PNG\r\n\x1a\n".as_slice(), b"\xff\xd8\xff"] {
        assert!(matches!(probe(bytes), Err(DecodeError::Invalid(_))));
        assert!(matches!(
            decode_image(bytes, Limits::IMAGE),
            Err(DecodeError::Invalid(_))
        ));
    }
    let arithmetic = include_bytes!("../tests/data/arithmetic.jpg");
    assert!(matches!(
        probe(arithmetic),
        Err(DecodeError::Unsupported(_))
    ));
    assert!(matches!(
        decode_image(arithmetic, Limits::IMAGE),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn truncated_files_never_panic() {
    let png = png(2, 2, ColorType::Rgba, BitDepth::Eight, &[123; 16]);
    for end in 0..png.len() {
        assert!(
            decode_image(&png[..end], Limits::IMAGE).is_err(),
            "PNG prefix {end}"
        );
        let _ = probe(&png[..end]);
    }
    for bytes in [JPEG, include_bytes!("../tests/data/progressive.jpg")] {
        for end in 0..bytes.len() {
            // Zune tolerates some truncated entropy streams, even in strict mode.
            // Successful decodes must still have the complete bounded geometry.
            if let Ok(frame) = decode_image(&bytes[..end], Limits::IMAGE) {
                assert_eq!(
                    (frame.width(), frame.height(), frame.data().len()),
                    (24, 16, 24 * 16 * 4)
                );
            }
            let _ = probe(&bytes[..end]);
        }
    }
}

#[test]
fn corruptions_never_panic_and_broken_headers_fail() {
    let png = png(2, 2, ColorType::Rgba, BitDepth::Eight, &[123; 16]);
    let limits = Limits {
        max_side: 32,
        max_pixels: 1024,
    };
    for bytes in [
        png.as_slice(),
        JPEG,
        include_bytes!("../tests/data/progressive.jpg"),
    ] {
        for index in 0..bytes.len() {
            let mut corrupt = bytes.to_vec();
            corrupt[index] ^= 0xff;
            let _ = probe(&corrupt);
            let _ = decode_image(&corrupt, limits);
        }
    }
    // Deterministic random bytes under each supported signature.
    let mut state = 123456789u32;
    for length in 8..256 {
        let mut bytes = vec![0; length];
        for byte in &mut bytes {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(decode_image(&bytes, limits).is_err());
        bytes[..3].copy_from_slice(b"\xff\xd8\xff");
        assert!(decode_image(&bytes, limits).is_err());
    }
}

#[test]
fn png_exif_does_not_rotate_pixels() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
        encoder.set_color(ColorType::Rgb);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_chunk(png::chunk::eXIf, &tiff(6, true))
            .unwrap();
        writer.write_image_data(&[255, 0, 0, 0, 255, 0]).unwrap();
        writer.finish().unwrap();
    }
    assert_frame(&bytes, 2, 1, &[0, 0, 255, 255, 0, 255, 0, 255]);
}

#[test]
fn sixteen_bit_image_fits_its_explicit_memory_budget() {
    let (width, height) = (512, 512);
    let input: Vec<_> = (0..width * height)
        .flat_map(|i| [(i >> 8) as u8, i as u8, 100, 88, 50, 77, 128, 66])
        .collect();
    let bytes = png(width, height, ColorType::Rgba, BitDepth::Sixteen, &input);
    let frame = decode_image(
        &bytes,
        Limits {
            max_side: 512,
            max_pixels: 512 * 512,
        },
    )
    .unwrap();
    assert_eq!(frame.data().len(), 512 * 512 * 4);
    for (index, actual) in frame.data().as_chunks::<4>().0.iter().enumerate() {
        let r = input[index * 8];
        assert_eq!(
            *actual,
            [25, 50, ((u16::from(r) * 128 + 127) / 255) as u8, 128]
        );
    }
}

/// `jpeg` with `segment` (marker and body) spliced in right after its
/// frame header.
fn after_sof(jpeg: &[u8], marker: u8, body: &[u8]) -> Vec<u8> {
    let end = sof_end(jpeg);
    let mut bytes = jpeg[..end].to_vec();
    bytes.extend([0xff, marker]);
    bytes.extend(u16::try_from(body.len() + 2).unwrap().to_be_bytes());
    bytes.extend(body);
    bytes.extend(&jpeg[end..]);
    bytes
}

/// `jpeg` with `count` APP2 segments of `size` bytes each after its SOI.
fn with_app2(jpeg: &[u8], count: usize, size: usize) -> Vec<u8> {
    let mut bytes = jpeg[..2].to_vec();
    for _ in 0..count {
        bytes.extend([0xff, 0xe2]);
        bytes.extend(u16::try_from(size + 2).unwrap().to_be_bytes());
        bytes.extend(std::iter::repeat_n(0x55, size));
    }
    bytes.extend(&jpeg[2..]);
    bytes
}

#[test]
fn exif_after_the_frame_header_is_applied() {
    let mut body = b"Exif\0\0".to_vec();
    body.extend(tiff(6, true));
    let bytes = after_sof(JPEG, 0xe1, &body);
    let shown = ImageInfo {
        format: ImageFormat::Jpeg,
        width: 16,
        height: 24,
    };
    assert_eq!(probe(&bytes).unwrap(), shown);
    assert_eq!(
        probe_reader(&mut std::io::Cursor::new(&bytes)).unwrap(),
        shown
    );
    let frame = decode_image(&bytes, Limits::IMAGE).unwrap();
    assert_eq!((frame.width(), frame.height()), (16, 24));
}

#[test]
fn a_file_probe_seeks_past_large_metadata() {
    // EXIF orientation 6, then 320 KiB of ICC-like segments before the
    // frame header: more than a prefix of 256 KiB holds.
    let bytes = with_exif(&with_app2(JPEG, 5, 65_000), &tiff(6, false));
    assert!(bytes.len() > 320_000);
    let shown = probe_reader(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!((shown.width, shown.height), (16, 24));
    assert!(probe(&bytes[..256 * 1024]).is_err());

    let png = png(3, 2, ColorType::Rgb, BitDepth::Eight, &[9; 18]);
    let info = probe_reader(&mut std::io::Cursor::new(&png)).unwrap();
    assert_eq!(
        (info.format, info.width, info.height),
        (ImageFormat::Png, 3, 2)
    );
    // Cut off before the frame header, or not an image at all.
    let cut = &bytes[..1000];
    assert!(probe_reader(&mut std::io::Cursor::new(cut)).is_err());
    assert!(matches!(
        probe_reader(&mut std::io::Cursor::new(b"GIF89a")),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn too_many_segments_are_refused_before_decoding() {
    let many = with_app2(JPEG, MAX_JPEG_SEGMENTS, 4);
    assert!(matches!(
        decode_image(&many, Limits::IMAGE),
        Err(DecodeError::Unsupported(message)) if message.contains("segments")
    ));
    assert!(probe(&many).is_err());
    assert!(probe_reader(&mut std::io::Cursor::new(&many)).is_err());
    // Below the bound, they are only metadata.
    let some = with_app2(JPEG, 100, 4);
    assert!(decode_image(&some, Limits::IMAGE).is_ok());
}
