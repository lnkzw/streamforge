//! What a Media Foundation camera offers, independent of the Windows API so
//! it is tested everywhere: the video subtypes StreamForge accepts, the
//! packed size and rate attributes, and where a locked frame's rows are.
//!
//! Media Foundation's video processing turns every accepted subtype into
//! RGB32 (BGRX in memory), decoding MJPEG on the way, so StreamForge
//! converts nothing itself on Windows (ADR 0019).

use capture_api::{ModeOffer, OfferFormat, Rational, UNSTATED_RATE, reduce_rate};
use media_core::VideoFrame;

/// The tail every FourCC or D3DFORMAT video subtype GUID shares:
/// `XXXXXXXX-0000-0010-8000-00AA00389B71`.
const SUBTYPE_BASE: u128 = 0x0000_0010_8000_00aa_0038_9b71;

/// Bound on the native types read from one camera, whatever it reports.
pub(crate) const MAX_NATIVE_TYPES: u32 = 1024;
/// Bound on a frame side, as on Linux.
const MAX_DIMENSION: u32 = 8192;

/// A camera subtype that the source reader can turn into RGB32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CameraFormat {
    /// `MFVideoFormat_RGB32` (BGRX in memory).
    Rgb32,
    /// `MFVideoFormat_ARGB32` (BGRA in memory).
    Argb32,
    /// `MFVideoFormat_RGB24` (BGR in memory).
    Rgb24,
    Yuy2,
    Uyvy,
    Nv12,
    /// `MFVideoFormat_I420` or its twin `MFVideoFormat_IYUV`.
    I420,
    Mjpeg,
}

impl CameraFormat {
    /// The format of a subtype GUID (as `u128`), if StreamForge accepts it.
    pub(crate) fn from_subtype(subtype: u128) -> Option<Self> {
        if subtype & ((1u128 << 96) - 1) != SUBTYPE_BASE {
            return None;
        }
        let code = (subtype >> 96) as u32;
        Some(match code {
            22 => Self::Rgb32,
            21 => Self::Argb32,
            20 => Self::Rgb24,
            _ => match &code.to_le_bytes() {
                b"YUY2" => Self::Yuy2,
                b"UYVY" => Self::Uyvy,
                b"NV12" => Self::Nv12,
                b"I420" | b"IYUV" => Self::I420,
                b"MJPG" => Self::Mjpeg,
                _ => return None,
            },
        })
    }

    /// Reported as the format a camera runs in; the names match the Linux
    /// backend's for the same layout.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Rgb32 => "BGRx",
            Self::Argb32 => "BGRA",
            Self::Rgb24 => "BGR",
            Self::Yuy2 => "YUY2",
            Self::Uyvy => "UYVY",
            Self::Nv12 => "NV12",
            Self::I420 => "I420",
            Self::Mjpeg => "MJPG",
        }
    }
}

impl OfferFormat for CameraFormat {
    fn rank(self) -> u8 {
        match self {
            Self::Rgb32 | Self::Argb32 | Self::Rgb24 => 0,
            Self::Yuy2 | Self::Uyvy => 1,
            Self::Nv12 => 2,
            Self::I420 => 3,
            Self::Mjpeg => 4,
        }
    }
}

/// `MF_MT_FRAME_SIZE`: width in the high 32 bits, height in the low.
pub(crate) fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

#[cfg(test)]
fn pack_size((width, height): (u32, u32)) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

/// `MF_MT_FRAME_RATE`: numerator in the high 32 bits, denominator in the
/// low; a zero part means the camera states no rate.
pub(crate) fn unpack_rate(packed: Option<u64>) -> Rational {
    let Some(packed) = packed else {
        return UNSTATED_RATE;
    };
    let (num, den) = unpack_size(packed);
    if num == 0 || den == 0 {
        UNSTATED_RATE
    } else {
        reduce_rate(Rational { num, den })
    }
}

/// A native media type as a mode, if StreamForge accepts its subtype and
/// size. `rate` is the packed `MF_MT_FRAME_RATE`, if the type has one.
pub(crate) fn offer(
    subtype: u128,
    size: u64,
    rate: Option<u64>,
) -> Option<ModeOffer<CameraFormat>> {
    let format = CameraFormat::from_subtype(subtype)?;
    let (width, height) = unpack_size(size);
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return None;
    }
    Some(ModeOffer {
        width,
        height,
        fps: unpack_rate(rate),
        format,
    })
}

/// Where the rows of a locked RGB32 frame are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowSpan {
    /// Bytes from one row to the next.
    pub stride: usize,
    /// Bytes from the lowest row's start to the end of the highest row's
    /// pixels.
    pub len: usize,
    /// Bytes from the locked pointer back to the lowest row.
    pub back: usize,
    /// The lowest row in memory is the bottom one.
    pub bottom_up: bool,
}

