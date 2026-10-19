//! Deterministic test sources.

use media_core::{PixelFormat, VideoFrame, VideoFrameError};

const BPP: usize = PixelFormat::BYTES_PER_PIXEL;

/// Full-intensity bars in BGRA: white, yellow, cyan, green, magenta, red, blue, black.
pub const BARS: [[u8; 4]; 8] = [
    [255, 255, 255, 255],
    [0, 255, 255, 255],
    [255, 255, 0, 255],
    [0, 255, 0, 255],
    [255, 0, 255, 255],
    [0, 0, 255, 255],
    [255, 0, 0, 255],
    [0, 0, 0, 255],
];

/// Eight vertical color bars over the top three quarters and a horizontal
/// gray ramp (black to white) below; opaque Bgra8.
pub fn color_bars(width: u32, height: u32) -> Result<VideoFrame, VideoFrameError> {
    let mut frame = VideoFrame::zeroed(width, height, PixelFormat::Bgra8)?;
    let stride = frame.stride();
    let bars_end = height - height / 4;
    let data = frame.data_mut();
    for y in 0..height {
        let row = &mut data[y as usize * stride..][..width as usize * BPP];
        for (x, px) in row.as_chunks_mut::<BPP>().0.iter_mut().enumerate() {
            let x = x as u64;
            let color = if y < bars_end {
                BARS[(x * 8 / u64::from(width)) as usize]
            } else {
                let v = (x * 255 / u64::from(width.max(2) - 1)) as u8;
                [v, v, v, 255]
            };
            *px = color;
        }
    }
    Ok(frame)
}

/// Uniform frame; `bgra` must be premultiplied.
pub fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Result<VideoFrame, VideoFrameError> {
    let mut frame = VideoFrame::zeroed(width, height, PixelFormat::Bgra8)?;
    for px in frame.data_mut().as_chunks_mut::<BPP>().0 {
        *px = bgra;
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_and_ramp_are_where_expected() {
        let frame = color_bars(800, 400).expect("pattern");
        for (i, color) in BARS.iter().enumerate() {
            assert_eq!(frame.pixel(i as u32 * 100 + 50, 10), *color, "bar {i}");
        }
        assert_eq!(frame.pixel(0, 399), [0, 0, 0, 255]);
        assert_eq!(frame.pixel(799, 399), [255, 255, 255, 255]);
        let mid = frame.pixel(400, 350)[0];
        assert!((126..=129).contains(&mid), "{mid}");
    }

    #[test]
    fn solid_fills_every_pixel() {
        let frame = solid(3, 2, [1, 2, 3, 4]).expect("solid");
        assert!(
            frame
                .data()
                .as_chunks::<BPP>()
                .0
                .iter()
                .all(|px| *px == [1, 2, 3, 4])
        );
    }
}
