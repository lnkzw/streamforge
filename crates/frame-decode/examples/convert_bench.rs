//! Single-threaded camera conversion at 1920x1080, BT.709 limited range.
//!
//! cargo run --release -p frame-decode --example convert_bench [frames]

use frame_decode::{
    ColorMatrix, ColorRange, MjpegDecoder, Planes, YuvColor, YuvLayout, convert_yuv,
};
use std::{hint::black_box, time::Instant};

fn measure(name: &str, frames: usize, mut convert: impl FnMut()) {
    for _ in 0..10 {
        convert();
    }
    let mut times = Vec::with_capacity(frames);
    for _ in 0..frames {
        let start = Instant::now();
        convert();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / frames as f64;
    let p99 = times[(frames * 99).div_ceil(100) - 1];
    println!("{name:<6} frames={frames} mean={mean:.3} ms p99={p99:.3} ms");
}

fn main() {
    let frames: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(300);
    assert!(frames > 0, "frames must be positive");
    let (w, h) = (1920u32, 1080u32);
    let color = YuvColor {
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
    };
    let mut dst = vec![0; w as usize * h as usize * 4];
    for layout in [
        YuvLayout::Yuy2,
        YuvLayout::Uyvy,
        YuvLayout::Nv12,
        YuvLayout::I420,
    ] {
        let stride = if layout.plane_count() == 1 {
            w as usize * 2
        } else {
            w as usize
        };
        let bytes = if layout.plane_count() == 1 {
            stride * h as usize
        } else {
            stride * h as usize * 3 / 2
        };
        // Exercise the full range; prevent constant propagation through conversion.
        let data: Vec<_> = (0..bytes)
            .map(|i| ((i * 73 + i / 1920 * 19) % 256) as u8)
            .collect();
        let planes = Planes::contiguous(layout, &data, stride, h).expect("planes");
        measure(&format!("{layout:?}"), frames, || {
            convert_yuv(
                layout,
                black_box(&planes),
                w,
                h,
                color,
                black_box(&mut dst),
                w as usize * 4,
            )
            .expect("YUV conversion");
            black_box(&dst);
        });
    }
    let jpeg = include_bytes!("../tests/data/camera-1080p-422.jpg");
    let mut decoder = MjpegDecoder::new();
    measure("MJPEG", frames, || {
        decoder
            .decode_into(black_box(jpeg), w, h, black_box(&mut dst))
            .expect("MJPEG conversion");
        black_box(&dst);
    });
}
