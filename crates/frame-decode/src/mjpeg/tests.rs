use super::*;
use crate::{Limits, decode_image};

const JPEG420: &[u8] = include_bytes!("../../tests/data/420.jpg");
const JPEG422: &[u8] = include_bytes!("../../tests/data/422.jpg");

fn without_dht(frame: &[u8]) -> Vec<u8> {
    let mut out = frame[..2].to_vec();
    let mut o = 2;
    while frame[o + 1] != 0xda {
        let length = usize::from(u16::from_be_bytes([frame[o + 2], frame[o + 3]]));
        if frame[o + 1] != 0xc4 {
            out.extend_from_slice(&frame[o..o + 2 + length]);
        }
        o += 2 + length;
    }
    out.extend_from_slice(&frame[o..]);
    out
}

#[test]
fn camera_jpegs_match_image_decoder_and_reuse_one_decoder() {
    let mut decoder = MjpegDecoder::new();
    for fixture in [JPEG420, JPEG422, JPEG420] {
        let expected = decode_image(fixture, Limits::IMAGE).unwrap();
        let mut dst = vec![0xa5; 24 * 16 * 4 + 7];
        decoder.decode_into(fixture, 24, 16, &mut dst).unwrap();
        assert_eq!(&dst[..24 * 16 * 4], expected.data());
        assert!(
            dst[..24 * 16 * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[3] == 255)
        );
        assert_eq!(&dst[24 * 16 * 4..], &[0xa5; 7]);
    }
}

#[test]
fn missing_dht_without_avi1_matches_and_reuses_scratch() {
    let mut decoder = MjpegDecoder::default();
    for fixture in [JPEG420, JPEG422] {
        assert!(!fixture.windows(4).any(|b| b == b"AVI1"));
        let stripped = without_dht(fixture);
        assert!(!stripped.windows(2).any(|b| b == [0xff, 0xc4]));
        let mut expected = vec![0; 24 * 16 * 4];
        decoder.decode_into(fixture, 24, 16, &mut expected).unwrap();
        let mut got = vec![0; 24 * 16 * 4];
        decoder.decode_into(&stripped, 24, 16, &mut got).unwrap();
        let capacity = decoder.scratch.capacity();
        let address = decoder.scratch.as_ptr();
        decoder.decode_into(&stripped, 24, 16, &mut got).unwrap();
        assert_eq!(decoder.scratch.capacity(), capacity);
        assert_eq!(decoder.scratch.as_ptr(), address);
        assert_eq!(got, expected);
        let mut avi = stripped[..2].to_vec();
        avi.extend_from_slice(b"\xff\xe0\x00\x07AVI1\0");
        avi.extend_from_slice(&stripped[2..]);
        decoder.decode_into(&avi, 24, 16, &mut got).unwrap();
        assert_eq!(got, expected);
    }
}

#[test]
fn mjpeg_errors_and_header_mismatch_precedes_decode() {
    let mut decoder = MjpegDecoder::new();
    let mut dst = vec![0xa5; 24 * 16 * 4];
    for (w, h) in [(0, 16), (24, 0)] {
        assert_eq!(
            decoder.decode_into(JPEG420, w, h, &mut dst),
            Err(ConvertError::EmptyPicture)
        );
    }
    assert_eq!(
        decoder.decode_into(JPEG420, 25, 16, &mut []),
        Err(ConvertError::SizeMismatch {
            expected: (25, 16),
            got: (24, 16)
        })
    );
    assert_eq!(
        decoder.decode_into(JPEG420, 24, 16, &mut dst[..1535]),
        Err(ConvertError::BufferTooSmall {
            what: "dst",
            needed: 1536,
            got: 1535
        })
    );
    assert!(
        decoder
            .decode_into(JPEG420, u32::MAX, u32::MAX, &mut [])
            .is_err()
    );
    for n in 0..JPEG420.len() {
        assert!(
            matches!(
                decoder.decode_into(&JPEG420[..n], 24, 16, &mut dst),
                Err(ConvertError::Jpeg(_))
            ),
            "prefix {n}"
        );
    }
    for bytes in [
        &b"not jpeg"[..],
        &b"\xff\xd8\xff\xe0\x00\x01\xff\xd9"[..],
        &b"\xff\xd8\xff\xda\x00\x02\xff\xd9"[..],
    ] {
        assert!(matches!(
            decoder.decode_into(bytes, 24, 16, &mut dst),
            Err(ConvertError::Jpeg(_))
        ));
    }
    // Valid framing and SOF but invalid quantization-table selector: let zune
    // supply its detailed error, rather than losing it at the boundary.
    let mut corrupt = JPEG420.to_vec();
    let sof = corrupt.windows(2).position(|b| b == [0xff, 0xc0]).unwrap();
    corrupt[sof + 12] = 0xff;
    assert!(matches!(
        decoder.decode_into(&corrupt, 24, 16, &mut dst),
        Err(ConvertError::Jpeg(_))
    ));
    assert!(dst.iter().all(|&b| b == 0xa5));
    decoder.decode_into(JPEG420, 24, 16, &mut dst).unwrap();
}

#[test]
fn additional_jpeg_color_spaces_progressive_and_padding() {
    for fixture in [
        &include_bytes!("../../tests/data/gray.jpg")[..],
        &include_bytes!("../../tests/data/rgb.jpg")[..],
        &include_bytes!("../../tests/data/cmyk.jpg")[..],
        &include_bytes!("../../tests/data/progressive.jpg")[..],
    ] {
        let expected = decode_image(fixture, Limits::IMAGE).unwrap();
        let mut padded = fixture.to_vec();
        padded.extend_from_slice(&[0; 32]);
        let mut dst = vec![0; 24 * 16 * 4];
        MjpegDecoder::new()
            .decode_into(&padded, 24, 16, &mut dst)
            .unwrap();
        assert_eq!(dst, expected.data());
    }
}

#[test]
fn byte_corruptions_and_random_headers_do_not_panic() {
    let mut decoder = MjpegDecoder::new();
    let mut dst = vec![0; 24 * 16 * 4];
    for fixture in [JPEG420, JPEG422] {
        for i in 0..fixture.len() {
            let mut damaged = fixture.to_vec();
            damaged[i] ^= 0x5a;
            // Some mutations still describe a valid JPEG. Only require safety.
            let _ = decoder.decode_into(&damaged, 24, 16, &mut dst);
        }
    }
    let mut state = 42u32;
    for length in 0..128 {
        let mut bytes = vec![0xff, 0xd8];
        for _ in 0..length {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            bytes.push((state >> 24) as u8);
        }
        assert!(decoder.decode_into(&bytes, 24, 16, &mut dst).is_err());
    }
}
