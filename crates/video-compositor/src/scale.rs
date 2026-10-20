//! Integer downscaling for previews.

use media_core::{FrameBuffer, VideoFrame, VideoFrameError};

/// Downscale `src` by `factor` (≥ 1) into `buffer`, averaging four samples
/// per output pixel (at a quarter and three quarters of each source block).
/// Cheap enough for the render thread; meant for previews, not for output.
/// The result keeps the source format and is tightly packed.
pub fn downscale(
    src: &VideoFrame,
    factor: u32,
    buffer: FrameBuffer,
) -> Result<VideoFrame, VideoFrameError> {
    let factor = factor.max(1);
    let (width, height) = (
        (src.width() / factor).max(1),
        (src.height() / factor).max(1),
    );
    let stride = VideoFrame::packed_stride(width)?;
    let mut frame = VideoFrame::new(width, height, stride, src.format(), src.pts(), buffer)?;
    let near = factor / 4;
    let far = (factor * 3 / 4).min(factor - 1);
    let clamp_x = |x: u32| x.min(src.width() - 1);
    let clamp_y = |y: u32| y.min(src.height() - 1);
    for y in 0..height {
        let (y0, y1) = (clamp_y(y * factor + near), clamp_y(y * factor + far));
        let (row0, row1) = (src.row(y0), src.row(y1));
        let out = &mut frame.data_mut()[y as usize * stride..][..width as usize * 4];
        for (x, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = x as u32;
            let (x0, x1) = (
                clamp_x(x * factor + near) as usize * 4,
                clamp_x(x * factor + far) as usize * 4,
            );
            for (channel, value) in pixel.iter_mut().enumerate() {
                let sum = u16::from(row0[x0 + channel])
                    + u16::from(row0[x1 + channel])
                    + u16::from(row1[x0 + channel])
                    + u16::from(row1[x1 + channel]);
                *value = ((sum + 2) / 4) as u8;
            }
        }
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use media_core::PixelFormat;

    use super::*;
    use crate::pattern;

    #[test]
    fn keeps_flat_colors_and_quarters_the_size() {
        let src = pattern::solid(64, 48, [10, 20, 30, 255]).expect("solid");
        let small = downscale(&src, 4, FrameBuffer::unpooled(vec![0; 16 * 12 * 4])).expect("scale");
        assert_eq!((small.width(), small.height()), (16, 12));
        assert_eq!(small.format(), PixelFormat::Bgra8);
        assert_eq!(small.pixel(15, 11), [10, 20, 30, 255]);
    }

    #[test]
    fn averages_across_a_block() {
        // Left half black, right half white: the boundary block mixes both.
        let mut src = VideoFrame::zeroed(8, 4, PixelFormat::Bgra8).expect("frame");
        for y in 0..4 {
            for x in 0..8u32 {
                let value = if x >= 3 { 255 } else { 0 };
                let at = y * src.stride() + x as usize * 4;
                src.data_mut()[at..at + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        let small = downscale(&src, 4, FrameBuffer::unpooled(vec![0; 2 * 4])).expect("scale");
        // Block 0 samples x = 1 and x = 3: half black, half white.
        assert_eq!(small.pixel(0, 0), [128, 128, 128, 255]);
        assert_eq!(small.pixel(1, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn a_too_small_buffer_is_an_error() {
        let src = pattern::solid(64, 48, [0, 0, 0, 255]).expect("solid");
        assert!(downscale(&src, 4, FrameBuffer::unpooled(vec![0; 10])).is_err());
    }
}
