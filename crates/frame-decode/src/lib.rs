//! PNG and JPEG images as premultiplied BGRA frames.
//!
//! APNG uses only its default (first) image. Colour metadata is ignored: pixels
//! are assumed to be sRGB. Only JPEG applies EXIF orientation; PNG eXIf is ignored.
//! Zune can accept truncated JPEG entropy streams, even with strict mode enabled.

#![forbid(unsafe_code)]

mod exif;

use std::io::Cursor;

use media_core::{FrameBuffer, PixelFormat, VideoFrame};
use zune_core::{colorspace::ColorSpace, options::DecoderOptions};
use zune_jpeg::{JpegDecoder, errors::DecodeErrors};

/// Size limits for one decoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most pixels per side (width and height each).
    pub max_side: u32,
    /// Most pixels in all.
    pub max_pixels: u64,
}

impl Limits {
    /// The image source's limits (ADR 0019): 8192 per side, 16 Mi pixels (64 MiB as BGRA).
    pub const IMAGE: Limits = Limits {
        max_side: 8192,
        max_pixels: 16 * 1024 * 1024,
    };
}

/// An image's signature format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

/// What a file's header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: ImageFormat,
    /// Size as shown: after the JPEG's EXIF orientation (5 to 8 swap width and height).
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not a PNG or JPEG file (by its signature), or a variant the decoder cannot read.
    /// The message says which.
    #[error("{0}")]
    Unsupported(String),
    /// Larger than the limits; checked from the header, before decoding.
    #[error("{width}×{height} pixels is larger than the limit ({} pixels per side, {} pixels in all)", limits.max_side, limits.max_pixels)]
    TooLarge {
        width: u32,
        height: u32,
        limits: Limits,
    },
    /// Truncated or corrupt data. The message is the decoder's.
    #[error("{0}")]
    Invalid(String),
}

/// Read only the header of a PNG or JPEG file. Works on a prefix of the file (the
/// engine passes at most the first 256 KiB when listing a folder), as long as the header
/// is in it: for a PNG the IHDR chunk, for a JPEG the segments up to the first SOF
/// (including APP1/EXIF for the orientation). Does not check limits.
pub fn probe(bytes: &[u8]) -> Result<ImageInfo, DecodeError> {
    match format(bytes)? {
        ImageFormat::Png => {
            let mut decoder = png_decoder(bytes, Limits::IMAGE);
            let info = decoder.read_header_info().map_err(invalid)?;
            Ok(ImageInfo {
                format: ImageFormat::Png,
                width: info.width,
                height: info.height,
            })
        }
        ImageFormat::Jpeg => jpeg_header(bytes).map(|header| header.info),
    }
}

/// Decode a whole PNG or JPEG file into a tightly packed `PixelFormat::Bgra8` frame with
/// premultiplied alpha (`media_core::VideoFrame`, `FrameBuffer::unpooled`, pts `None`),
/// shown as the file says: the JPEG's EXIF orientation applied. The limits are checked
/// from the header before the pixel buffer is allocated.
pub fn decode_image(bytes: &[u8], limits: Limits) -> Result<VideoFrame, DecodeError> {
    match format(bytes)? {
        ImageFormat::Png => decode_png(bytes, limits),
        ImageFormat::Jpeg => decode_jpeg(bytes, limits),
    }
}

fn format(bytes: &[u8]) -> Result<ImageFormat, DecodeError> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok(ImageFormat::Png)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Ok(ImageFormat::Jpeg)
    } else {
        Err(DecodeError::Unsupported(
            "not a PNG or JPEG file (by its signature)".into(),
        ))
    }
}

fn invalid(error: impl std::fmt::Display) -> DecodeError {
    DecodeError::Invalid(error.to_string())
}

fn check_size(width: u32, height: u32, limits: Limits) -> Result<(), DecodeError> {
    if width > limits.max_side
        || height > limits.max_side
        || u64::from(width) * u64::from(height) > limits.max_pixels
    {
        return Err(DecodeError::TooLarge {
            width,
            height,
            limits,
        });
    }
    Ok(())
}

fn buffer(len: usize) -> Result<Vec<u8>, DecodeError> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(invalid)?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn frame(width: u32, height: u32, data: Vec<u8>) -> Result<VideoFrame, DecodeError> {
    let stride = VideoFrame::packed_stride(width).map_err(invalid)?;
    VideoFrame::new(
        width,
        height,
        stride,
        PixelFormat::Bgra8,
        None,
        FrameBuffer::unpooled(data),
    )
    .map_err(invalid)
}

