//! PNG/JPEG images as premultiplied BGRA frames, and camera YUV/MJPEG as BGRX.
//!
//! [`convert_yuv`] accepts YUY2, UYVY, NV12 and I420, with nearest-sample chroma
//! (no interpolation), BT.601/BT.709 and limited/full range. It uses borrowed
//! planes and a caller-owned destination, checks bounds before writing, and
//! allocates nothing. Unspecified camera colour defaults to BT.601 limited.
//!
//! [`MjpegDecoder`] checks the stored size before decoding pixels directly into
//! the caller's tightly packed BGRX buffer (X = 255), without EXIF rotation.
//! Missing Huffman tables are supplied from JPEG Annex K.3 in reused compressed
//! scratch storage, even without an AVI1 marker. Zune 0.5 has no decoder reset
//! API: its internal buffers still allocate per frame. With zune-jpeg 0.5.15 on
//! x86_64 Linux, the 1920×1080 quality-85 4:2:2 fixture requests **17 allocations,
//! 212,688 bytes per frame** after warm-up, both with DHT and with DHT removed.
//! These are total allocation/reallocation requests, not peak live memory; other
//! images, versions and platforms may differ. Run `cargo test -p frame-decode
//! --test alloc -- --nocapture` to measure. The missing-DHT scratch does not
//! allocate again for equal-sized or smaller frames.
//!
//! APNG uses only its default (first) image. Colour metadata is ignored: pixels
//! are assumed to be sRGB. Only JPEG image files apply EXIF orientation; PNG
//! eXIf is ignored. Zune can accept damaged/truncated JPEG entropy streams even
//! in strict mode; the camera path additionally requires complete framing through
//! EOI, so frames missing their end marker are rejected.

#![forbid(unsafe_code)]

mod camera;
mod exif;
mod mjpeg;

pub use camera::{
    ColorMatrix, ColorRange, ConvertError, Plane, Planes, YuvColor, YuvLayout, convert_yuv,
};
pub use mjpeg::MjpegDecoder;

use std::io::{Cursor, Read, Seek, SeekFrom};

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

    /// Whether a `width` x `height` image is within the limits.
    pub fn check(self, width: u32, height: u32) -> Result<(), DecodeError> {
        if width > self.max_side
            || height > self.max_side
            || u64::from(width) * u64::from(height) > self.max_pixels
        {
            return Err(DecodeError::TooLarge {
                width,
                height,
                limits: self,
            });
        }
        Ok(())
    }
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

/// [`probe`] from a file: reads only the header, skipping a JPEG's other
/// segments (ICC profiles, XMP, thumbnails) by seeking past them, however
/// large they are.
pub fn probe_reader<R: Read + Seek>(reader: &mut R) -> Result<ImageInfo, DecodeError> {
    let mut head = [0; 8];
    let read = read_full(reader, &mut head).map_err(invalid)?;
    match format(&head[..read])? {
        ImageFormat::Png => {
            // The signature, then IHDR: length, type and 13 bytes of data.
            let mut bytes = vec![0; 33];
            bytes[..8].copy_from_slice(&head);
            let read = read_full(reader, &mut bytes[8..]).map_err(invalid)?;
            bytes.truncate(8 + read);
            probe(&bytes)
        }
        ImageFormat::Jpeg => {
            let mut segments = JpegSegments {
                reader,
                pending: head[2..read].to_vec(),
            };
            segments.header().map(|header| header.info)
        }
    }
}

/// Walks a JPEG file's segments, keeping only those the header parser
/// needs (EXIF and the frame header) in a minimal JPEG.
struct JpegSegments<'r, R> {
    reader: &'r mut R,
    /// Bytes read ahead of the walk.
    pending: Vec<u8>,
}

