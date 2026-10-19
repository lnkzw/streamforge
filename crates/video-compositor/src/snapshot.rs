//! Binary PPM (P6) export of composited frames for visual inspection.

use std::io::{self, Write};

use media_core::{PixelFormat, VideoFrame};

/// Write `frame` as 8-bit RGB PPM. Alpha is dropped; premultiplied pixels
/// therefore appear composited over black. Canvas output is opaque anyway.
pub fn write_ppm(frame: &VideoFrame, out: &mut impl Write) -> io::Result<()> {
    write!(out, "P6\n{} {}\n255\n", frame.width(), frame.height())?;
    let mut rgb = Vec::with_capacity(frame.width() as usize * 3);
    for y in 0..frame.height() {
        rgb.clear();
        for px in frame
            .row(y)
            .as_chunks::<{ PixelFormat::BYTES_PER_PIXEL }>()
            .0
        {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        out.write_all(&rgb)?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use media_core::FrameBuffer;

    use super::*;

    #[test]
    fn ppm_has_header_and_rgb_order() {
        let frame = VideoFrame::new(
            2,
            1,
            8,
            PixelFormat::Bgra8,
            None,
            FrameBuffer::unpooled(vec![1, 2, 3, 255, 4, 5, 6, 255]),
        )
        .expect("frame");
        let mut out = Vec::new();
        write_ppm(&frame, &mut out).expect("write");
        assert_eq!(out, b"P6\n2 1\n255\n\x03\x02\x01\x06\x05\x04");
    }
}