fn png_decoder(bytes: &[u8], limits: Limits) -> png::Decoder<Cursor<&[u8]>> {
    // A maximal 16-bit RGBA image, filter bytes and bounded metadata/row scratch.
    // Saturation only matters for caller-supplied limits larger than addressable memory.
    let pixels = limits.max_pixels.min(u64::from(limits.max_side).pow(2));
    let budget = pixels
        .saturating_mul(8)
        .saturating_add(u64::from(limits.max_side) * 64)
        .saturating_add(1024 * 1024);
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: usize::try_from(budget)
                .unwrap_or(usize::MAX)
                .min(isize::MAX as usize),
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder
}

fn decode_png(bytes: &[u8], limits: Limits) -> Result<VideoFrame, DecodeError> {
    let mut decoder = png_decoder(bytes, limits);
    // Check IHDR before read_info can prepare even a single oversized scanline.
    let header = decoder.read_header_info().map_err(invalid)?;
    check_size(header.width, header.height, limits)?;
    let mut reader = decoder.read_info().map_err(invalid)?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    check_size(width, height, limits)?;
    let stride = VideoFrame::packed_stride(width).map_err(invalid)?;
    let len = VideoFrame::required_len(width, height, stride).map_err(invalid)?;
    let mut raw = buffer(
        reader
            .output_buffer_size()
            .ok_or_else(|| invalid("the PNG's size overflows usize"))?,
    )?;
    let output = reader.next_frame(&mut raw).map_err(invalid)?;
    reader.finish().map_err(invalid)?;
    let mut bgra = buffer(len)?;
    let channels = output.color_type.samples();
    for (src, dst) in raw[..output.buffer_size()]
        .chunks_exact(channels)
        .zip(bgra.as_chunks_mut::<4>().0)
    {
        let (r, g, b, a) = match output.color_type {
            png::ColorType::Grayscale => (src[0], src[0], src[0], 255),
            png::ColorType::GrayscaleAlpha => (src[0], src[0], src[0], src[1]),
            png::ColorType::Rgb => (src[0], src[1], src[2], 255),
            png::ColorType::Rgba => (src[0], src[1], src[2], src[3]),
            png::ColorType::Indexed => return Err(invalid("the PNG palette was not expanded")),
        };
        let premultiply = |c| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        dst.copy_from_slice(&[premultiply(b), premultiply(g), premultiply(r), a]);
    }
    frame(width, height, bgra)
}

fn jpeg_error(error: DecodeErrors) -> DecodeError {
    match error {
        DecodeErrors::Unsupported(_) => DecodeError::Unsupported(error.to_string()),
        _ => invalid(error),
    }
}

fn decode_jpeg(bytes: &[u8], limits: Limits) -> Result<VideoFrame, DecodeError> {
    // Preflight also gives TooLarge for a header with no scan data and for side
    // limits (which zune otherwise reports as an unstructured format error).
    let header = jpeg_header(bytes)?;
    check_size(header.info.width, header.info.height, limits)?;
    let options = DecoderOptions::default()
        .set_max_width(limits.max_side as usize)
        .set_max_height(limits.max_side as usize)
        .set_strict_mode(true)
        .jpeg_set_out_colorspace(ColorSpace::BGRA);
    let mut decoder = JpegDecoder::new_with_options(Cursor::new(bytes), options);
    decoder.decode_headers().map_err(jpeg_error)?;
    let info = decoder
        .info()
        .ok_or_else(|| invalid("the JPEG has no dimensions"))?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    let orientation = header.orientation;
    let (shown_width, shown_height) = oriented_size(width, height, orientation);
    check_size(shown_width, shown_height, limits)?;
    let stride = VideoFrame::packed_stride(width).map_err(invalid)?;
    let len = VideoFrame::required_len(width, height, stride).map_err(invalid)?;
    // Zune supports direct BGRA only for YCbCr. Its other supported mappings
    // are Luma -> Luma and RGB/CMYK/YCCK -> RGB. Zune 0.5's CMYK/YCCK
    // four-channel converters have inconsistent row/channel strides; use RGB.
    let output = match decoder.input_colorspace() {
        Some(ColorSpace::YCbCr) => ColorSpace::BGRA,
        Some(ColorSpace::Luma) => ColorSpace::Luma,
        Some(ColorSpace::RGB | ColorSpace::CMYK | ColorSpace::YCCK) => ColorSpace::RGB,
        other => {
            return Err(DecodeError::Unsupported(format!(
                "unsupported JPEG colour space {other:?}"
            )));
        }
    };
    if output != ColorSpace::BGRA {
        // Recreate it so zune selects the matching colour converter as well as
        // the output layout (set_options alone does not update that converter).
        decoder = JpegDecoder::new_with_options(
            Cursor::new(bytes),
            options.jpeg_set_out_colorspace(output),
        );
        decoder.decode_headers().map_err(jpeg_error)?;
    }
    let mut pixels = buffer(len)?;
    decoder.decode_into(&mut pixels).map_err(jpeg_error)?;
    // Expand backwards so packed Luma/RGB can reuse the BGRA allocation.
    for index in (0..len / 4).rev() {
        let pixel = match output {
            ColorSpace::Luma => [pixels[index], pixels[index], pixels[index], 255],
            ColorSpace::RGB => [
                pixels[index * 3 + 2],
                pixels[index * 3 + 1],
                pixels[index * 3],
                255,
            ],
            _ => [
                pixels[index * 4],
                pixels[index * 4 + 1],
                pixels[index * 4 + 2],
                255,
            ],
        };
        pixels[index * 4..index * 4 + 4].copy_from_slice(&pixel);
    }
    orient(pixels, width, height, orientation)
}

