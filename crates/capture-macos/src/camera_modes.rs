//! What an AVFoundation camera offers, independent of the macOS API so it
//! is tested everywhere: the pixel formats StreamForge accepts (by their
//! four-character code), the rates of a format's frame rate ranges with
//! the frame duration that selects each, media times in nanoseconds, and
//! where a locked frame's rows are.
//!
//! AVFoundation turns every accepted format into BGRA, decoding MJPEG on
//! the way, so StreamForge converts nothing itself on macOS (ADR 0019).

use capture_api::{ModeOffer, OfferFormat, Rational, rate_value, reduce_rate};

/// Bound on the formats read from one camera, whatever it reports.
pub(crate) const MAX_FORMATS: usize = 1024;
/// Bound on the modes kept per camera.
const MAX_MODES: usize = 1024;
/// Bound on a frame side, as on Linux and Windows.
const MAX_DIMENSION: u32 = 8192;
/// Rates tried within a rate range, as on Linux.
const RANGE_RATES: [u32; 8] = [60, 50, 30, 25, 24, 15, 10, 5];
/// A range's top rate this close (relative) to a whole or an NTSC rate is
/// listed as that rate: a UVC camera states 30 fps as 333333 × 100 ns,
/// which AVFoundation reports as 30.00003 fps.
const RATE_TOLERANCE: f64 = 0.0005;

/// A camera format that AVFoundation can turn into BGRA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CameraFormat {
    /// `kCVPixelFormatType_32BGRA`.
    Bgra,
    /// `kCVPixelFormatType_32ARGB`.
    Argb,
    /// `kCVPixelFormatType_422YpCbCr8_yuvs`.
    Yuy2,
    /// `kCVPixelFormatType_422YpCbCr8` (`2vuy`).
    Uyvy,
    /// `420v` (video range) or `420f` (full range).
    Nv12,
    /// `y420` (video range) or `f420` (full range).
    I420,
    /// `dmb1` (OpenDML) or `jpeg`.
    Mjpeg,
}

impl CameraFormat {
    /// The format of a media subtype (a four-character code), if StreamForge
    /// accepts it; others are left out, as on Windows.
    pub(crate) fn from_subtype(code: u32) -> Option<Self> {
        Some(match &code.to_be_bytes() {
            b"BGRA" => Self::Bgra,
            // kCVPixelFormatType_32ARGB is the number 32, not a FourCC.
            [0, 0, 0, 32] => Self::Argb,
            b"yuvs" => Self::Yuy2,
            b"2vuy" => Self::Uyvy,
            b"420v" | b"420f" => Self::Nv12,
            b"y420" | b"f420" => Self::I420,
            b"dmb1" | b"jpeg" => Self::Mjpeg,
            _ => return None,
        })
    }

    /// Reported as the format a camera runs in; the names match the Linux
    /// and Windows backends' for the same layout.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bgra => "BGRA",
            Self::Argb => "ARGB",
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
            Self::Bgra | Self::Argb => 0,
            Self::Yuy2 | Self::Uyvy => 1,
            Self::Nv12 => 2,
            Self::I420 => 3,
            Self::Mjpeg => 4,
        }
    }
}

/// A media time as Core Media states it: `value / timescale` seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MediaTime {
    pub value: i64,
    pub timescale: i32,
}

/// Nanoseconds of a media time, if its timescale is valid and it fits.
pub(crate) fn media_time_ns(time: MediaTime) -> Option<i64> {
    if time.timescale <= 0 {
        return None;
    }
    let ns = i128::from(time.value) * 1_000_000_000 / i128::from(time.timescale);
    i64::try_from(ns).ok()
}

/// One of a format's frame rate ranges, as AVFoundation states it: the
/// shortest frame duration is the highest rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RateRange {
    pub min_duration: MediaTime,
    pub max_duration: MediaTime,
}

/// A camera format as AVFoundation lists it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FormatInfo {
    pub subtype: u32,
    pub width: i32,
    pub height: i32,
    pub ranges: Vec<RateRange>,
}