impl<R: Read + Seek> JpegSegments<'_, R> {
    /// A minimal JPEG for [`jpeg_header`]: the first EXIF segment and the
    /// first frame header, up to the image data; the rest is skipped. Where
    /// the file ends early, the parser decides what that prefix gives.
    fn header(&mut self) -> Result<JpegHeader, DecodeError> {
        let mut kept = vec![0xff, 0xd8];
        let (mut exif, mut frame) = (false, false);
        for _ in 0..=MAX_JPEG_SEGMENTS {
            let Ok(mut marker) = self.next(2) else {
                return jpeg_header(&kept);
            };
            while marker[1] == 0xff {
                let Ok(next) = self.next(1) else {
                    return jpeg_header(&kept);
                };
                marker[1] = next[0];
            }
            if marker[0] != 0xff || matches!(marker[1], 0 | 0xd8..=0xda | 0xcc) {
                // The image data, or what the parser refuses.
                kept.extend_from_slice(&marker);
                kept.extend_from_slice(&[0, 2]);
                return jpeg_header(&kept);
            }
            if marker[1] == 1 || (0xd0..=0xd7).contains(&marker[1]) {
                continue;
            }
            let Ok(length) = self.next(2) else {
                return jpeg_header(&kept);
            };
            let body = usize::from(u16::from_be_bytes([length[0], length[1]])).saturating_sub(2);
            let sof = (0xc0..=0xcf).contains(&marker[1]) && !matches!(marker[1], 0xc4 | 0xc8);
            let wanted = if marker[1] == 0xe1 && !exif {
                let prefix = body.min(6);
                let Ok(start) = self.next(prefix) else {
                    return jpeg_header(&kept);
                };
                if start == b"Exif\0\0" {
                    let Ok(rest) = self.next(body - prefix) else {
                        return jpeg_header(&kept);
                    };
                    exif = true;
                    Some([start, rest].concat())
                } else {
                    self.skip(body - prefix)?;
                    None
                }
            } else if sof && !frame {
                frame = true;
                let Ok(segment) = self.next(body) else {
                    return jpeg_header(&kept);
                };
                Some(segment)
            } else {
                self.skip(body)?;
                None
            };
            if let Some(segment) = wanted {
                kept.extend_from_slice(&marker);
                kept.extend_from_slice(&length);
                kept.extend_from_slice(&segment);
            }
        }
        Err(DecodeError::Unsupported(format!(
            "the JPEG has more than {MAX_JPEG_SEGMENTS} segments before its image data"
        )))
    }

    /// Skip `count` bytes.
    fn skip(&mut self, count: usize) -> Result<(), DecodeError> {
        let buffered = count.min(self.pending.len());
        self.pending.drain(..buffered);
        let skip = i64::try_from(count - buffered).map_err(invalid)?;
        self.reader.seek(SeekFrom::Current(skip)).map_err(invalid)?;
        Ok(())
    }

    /// The next `count` bytes.
    fn next(&mut self, count: usize) -> Result<Vec<u8>, DecodeError> {
        let mut bytes: Vec<u8> = self
            .pending
            .drain(..count.min(self.pending.len()))
            .collect();
        let start = bytes.len();
        bytes.resize(count, 0);
        let read = read_full(self.reader, &mut bytes[start..]).map_err(invalid)?;
        if start + read < count {
            return Err(invalid("truncated JPEG header"));
        }
        Ok(bytes)
    }
}

/// Read until `buffer` is full or the input ends; returns the bytes read.
fn read_full<R: Read>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
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
    limits.check(width, height)
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
    // Decoded 8-bit samples (at most four per pixel) fit in the frame's
    // buffer and are expanded in place: one buffer, not two.
    let decoded = reader
        .output_buffer_size()
        .ok_or_else(|| invalid("the PNG's size overflows usize"))?;
    let mut pixels = buffer(len.max(decoded))?;
    let output = reader.next_frame(&mut pixels).map_err(invalid)?;
    reader.finish().map_err(invalid)?;
    let channels = output.color_type.samples();
    if output.color_type == png::ColorType::Indexed || output.line_size != width as usize * channels
    {
        return Err(invalid("the PNG was not expanded to 8-bit samples"));
    }
    // Backwards: pixel i's samples start at or before byte 4i, and a write
    // overwrites only samples of pixels already expanded.
    for index in (0..len / 4).rev() {
        let src = &pixels[index * channels..index * channels + channels];
        let (r, g, b, a) = match output.color_type {
            png::ColorType::Grayscale => (src[0], src[0], src[0], 255),
            png::ColorType::GrayscaleAlpha => (src[0], src[0], src[0], src[1]),
            png::ColorType::Rgb => (src[0], src[1], src[2], 255),
            _ => (src[0], src[1], src[2], src[3]),
        };
        let premultiply = |c| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        pixels[index * 4..index * 4 + 4].copy_from_slice(&[
            premultiply(b),
            premultiply(g),
            premultiply(r),
            a,
        ]);
    }
    pixels.truncate(len);
    frame(width, height, pixels)
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