fn oriented_size(width: u32, height: u32, orientation: u8) -> (u32, u32) {
    if orientation >= 5 {
        (height, width)
    } else {
        (width, height)
    }
}

fn orient(
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    orientation: u8,
) -> Result<VideoFrame, DecodeError> {
    if orientation == 1 {
        return frame(width, height, pixels);
    }
    let (shown_width, shown_height) = oriented_size(width, height, orientation);
    let mut shown = buffer(pixels.len())?;
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = match orientation {
                2 => (width - 1 - x, y),
                3 => (width - 1 - x, height - 1 - y),
                4 => (x, height - 1 - y),
                5 => (y, x),
                6 => (height - 1 - y, x),
                7 => (height - 1 - y, width - 1 - x),
                8 => (y, width - 1 - x),
                _ => (x, y),
            };
            let src = (y as usize * width as usize + x as usize) * 4;
            let dst = (dy as usize * shown_width as usize + dx as usize) * 4;
            shown[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    frame(shown_width, shown_height, shown)
}

/// A JPEG's size as shown, and the EXIF orientation that gives it.
struct JpegHeader {
    info: ImageInfo,
    orientation: u8,
}

/// Walk length-delimited JPEG segments without decoding or allocating metadata.
fn jpeg_header(bytes: &[u8]) -> Result<JpegHeader, DecodeError> {
    let mut offset = 2usize;
    let mut orientation = 1;
    loop {
        if bytes.get(offset) != Some(&0xff) {
            return Err(invalid("a JPEG marker is missing"));
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes
            .get(offset)
            .ok_or_else(|| invalid("truncated JPEG marker"))?;
        offset += 1;
        if matches!(marker, 0 | 0xd8..=0xda) {
            return Err(invalid("the JPEG ends before its frame header"));
        }
        if marker == 1 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let length_bytes = bytes
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("truncated JPEG segment length"))?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err(invalid("invalid JPEG segment length"));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("a JPEG offset overflows usize"))?;
        let segment = bytes
            .get(offset + 2..end)
            .ok_or_else(|| invalid("truncated JPEG segment"))?;
        if marker == 0xe1 && segment.starts_with(b"Exif\0\0") {
            orientation = exif::orientation(&segment[6..]);
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if !matches!(marker, 0xc0..=0xc2) {
                return Err(DecodeError::Unsupported(format!(
                    "unsupported JPEG frame type (SOF marker FF{marker:02X})"
                )));
            }
            if segment.len() < 6 {
                return Err(invalid("truncated JPEG frame header"));
            }
            if segment[0] != 8 {
                return Err(DecodeError::Unsupported(format!(
                    "unsupported {}-bit JPEG",
                    segment[0]
                )));
            }
            if segment.len() != 6 + usize::from(segment[5]) * 3 || segment[5] == 0 {
                return Err(invalid("invalid JPEG frame components"));
            }
            let height = u32::from(u16::from_be_bytes([segment[1], segment[2]]));
            let width = u32::from(u16::from_be_bytes([segment[3], segment[4]]));
            if width == 0 || height == 0 {
                return Err(invalid("the JPEG has a width or height of 0"));
            }
            let (width, height) = oriented_size(width, height, orientation);
            return Ok(JpegHeader {
                info: ImageInfo {
                    format: ImageFormat::Jpeg,
                    width,
                    height,
                },
                orientation,
            });
        }
        if marker == 0xcc {
            return Err(DecodeError::Unsupported(
                "arithmetic-coded JPEGs are not supported".into(),
            ));
        }
        offset = end;
    }
}

#[cfg(test)]
mod tests;