/// One mode of one of the camera's formats, and the frame duration that
/// selects its rate. The duration lies inside a range of that format, so
/// setting it cannot raise an Objective-C exception.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NativeMode {
    /// Index into the device's `formats`.
    pub format_index: usize,
    pub offer: ModeOffer<CameraFormat>,
    pub duration: MediaTime,
}

/// The modes of a camera's formats: each format's size at the rates of its
/// ranges, every mode once (the first format offering it selects it).
/// Formats StreamForge does not accept and sizes above 8192 are left out.
pub(crate) fn native_modes(formats: &[FormatInfo]) -> Vec<NativeMode> {
    let mut modes: Vec<NativeMode> = Vec::new();
    for (format_index, info) in formats.iter().enumerate().take(MAX_FORMATS) {
        let Some(format) = CameraFormat::from_subtype(info.subtype) else {
            continue;
        };
        let (Some(width), Some(height)) = (dimension(info.width), dimension(info.height)) else {
            continue;
        };
        for range in &info.ranges {
            for (fps, duration) in range_rates(*range) {
                let offer = ModeOffer {
                    width,
                    height,
                    fps,
                    format,
                };
                if modes.len() < MAX_MODES && !modes.iter().any(|mode| mode.offer == offer) {
                    modes.push(NativeMode {
                        format_index,
                        offer,
                        duration,
                    });
                }
            }
        }
    }
    modes
}

fn dimension(side: i32) -> Option<u32> {
    u32::try_from(side)
        .ok()
        .filter(|&side| side > 0 && side <= MAX_DIMENSION)
}

/// The rates a range offers, with the frame duration that selects each:
/// its top rate (listed whole or NTSC when it is that close; the duration
/// is the range's own), then the rates of [`RANGE_RATES`] inside it.
pub(crate) fn range_rates(range: RateRange) -> Vec<(Rational, MediaTime)> {
    let RateRange {
        min_duration: shortest,
        max_duration: longest,
    } = range;
    if shortest.value <= 0 || shortest.timescale <= 0 {
        return Vec::new();
    }
    let mut rates = Vec::new();
    if let Some(top) = listed_rate(shortest) {
        rates.push((top, shortest));
    }
    // An unusable longest duration leaves only the top rate.
    if longest.value > 0 && longest.timescale > 0 {
        for rate in RANGE_RATES {
            let duration = MediaTime {
                value: 1,
                timescale: rate as i32,
            };
            let fps = Rational::integer(rate);
            let inside = !shorter(duration, shortest) && !shorter(longest, duration);
            if inside && !rates.iter().any(|(listed, _)| *listed == fps) {
                rates.push((fps, duration));
            }
        }
    }
    rates
}

/// Whether duration `a` is shorter than `b` (both positive).
pub(crate) fn shorter(a: MediaTime, b: MediaTime) -> bool {
    i128::from(a.value) * i128::from(b.timescale) < i128::from(b.value) * i128::from(a.timescale)
}

/// The rate a frame duration lists: whole or NTSC (`n·1000/1001`) when it
/// is that close, else the exact rate if it fits a `Rational`.
pub(crate) fn listed_rate(duration: MediaTime) -> Option<Rational> {
    if duration.value <= 0 || duration.timescale <= 0 {
        return None;
    }
    let exact = f64::from(duration.timescale) / duration.value as f64;
    let whole = exact.round();
    if whole >= 1.0 && ((exact - whole) / exact).abs() < RATE_TOLERANCE {
        return u32::try_from(whole as u64).ok().map(Rational::integer);
    }
    let ntsc = (exact * 1.001).round();
    if ntsc >= 1.0 && ((exact - ntsc * 1000.0 / 1001.0) / exact).abs() < RATE_TOLERANCE {
        let num = u32::try_from(ntsc as u64).ok()?.checked_mul(1000)?;
        return Some(Rational { num, den: 1001 });
    }
    let num = u32::try_from(duration.timescale).ok()?;
    let den = u32::try_from(duration.value).ok()?;
    Some(reduce_rate(Rational { num, den })).filter(|rate| rate_value(*rate) > 0.0)
}

