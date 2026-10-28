//! MJPEG decoding; only the missing-DHT compressed-input scratch is retained.

use std::io::Cursor;

use zune_core::{colorspace::ColorSpace, options::DecoderOptions};
use zune_jpeg::JpegDecoder;

use crate::{
    ConvertError,
    camera::{mul, require},
};

// JPEG Annex K.3, the four standard DC/AC luminance/chrominance tables.
// One DHT segment, generated from cjpeg's non-optimized tables (see fixtures README).
const STANDARD_DHT: &[u8] = include_bytes!("mjpeg_dht.bin");

/// Decodes MJPEG into a caller's BGRX buffer. Reuses the compressed scratch
/// buffer for frames missing DHT. Zune 0.5 has no reset/reuse API for its own
/// internal buffers, so those still allocate on each decode (see crate docs).
#[derive(Default)]
pub struct MjpegDecoder {
    scratch: Vec<u8>,
}

impl MjpegDecoder {
    /// Create a decoder with empty scratch storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one frame as tightly packed BGRX (X = 255). Checks the stored
    /// header size against `width` x `height` before decoding pixels; camera
    /// frames do not apply EXIF orientation. Missing DHT is supplied from
    /// JPEG Annex K.3, including when there is no AVI1 APP0 marker.
    /// A complete JPEG through EOI is required; trailing buffer padding is allowed.
    pub fn decode_into(
        &mut self,
        frame: &[u8],
        width: u32,
        height: u32,
        dst: &mut [u8],
    ) -> Result<(), ConvertError> {
        if width == 0 || height == 0 {
            return Err(ConvertError::EmptyPicture);
        }
        let len = mul(mul(width as usize, 4, "dst")?, height as usize, "dst")?;
        let (got, has_dht) = inspect(frame, (width, height))?;
        debug_assert_eq!(got, (width, height));
        require(dst, len, "dst")?;
        let bytes =
            if has_dht {
                frame
            } else {
                let needed = frame.len().checked_add(STANDARD_DHT.len()).ok_or(
                    ConvertError::SizeOverflow {
                        what: "JPEG scratch",
                    },
                )?;
                self.scratch.clear();
                self.scratch.try_reserve(needed).map_err(jpeg)?;
                self.scratch.extend_from_slice(&frame[..2]);
                self.scratch.extend_from_slice(STANDARD_DHT);
                self.scratch.extend_from_slice(&frame[2..]);
                &self.scratch
            };
        let options = DecoderOptions::default()
            .set_max_width(width as usize)
            .set_max_height(height as usize)
            .set_strict_mode(true)
            .jpeg_set_out_colorspace(ColorSpace::BGRA);
        let mut decoder = JpegDecoder::new_with_options(Cursor::new(bytes), options);
        decoder.decode_headers().map_err(jpeg)?;
        let output = match decoder.input_colorspace() {
            Some(ColorSpace::YCbCr) => ColorSpace::BGRA,
            Some(ColorSpace::Luma) => ColorSpace::Luma,
            Some(ColorSpace::RGB | ColorSpace::CMYK | ColorSpace::YCCK) => ColorSpace::RGB,
            other => return Err(jpeg(format!("unsupported JPEG colour space {other:?}"))),
        };
        if output != ColorSpace::BGRA {
            decoder = JpegDecoder::new_with_options(
                Cursor::new(bytes),
                options.jpeg_set_out_colorspace(output),
            );
            decoder.decode_headers().map_err(jpeg)?;
        }
        decoder.decode_into(&mut dst[..len]).map_err(jpeg)?;
        if output != ColorSpace::BGRA {
            for i in (0..len / 4).rev() {
                let pixel = if output == ColorSpace::Luma {
                    [dst[i], dst[i], dst[i], 255]
                } else {
                    [dst[i * 3 + 2], dst[i * 3 + 1], dst[i * 3], 255]
                };
                dst[i * 4..i * 4 + 4].copy_from_slice(&pixel);
            }
        }
        Ok(())
    }
}

fn jpeg(e: impl std::fmt::Display) -> ConvertError {
    ConvertError::Jpeg(e.to_string())
}

// Walk markers including scan data (byte stuffing/restart markers) so a short
// entropy stream missing EOI cannot be silently accepted by zune. No allocation.
// Dimensions are checked as soon as SOF is seen, before any decoder allocation.
fn inspect(frame: &[u8], expected: (u32, u32)) -> Result<((u32, u32), bool), ConvertError> {
    if !frame.starts_with(&[0xff, 0xd8]) {
        return Err(jpeg("missing JPEG SOI"));
    }
    let mut offset = 2;
    let mut size = None;
    let mut has_dht = false;
    let mut scan = false;
    let mut segments = 0;
    loop {
        if scan {
            offset += frame[offset..]
                .iter()
                .position(|&b| b == 0xff)
                .ok_or_else(|| jpeg("truncated JPEG scan (missing EOI)"))?;
        }
        if frame.get(offset) != Some(&0xff) {
            return Err(jpeg("missing JPEG marker"));
        }
        while frame.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *frame
            .get(offset)
            .ok_or_else(|| jpeg("truncated JPEG marker"))?;
        offset += 1;
        match marker {
            0 if scan => continue,
            0xd0..=0xd7 if scan => continue,
            0xd9 if scan => return Ok((size.ok_or_else(|| jpeg("missing JPEG SOF"))?, has_dht)),
            0 | 0xd8 | 0xd9 => return Err(jpeg("unexpected JPEG marker")),
            0x01 => continue,
            _ => {}
        }
        segments += 1;
        if segments > crate::MAX_JPEG_SEGMENTS {
            return Err(jpeg("too many JPEG segments"));
        }
        let length = frame
            .get(offset..offset + 2)
            .ok_or_else(|| jpeg("truncated JPEG segment length"))?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        if length < 2 {
            return Err(jpeg("invalid JPEG segment length"));
        }
        let end = offset
            .checked_add(length)
            .ok_or(ConvertError::SizeOverflow {
                what: "JPEG segment",
            })?;
        let body = frame
            .get(offset + 2..end)
            .ok_or_else(|| jpeg("truncated JPEG segment"))?;
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if size.is_some() {
                return Err(jpeg("duplicate JPEG SOF"));
            }
            let got = crate::frame_size(marker, body).map_err(jpeg)?;
            if got != expected {
                return Err(ConvertError::SizeMismatch { expected, got });
            }
            size = Some(got);
        }
        has_dht |= marker == 0xc4;
        if marker == 0xda {
            scan = true;
        }
        offset = end;
    }
}

#[cfg(test)]
mod tests;