impl RowSpan {
    /// The address of the lowest row, if every row lies inside the locked
    /// buffer of `buffer_len` bytes at `buffer_start`; `pointer` is the
    /// locked pointer (the top row of a 2D lock).
    pub(crate) fn lowest_row(
        &self,
        pointer: usize,
        buffer_start: usize,
        buffer_len: usize,
    ) -> Option<usize> {
        let lowest = pointer.checked_sub(self.back)?;
        let end = lowest.checked_add(self.len)?;
        (lowest >= buffer_start && end <= buffer_start.checked_add(buffer_len)?).then_some(lowest)
    }
}

fn span(width: u32, height: u32, pitch: i64) -> Result<(usize, usize), String> {
    let stride = usize::try_from(pitch.unsigned_abs())
        .map_err(|_| format!("a row pitch of {pitch} bytes does not fit in memory"))?;
    let len = VideoFrame::required_len(width, height, stride).map_err(|err| err.to_string())?;
    Ok((stride, len))
}

/// A 2D buffer locked with `Lock2D`: `pitch` from its first (top) row,
/// negative when the frame is stored bottom-up.
pub(crate) fn locked_2d(width: u32, height: u32, pitch: i32) -> Result<RowSpan, String> {
    let (stride, len) = span(width, height, i64::from(pitch))?;
    let bottom_up = pitch < 0;
    Ok(RowSpan {
        stride,
        len,
        back: if bottom_up {
            len - VideoFrame::packed_stride(width).map_err(|err| err.to_string())?
        } else {
            0
        },
        bottom_up,
    })
}