/// A four-character code as text (`420v`), or as hex when not printable.
pub(crate) fn fourcc_text(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        bytes.iter().map(|&byte| char::from(byte)).collect()
    } else {
        format!("{code:#010x}")
    }
}

/// The rows of a locked BGRA frame of `width`×`height` with `stride` bytes
/// per row, each `width * 4` bytes long, or why they do not fit `data`.
pub(crate) fn frame_rows(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
) -> Result<impl Iterator<Item = &[u8]>, String> {
    let row = (width as usize)
        .checked_mul(4)
        .ok_or("the frame is too wide")?;
    if stride < row {
        return Err(format!(
            "the frame's rows hold {stride} bytes, {width} pixels need {row}"
        ));
    }
    let needed = (height as usize)
        .checked_sub(1)
        .map_or(Some(0), |rows| rows.checked_mul(stride)?.checked_add(row))
        .ok_or("the frame is too large")?;
    if data.len() < needed {
        return Err(format!(
            "the frame holds {} bytes, {width}x{height} needs {needed}",
            data.len()
        ));
    }
    Ok((0..height as usize).map(move |index| &data[index * stride..index * stride + row]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn time(value: i64, timescale: i32) -> MediaTime {
        MediaTime { value, timescale }
    }

    fn fourcc(code: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*code)
    }

    fn rates(range: RateRange) -> Vec<Rational> {
        range_rates(range).into_iter().map(|(fps, _)| fps).collect()
    }

    #[test]
    fn accepted_subtypes_map_to_the_shared_names_and_ranks() {
        for (code, name, rank) in [
            (fourcc(b"BGRA"), "BGRA", 0),
            (32, "ARGB", 0),
            (fourcc(b"yuvs"), "YUY2", 1),
            (fourcc(b"2vuy"), "UYVY", 1),
            (fourcc(b"420v"), "NV12", 2),
            (fourcc(b"420f"), "NV12", 2),
            (fourcc(b"y420"), "I420", 3),
            (fourcc(b"f420"), "I420", 3),
            (fourcc(b"dmb1"), "MJPG", 4),
            (fourcc(b"jpeg"), "MJPG", 4),
        ] {
            let format = CameraFormat::from_subtype(code).expect(name);
            assert_eq!((format.name(), format.rank()), (name, rank));
        }
        for unknown in [fourcc(b"x420"), fourcc(b"hvc1"), fourcc(b"avc1"), 0] {
            assert_eq!(CameraFormat::from_subtype(unknown), None, "{unknown:#x}");
        }
    }

    #[test]
    fn a_fixed_range_lists_its_rate_with_its_own_duration() {
        let uvc = time(333_333, 10_000_000);
        assert_eq!(
            range_rates(RateRange {
                min_duration: uvc,
                max_duration: uvc
            }),
            [(Rational::integer(30), uvc)]
        );
        let ntsc = time(1001, 30000);
        assert_eq!(
            range_rates(RateRange {
                min_duration: ntsc,
                max_duration: ntsc
            }),
            [(
                Rational {
                    num: 30000,
                    den: 1001
                },
                ntsc
            )]
        );
        // 29.97 stated in hundredths is NTSC too; 30 and 29.97 stay apart.
        let rounded = time(100, 2997);
        assert_eq!(
            rates(RateRange {
                min_duration: rounded,
                max_duration: rounded
            }),
            [Rational {
                num: 30000,
                den: 1001
            }]
        );
        // A rate near neither stays exact.
        let odd = time(2, 55);
        assert_eq!(
            rates(RateRange {
                min_duration: odd,
                max_duration: odd
            }),
            [Rational { num: 55, den: 2 }]
        );
    }

    #[test]
    fn a_wide_range_adds_the_common_rates_inside_it() {
        let range = RateRange {
            min_duration: time(1, 30),
            max_duration: time(1, 1),
        };
        assert_eq!(
            rates(range),
            [30, 25, 24, 15, 10, 5].map(Rational::integer).to_vec()
        );
        // Each added rate is selected by its own exact duration.
        assert!(
            range_rates(range)[1..]
                .iter()
                .all(|(fps, duration)| *duration == time(1, fps.num as i32))
        );
        // The ends count as inside; rates above the top are left out.
        let narrow = RateRange {
            min_duration: time(1, 24),
            max_duration: time(1, 10),
        };
        assert_eq!(rates(narrow), [24, 15, 10].map(Rational::integer).to_vec());
    }

    #[test]
    fn invalid_ranges_offer_nothing_or_only_their_top() {
        let empty = RateRange {
            min_duration: time(0, 30),
            max_duration: time(1, 1),
        };
        assert!(range_rates(empty).is_empty());
        let no_scale = RateRange {
            min_duration: time(1, 0),
            max_duration: time(1, 1),
        };
        assert!(range_rates(no_scale).is_empty());
        let top_only = RateRange {
            min_duration: time(1, 30),
            max_duration: time(1, -1),
        };
        assert_eq!(rates(top_only), [Rational::integer(30)]);
    }

    fn format(subtype: &[u8; 4], width: i32, height: i32, fps: i32) -> FormatInfo {
        FormatInfo {
            subtype: fourcc(subtype),
            width,
            height,
            ranges: vec![RateRange {
                min_duration: time(1, fps),
                max_duration: time(1, fps),
            }],
        }
    }

    #[test]
    fn native_modes_keep_the_first_format_of_each_mode_and_skip_the_rest() {
        let formats = [
            format(b"420v", 1920, 1080, 30),
            // Same mode in full range: the video-range format selects it.
            format(b"420f", 1920, 1080, 30),
            format(b"hvc1", 1920, 1080, 30),
            format(b"dmb1", 1920, 1080, 30),
            format(b"yuvs", 9000, 4000, 30),
            format(b"yuvs", 0, 480, 30),
            format(b"yuvs", 640, 480, 30),
        ];
        let modes = native_modes(&formats);
        let summary: Vec<_> = modes
            .iter()
            .map(|mode| {
                (
                    mode.format_index,
                    mode.offer.format.name(),
                    mode.offer.width,
                    mode.offer.height,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (0, "NV12", 1920, 1080),
                (3, "MJPG", 1920, 1080),
                (6, "YUY2", 640, 480),
            ]
        );
        assert!(modes.iter().all(|mode| mode.offer.fps == Rational::integer(30)
            && mode.duration == time(1, 30)));
    }

    #[test]
    fn four_character_codes_read_as_text_or_hex() {
        assert_eq!(fourcc_text(fourcc(b"420v")), "420v");
        assert_eq!(fourcc_text(32), "0x00000020");
    }

    #[test]
    fn media_times_convert_to_nanoseconds_when_valid() {
        assert_eq!(media_time_ns(time(3, 30)), Some(100_000_000));
        assert_eq!(media_time_ns(time(-1, 1000)), Some(-1_000_000));
        assert_eq!(media_time_ns(time(1, 0)), None);
        assert_eq!(media_time_ns(time(i64::MAX, 1)), None);
    }

    #[test]
    fn frame_rows_skip_padding_and_check_the_buffer() {
        // Two rows of 2 pixels with 4 bytes of padding; the last row unpadded.
        let data: Vec<u8> = (0..20).collect();
        let rows: Vec<&[u8]> = frame_rows(&data, 2, 2, 12).expect("rows").collect();
        assert_eq!(rows, [&data[..8], &data[12..20]]);
        assert!(frame_rows(&data, 2, 2, 7).is_err(), "stride below a row");
        assert!(frame_rows(&data[..19], 2, 2, 12).is_err(), "short buffer");
        assert_eq!(frame_rows(&[], 2, 0, 8).expect("no rows").count(), 0);
    }
}