/// Most segments a JPEG may have before its image data. zune-jpeg's
/// handling of extended XMP grows with the square of their number, so a
/// small hostile file could stall the engine; real files have dozens.
const MAX_JPEG_SEGMENTS: usize = 1024;

/// Walk length-delimited JPEG segments up to the image data (SOS) without
/// decoding or allocating metadata: the first frame header, the first EXIF
/// orientation (before or after the frame header), and the segment count.
/// A prefix of a file that ends after the frame header gives what it has
/// (a listing reads only the start of a file).
fn jpeg_header(bytes: &[u8]) -> Result<JpegHeader, DecodeError> {
    let mut offset = 2usize;
    let mut orientation = None;
    let mut size: Option<(u32, u32)> = None;
    let mut segments = 0;
    // A prefix that ends after the frame header gives what it has.
    let ended = |size: Option<(u32, u32)>, what: &str| match size {
        Some(_) => Ok(()),
        None => Err(invalid(format!("truncated JPEG {what}"))),
    };
    loop {
        if offset >= bytes.len() {
            ended(size, "header")?;
            break;
        }
        if bytes[offset] != 0xff {
            return Err(invalid("a JPEG marker is missing"));
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let Some(&marker) = bytes.get(offset) else {
            ended(size, "marker")?;
            break;
        };
        offset += 1;
        match marker {
            0xda if size.is_some() => break,
            0 | 0xd8..=0xda => return Err(invalid("the JPEG ends before its frame header")),
            0x01 | 0xd0..=0xd7 => continue,
            _ => {}
        }
        segments += 1;
        if segments > MAX_JPEG_SEGMENTS {
            return Err(DecodeError::Unsupported(format!(
                "the JPEG has more than {MAX_JPEG_SEGMENTS} segments before its image data"
            )));
        }
        let Some(length_bytes) = bytes.get(offset..offset + 2) else {
            ended(size, "segment length")?;
            break;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err(invalid("invalid JPEG segment length"));
        }
        let end = offset + length;
        let Some(segment) = bytes.get(offset + 2..end) else {
            ended(size, "segment")?;
            break;
        };
        if marker == 0xe1 && orientation.is_none() && segment.starts_with(b"Exif\0\0") {
            orientation = Some(exif::orientation(&segment[6..]));
        }
        if size.is_none()
            && (0xc0..=0xcf).contains(&marker)
            && !matches!(marker, 0xc4 | 0xc8 | 0xcc)
        {
            size = Some(frame_size(marker, segment)?);
        }
        if marker == 0xcc {
            return Err(DecodeError::Unsupported(
                "arithmetic-coded JPEGs are not supported".into(),
            ));
        }
        offset = end;
    }
    let Some((width, height)) = size else {
        return Err(invalid("the JPEG ends before its frame header"));
    };
    let orientation = orientation.unwrap_or(1);
    let (width, height) = oriented_size(width, height, orientation);
    Ok(JpegHeader {
        info: ImageInfo {
            format: ImageFormat::Jpeg,
            width,
            height,
        },
        orientation,
    })
}

/// The stored size from a frame header (SOF) segment.
fn frame_size(marker: u8, segment: &[u8]) -> Result<(u32, u32), DecodeError> {
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
    Ok((width, height))
}

#[cfg(test)]
mod tests;
