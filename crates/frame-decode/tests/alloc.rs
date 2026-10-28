//! The allocator is confined to this integration-test binary. Production code
//! remains `forbid(unsafe_code)`. Thread-local counters exclude harness activity.

use frame_decode::{MjpegDecoder, Planes, YuvColor, YuvLayout, convert_yuv};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

struct Counting;
thread_local! {
    static COUNTS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}
fn count(bytes: usize) {
    let _ = COUNTS.try_with(|c| {
        if let Some((calls, total)) = c.get() {
            c.set(Some((
                calls.checked_add(1).expect("finite count"),
                total.checked_add(bytes).expect("finite bytes"),
            )));
        }
    });
}
// SAFETY: Each operation forwards the original pointer, layout and size to
// System. Counters neither allocate nor change the allocation's lifetime.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: GlobalAlloc's caller supplies a valid layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: GlobalAlloc's caller supplies a valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Pointer and layout are forwarded unchanged to their allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        // SAFETY: GlobalAlloc's caller guarantees the original allocation and size.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn measured<T>(f: impl FnOnce() -> T) -> (T, (usize, usize)) {
    COUNTS.with(|c| c.set(Some((0, 0))));
    let result = f();
    let counts = COUNTS.with(|c| c.replace(None).unwrap());
    (result, counts)
}

#[test]
fn camera_allocation_counts() {
    let (w, h) = (1920u32, 1080u32);
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
        let data = vec![128; stride * h as usize * 2];
        let planes = Planes::contiguous(layout, &data, stride, h).unwrap();
        let (result, counts) =
            measured(|| convert_yuv(layout, &planes, w, h, YuvColor::default(), &mut dst, 0));
        result.unwrap();
        assert_eq!(counts, (0, 0), "{layout:?} allocations / bytes");
        println!("{layout:?}: {} allocations, {} bytes", counts.0, counts.1);
    }
    let jpeg = include_bytes!("data/camera-1080p-422.jpg");
    let mut stripped = jpeg[..2].to_vec();
    let mut offset = 2;
    while jpeg[offset + 1] != 0xda {
        let n = usize::from(u16::from_be_bytes([jpeg[offset + 2], jpeg[offset + 3]]));
        if jpeg[offset + 1] != 0xc4 {
            stripped.extend_from_slice(&jpeg[offset..offset + 2 + n]);
        }
        offset += 2 + n;
    }
    stripped.extend_from_slice(&jpeg[offset..]);
    for (name, frame) in [
        ("MJPEG", jpeg.as_slice()),
        ("MJPEG missing DHT", stripped.as_slice()),
    ] {
        let mut decoder = MjpegDecoder::new();
        decoder.decode_into(frame, w, h, &mut dst).unwrap(); // warm scratch and decoder code paths
        for i in 1..=3 {
            let (result, (calls, bytes)) = measured(|| decoder.decode_into(frame, w, h, &mut dst));
            result.unwrap();
            // checked_add in the allocator proves the finite counters didn't overflow.
            println!("{name} warm frame {i}: {calls} allocations, {bytes} bytes");
        }
    }
}