/// A buffer locked with `Lock`: its bytes start with the lowest row;
/// `default_stride` is the media type's `MF_MT_DEFAULT_STRIDE`, negative
/// for a bottom-up frame, resolved from the subtype when the attribute
/// is absent. `available` is the locked length.
pub(crate) fn locked_contiguous(
    width: u32,
    height: u32,
    default_stride: i32,
    available: usize,
) -> Result<RowSpan, String> {
    let pitch = i64::from(default_stride);
    let (stride, len) = span(width, height, pitch)?;
    if available < len {
        return Err(format!(
            "the locked frame holds {available} bytes, {width}x{height} needs {len}"
        ));
    }
    Ok(RowSpan {
        stride,
        len,
        back: 0,
        bottom_up: pitch < 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subtype(code: &[u8; 4]) -> u128 {
        (u128::from(u32::from_le_bytes(*code)) << 96) | SUBTYPE_BASE
    }

    #[test]
    fn subtypes_are_recognised_by_fourcc_and_d3d_format() {
        // MFVideoFormat_RGB32, MFVideoFormat_MJPG and MFVideoFormat_IYUV as
        // the Windows SDK spells them.
        assert_eq!(
            CameraFormat::from_subtype(0x00000016_0000_0010_8000_00aa00389b71),
            Some(CameraFormat::Rgb32)
        );
        assert_eq!(
            CameraFormat::from_subtype(0x47504a4d_0000_0010_8000_00aa00389b71),
            Some(CameraFormat::Mjpeg)
        );
        assert_eq!(
            CameraFormat::from_subtype(0x56555949_0000_0010_8000_00aa00389b71),
            Some(CameraFormat::I420)
        );
        for (code, format) in [
            (b"YUY2", CameraFormat::Yuy2),
            (b"UYVY", CameraFormat::Uyvy),
            (b"NV12", CameraFormat::Nv12),
            (b"I420", CameraFormat::I420),
        ] {
            assert_eq!(CameraFormat::from_subtype(subtype(code)), Some(format));
        }
        assert_eq!(
            CameraFormat::from_subtype((21u128 << 96) | SUBTYPE_BASE),
            Some(CameraFormat::Argb32)
        );
        assert_eq!(
            CameraFormat::from_subtype((20u128 << 96) | SUBTYPE_BASE),
            Some(CameraFormat::Rgb24)
        );
        // H.264 and a GUID with another tail are not accepted.
        assert_eq!(CameraFormat::from_subtype(subtype(b"H264")), None);
        assert_eq!(CameraFormat::from_subtype(subtype(b"YUY2") ^ 1), None);
    }

    #[test]
    fn formats_rank_as_on_linux() {
        let mut formats = [
            CameraFormat::Mjpeg,
            CameraFormat::I420,
            CameraFormat::Nv12,
            CameraFormat::Yuy2,
            CameraFormat::Rgb32,
        ];
        formats.sort_by_key(|format| format.rank());
        assert_eq!(
            formats.map(CameraFormat::name),
            ["BGRx", "YUY2", "NV12", "I420", "MJPG"]
        );
        assert_eq!(CameraFormat::Uyvy.rank(), CameraFormat::Yuy2.rank());
    }

    #[test]
    fn sizes_and_rates_unpack_and_reduce() {
        assert_eq!(unpack_size(pack_size((1920, 1080))), (1920, 1080));
        let ntsc = Rational {
            num: 30000,
            den: 1001,
        };
        assert_eq!(unpack_rate(Some(pack_size((30000, 1001)))), ntsc);
        assert_eq!(
            unpack_rate(Some(pack_size((60, 2)))),
            Rational { num: 30, den: 1 }
        );
        assert_eq!(unpack_rate(Some(pack_size((0, 1)))), UNSTATED_RATE);
        assert_eq!(unpack_rate(Some(pack_size((30, 0)))), UNSTATED_RATE);
        assert_eq!(unpack_rate(None), UNSTATED_RATE);
    }

    #[test]
    fn native_types_become_offers() {
        let offer = offer(
            subtype(b"NV12"),
            pack_size((1280, 720)),
            Some(pack_size((30, 1))),
        )
        .expect("offer");
        assert_eq!(
            (offer.width, offer.height, offer.fps, offer.format),
            (1280, 720, Rational { num: 30, den: 1 }, CameraFormat::Nv12)
        );
        assert_eq!(offer.format.name(), "NV12");
        assert_eq!(
            super::offer(subtype(b"MJPG"), pack_size((640, 480)), None).map(|o| o.fps),
            Some(UNSTATED_RATE)
        );
        assert_eq!(
            super::offer(subtype(b"H264"), pack_size((640, 480)), None),
            None
        );
        assert_eq!(
            super::offer(subtype(b"YUY2"), pack_size((0, 480)), None),
            None
        );
        assert_eq!(
            super::offer(subtype(b"YUY2"), pack_size((8193, 480)), None),
            None
        );
    }

    #[test]
    fn locked_2d_frames_find_their_lowest_row() {
        assert_eq!(
            locked_2d(2, 3, 12),
            Ok(RowSpan {
                stride: 12,
                len: 32,
                back: 0,
                bottom_up: false
            })
        );
        // Bottom-up: the top row (where the pointer is) is the highest.
        assert_eq!(
            locked_2d(2, 3, -12),
            Ok(RowSpan {
                stride: 12,
                len: 32,
                back: 24,
                bottom_up: true
            })
        );
        assert_eq!(locked_2d(1, 1, 4).map(|span| span.len), Ok(4));
        assert!(locked_2d(4, 2, 12).unwrap_err().contains("smaller"));
        assert!(locked_2d(0, 2, 12).unwrap_err().contains("non-zero"));
        assert!(locked_2d(2, 0, 12).unwrap_err().contains("non-zero"));
    }

    #[test]
    fn rows_must_lie_inside_the_locked_buffer() {
        let top_down = locked_2d(2, 3, 12).expect("span");
        assert_eq!(top_down.lowest_row(1000, 1000, 32), Some(1000));
        assert_eq!(top_down.lowest_row(1000, 1000, 31), None);
        assert_eq!(top_down.lowest_row(1000, 1004, 64), None);
        let bottom_up = locked_2d(2, 3, -12).expect("span");
        // The pointer is the top row, 24 bytes above the bottom one.
        assert_eq!(bottom_up.lowest_row(1024, 1000, 32), Some(1000));
        assert_eq!(bottom_up.lowest_row(1020, 1000, 32), None);
        assert_eq!(bottom_up.lowest_row(10, 0, 64), None);
        assert_eq!(top_down.lowest_row(usize::MAX - 8, 0, usize::MAX), None);
    }

    #[test]
    fn contiguous_frames_follow_the_default_stride() {
        assert_eq!(
            locked_contiguous(2, 2, 8, 16),
            Ok(RowSpan {
                stride: 8,
                len: 16,
                back: 0,
                bottom_up: false
            })
        );
        assert_eq!(
            locked_contiguous(2, 2, -8, 16),
            Ok(RowSpan {
                stride: 8,
                len: 16,
                back: 0,
                bottom_up: true
            })
        );
        assert_eq!(locked_contiguous(2, 2, 12, 20).map(|s| s.len), Ok(20));
        assert!(
            locked_contiguous(2, 2, 12, 19)
                .unwrap_err()
                .contains("holds 19 bytes")
        );
        assert!(locked_contiguous(2, 2, 4, 64).is_err());
    }
}
