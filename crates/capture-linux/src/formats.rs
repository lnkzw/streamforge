//! SPA format negotiation helpers and name mappings.

use std::io::Cursor;

use capture_api::{AudioFormatInfo, Rational, VideoFormatInfo};
use frame_decode::{ColorMatrix, ColorRange, YuvColor, YuvLayout};
use media_core::PixelFormat;
use pipewire::spa;
use spa::param::ParamType;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::deserialize::PodDeserializer;
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

/// CPU-readable packed RGB formats offered to screen sources, in preference order.
const VIDEO_FORMATS: [(VideoFormat, &str); 8] = [
    (VideoFormat::BGRx, "BGRx"),
    (VideoFormat::BGRA, "BGRA"),
    (VideoFormat::RGBx, "RGBx"),
    (VideoFormat::RGBA, "RGBA"),
    (VideoFormat::xRGB, "xRGB"),
    (VideoFormat::ARGB, "ARGB"),
    (VideoFormat::xBGR, "xBGR"),
    (VideoFormat::ABGR, "ABGR"),
];

/// Names of the other raw formats a camera may deliver.
const CAMERA_FORMAT_NAMES: [(VideoFormat, &str); 6] = [
    (VideoFormat::RGB, "RGB"),
    (VideoFormat::BGR, "BGR"),
    (VideoFormat::YUY2, "YUY2"),
    (VideoFormat::UYVY, "UYVY"),
    (VideoFormat::NV12, "NV12"),
    (VideoFormat::I420, "I420"),
];

/// How a negotiated MJPEG format is reported.
pub(crate) const MJPEG_NAME: &str = "MJPG";

pub(crate) const MAX_DIMENSION: u32 = 8192;
const MAX_FRAMERATE: u32 = 360;

pub(crate) fn video_format_name(format: VideoFormat) -> String {
    VIDEO_FORMATS
        .iter()
        .chain(&CAMERA_FORMAT_NAMES)
        .find(|(known, _)| known.as_raw() == format.as_raw())
        .map_or_else(
            || format!("spa:{}", format.as_raw()),
            |(_, name)| (*name).to_owned(),
        )
}

pub(crate) fn memory_name(data_type: spa::buffer::DataType) -> &'static str {
    use spa::buffer::DataType;
    match data_type.as_raw() {
        raw if raw == DataType::MemFd.as_raw() => "memfd",
        raw if raw == DataType::MemPtr.as_raw() => "memptr",
        raw if raw == DataType::DmaBuf.as_raw() => "dmabuf",
        _ => "other",
    }
}

/// `EnumFormat` for raw video. No DMA-BUF modifiers are offered, so producers
/// fall back to shared-memory buffers the CPU can map.
pub(crate) fn video_enum_format(preferred: Option<(u32, u32)>) -> Result<Vec<u8>, String> {
    let (width, height) = preferred.unwrap_or((1920, 1080));
    let id = |value: u32| Id(value);
    serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                FormatProperties::MediaType.as_raw(),
                Value::Id(id(MediaType::Video.as_raw())),
            ),
            Property::new(
                FormatProperties::MediaSubtype.as_raw(),
                Value::Id(id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                FormatProperties::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: id(VIDEO_FORMATS[0].0.as_raw()),
                        alternatives: VIDEO_FORMATS.iter().map(|(f, _)| id(f.as_raw())).collect(),
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle { width, height },
                        min: Rectangle {
                            width: 1,
                            height: 1,
                        },
                        max: Rectangle {
                            width: MAX_DIMENSION,
                            height: MAX_DIMENSION,
                        },
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: 60, denom: 1 },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction {
                            num: MAX_FRAMERATE,
                            denom: 1,
                        },
                    },
                ))),
            ),
        ],
    })
}

/// Ask the producer to attach `spa_meta_header` (PTS, sequence, flags) to buffers.
pub(crate) fn header_meta_param() -> Result<Vec<u8>, String> {
    // 32 bytes; the cast cannot truncate.
    let size = std::mem::size_of::<spa::sys::spa_meta_header>() as i32;
    serialize(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(spa::sys::SPA_PARAM_META_size, Value::Int(size)),
        ],
    })
}

/// `EnumFormat` for interleaved 32-bit float audio: at the source's native
/// rate and channel count, or at a fixed `(rate, channels)` (1 or 2) that
/// PipeWire converts to.
pub(crate) fn audio_enum_format(fixed: Option<(u32, u32)>) -> Result<Vec<u8>, String> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    if let Some((rate, channels)) = fixed {
        info.set_rate(rate);
        info.set_channels(channels);
        let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
        match channels {
            1 => position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO,
            2 => {
                position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
                position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
            }
            _ => return Err(format!("unsupported channel count {channels}")),
        }
        info.set_position(position);
    }
    serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    })
}

/// Byte positions of B, G, R (and A) inside one 4-byte pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelLayout {
    b: usize,
    g: usize,
    r: usize,
    a: Option<usize>,
}

const BGRX: PixelLayout = PixelLayout {
    b: 0,
    g: 1,
    r: 2,
    a: None,
};

impl PixelLayout {
    /// Format of frames produced by [`convert_frame`].
    pub fn target_format(self) -> PixelFormat {
        if self.a.is_some() {
            PixelFormat::Bgra8
        } else {
            PixelFormat::Bgrx8
        }
    }
}

/// Layout of the packed formats offered in [`video_enum_format`].
pub(crate) fn pixel_layout(format: VideoFormat) -> Option<PixelLayout> {
    let layout = |b, g, r, a| Some(PixelLayout { b, g, r, a });
    match format.as_raw() {
        raw if raw == VideoFormat::BGRx.as_raw() => Some(BGRX),
        raw if raw == VideoFormat::BGRA.as_raw() => layout(0, 1, 2, Some(3)),
        raw if raw == VideoFormat::RGBx.as_raw() => layout(2, 1, 0, None),
        raw if raw == VideoFormat::RGBA.as_raw() => layout(2, 1, 0, Some(3)),
        raw if raw == VideoFormat::xRGB.as_raw() => layout(3, 2, 1, None),
        raw if raw == VideoFormat::ARGB.as_raw() => layout(3, 2, 1, Some(0)),
        raw if raw == VideoFormat::xBGR.as_raw() => layout(1, 2, 3, None),
        raw if raw == VideoFormat::ABGR.as_raw() => layout(1, 2, 3, Some(0)),
        _ => None,
    }
}

/// Byte positions of B, G and R inside one 3-byte pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rgb24Layout {
    b: usize,
    g: usize,
    r: usize,
}

/// Layout of the 24-bit packed formats a camera may deliver.
pub(crate) fn rgb24_layout(format: VideoFormat) -> Option<Rgb24Layout> {
    match format.as_raw() {
        raw if raw == VideoFormat::RGB.as_raw() => Some(Rgb24Layout { b: 2, g: 1, r: 0 }),
        raw if raw == VideoFormat::BGR.as_raw() => Some(Rgb24Layout { b: 0, g: 1, r: 2 }),
        _ => None,
    }
}

/// The YUV layouts `frame-decode` converts.
pub(crate) fn yuv_layout(format: VideoFormat) -> Option<YuvLayout> {
    match format.as_raw() {
        raw if raw == VideoFormat::YUY2.as_raw() => Some(YuvLayout::Yuy2),
        raw if raw == VideoFormat::UYVY.as_raw() => Some(YuvLayout::Uyvy),
        raw if raw == VideoFormat::NV12.as_raw() => Some(YuvLayout::Nv12),
        raw if raw == VideoFormat::I420.as_raw() => Some(YuvLayout::I420),
        _ => None,
    }
}

/// How a negotiated format's frames become Bgrx8 (or premultiplied Bgra8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameLayout {
    Packed(PixelLayout),
    Rgb24(Rgb24Layout),
    Yuv(YuvLayout, YuvColor),
    Mjpeg,
}

impl FrameLayout {
    /// Format of the frames delivered for this layout.
    pub fn target_format(self) -> PixelFormat {
        match self {
            Self::Packed(layout) => layout.target_format(),
            Self::Rgb24(_) | Self::Yuv(..) | Self::Mjpeg => PixelFormat::Bgrx8,
        }
    }
}

/// Copy a `width` x `height` 24-bit RGB image whose rows are `src_stride`
/// bytes apart into the tightly packed `dst` as Bgrx8. Returns `false` if
/// a buffer is too small. Does not allocate.
pub(crate) fn convert_rgb24(
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
    layout: Rgb24Layout,
    dst: &mut [u8],
) -> bool {
    let row = width * PixelFormat::BYTES_PER_PIXEL;
    let src_row = width * 3;
    if height == 0 || src_stride < src_row || dst.len() < row * height {
        return false;
    }
    if src.len() < src_stride * (height - 1) + src_row {
        return false;
    }
    for (y, out) in dst.chunks_exact_mut(row).take(height).enumerate() {
        let input = &src[y * src_stride..][..src_row];
        for (o, i) in pixels_mut(out).iter_mut().zip(input.as_chunks::<3>().0) {
            *o = [i[layout.b], i[layout.g], i[layout.r], 255];
        }
    }
    true
}

/// Copy a `width` x `height` image whose rows are `src_stride` bytes apart
/// into the tightly packed `dst` as Bgrx8, or as premultiplied Bgra8 when the
/// source has alpha (SPA alpha is straight). Returns `false` if a buffer is
/// too small. Does not allocate.
pub(crate) fn convert_frame(
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
    layout: PixelLayout,
    dst: &mut [u8],
) -> bool {
    let row = width * PixelFormat::BYTES_PER_PIXEL;
    if height == 0 || src_stride < row || dst.len() < row * height {
        return false;
    }
    if src.len() < src_stride * (height - 1) + row {
        return false;
    }
    for (y, out) in dst.chunks_exact_mut(row).take(height).enumerate() {
        let input = &src[y * src_stride..][..row];
        match layout.a {
            None if layout == BGRX => out.copy_from_slice(input),
            None => {
                for (o, i) in pixels_mut(out).iter_mut().zip(pixels(input)) {
                    *o = [i[layout.b], i[layout.g], i[layout.r], 255];
                }
            }
            Some(a) => {
                for (o, i) in pixels_mut(out).iter_mut().zip(pixels(input)) {
                    let alpha = u32::from(i[a]);
                    let premultiply = |c: u8| {
                        let x = u32::from(c) * alpha + 128;
                        ((x + (x >> 8)) >> 8) as u8
                    };
                    *o = [
                        premultiply(i[layout.b]),
                        premultiply(i[layout.g]),
                        premultiply(i[layout.r]),
                        i[a],
                    ];
                }
            }
        }
    }
    true
}

fn pixels(bytes: &[u8]) -> &[[u8; 4]] {
    bytes.as_chunks::<{ PixelFormat::BYTES_PER_PIXEL }>().0
}

fn pixels_mut(bytes: &mut [u8]) -> &mut [[u8; 4]] {
    bytes.as_chunks_mut::<{ PixelFormat::BYTES_PER_PIXEL }>().0
}

/// Negotiated video format, and how its frames are converted (`None` for a
/// format that cannot be).
pub(crate) fn parse_frame_format(
    param: &Pod,
) -> Result<(VideoFormatInfo, Option<FrameLayout>), String> {
    let (media_type, media_subtype) = format_utils::parse_format(param)
        .map_err(|err| format!("cannot parse negotiated format: {err}"))?;
    if media_type != MediaType::Video {
        return Err("producer negotiated a non-video format".to_owned());
    }
    if media_subtype == MediaSubtype::Mjpg {
        let (width, height, framerate) = fixed_size_and_rate(param)
            .ok_or("cannot parse the negotiated MJPEG size and frame rate")?;
        let format = VideoFormatInfo {
            pixel_format: MJPEG_NAME.to_owned(),
            width,
            height,
            framerate,
        };
        return Ok((format, Some(FrameLayout::Mjpeg)));
    }
    if media_subtype != MediaSubtype::Raw {
        return Err("producer negotiated a video format other than raw or MJPEG".to_owned());
    }
    let mut info = VideoInfoRaw::new();
    info.parse(param)
        .map_err(|err| format!("cannot parse raw video format: {err}"))?;
    let format = VideoFormatInfo {
        pixel_format: video_format_name(info.format()),
        width: info.size().width,
        height: info.size().height,
        framerate: Rational {
            num: info.framerate().num,
            den: info.framerate().denom,
        },
    };
    let layout = pixel_layout(info.format())
        .map(FrameLayout::Packed)
        .or_else(|| rgb24_layout(info.format()).map(FrameLayout::Rgb24))
        .or_else(|| {
            yuv_layout(info.format()).map(|layout| FrameLayout::Yuv(layout, yuv_color(&info)))
        });
    Ok((format, layout))
}

/// The colour matrix and range a raw format states; BT.601 limited range
/// when it does not say (ADR 0019). BT.2020 and other matrices also fall
/// back to BT.601.
fn yuv_color(info: &VideoInfoRaw) -> YuvColor {
    YuvColor {
        matrix: if info.color_matrix() == spa::sys::SPA_VIDEO_COLOR_MATRIX_BT709 {
            ColorMatrix::Bt709
        } else {
            ColorMatrix::Bt601
        },
        range: if info.color_range() == spa::sys::SPA_VIDEO_COLOR_RANGE_0_255 {
            ColorRange::Full
        } else {
            ColorRange::Limited
        },
    }
}

/// Size and rate of a negotiated (fixed) format pod.
fn fixed_size_and_rate(param: &Pod) -> Option<(u32, u32, Rational)> {
    let Ok((_, Value::Object(object))) = PodDeserializer::deserialize_any_from(param.as_bytes())
    else {
        return None;
    };
    let mut size = None;
    let mut rate = None;
    for property in object.properties {
        match (property.key, property.value) {
            (key, Value::Rectangle(r)) if key == FormatProperties::VideoSize.as_raw() => {
                size = Some((r.width, r.height));
            }
            (key, Value::Choice(ChoiceValue::Rectangle(Choice(_, ChoiceEnum::None(r)))))
                if key == FormatProperties::VideoSize.as_raw() =>
            {
                size = Some((r.width, r.height));
            }
            (key, Value::Fraction(f)) if key == FormatProperties::VideoFramerate.as_raw() => {
                rate = Some(f);
            }
            (key, Value::Choice(ChoiceValue::Fraction(Choice(_, ChoiceEnum::None(f)))))
                if key == FormatProperties::VideoFramerate.as_raw() =>
            {
                rate = Some(f);
            }
            _ => {}
        }
    }
    let (width, height) = size?;
    // A format without a rate is reported as variable (0/1).
    let rate = rate.unwrap_or(Fraction { num: 0, denom: 1 });
    Some((
        width,
        height,
        Rational {
            num: rate.num,
            den: rate.denom,
        },
    ))
}

pub(crate) fn parse_audio_format(param: &Pod) -> Result<AudioFormatInfo, String> {
    let (media_type, media_subtype) = format_utils::parse_format(param)
        .map_err(|err| format!("cannot parse negotiated format: {err}"))?;
    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
        return Err("producer negotiated a non-raw audio format".to_owned());
    }
    let mut info = AudioInfoRaw::new();
    info.parse(param)
        .map_err(|err| format!("cannot parse raw audio format: {err}"))?;
    if info.format() != AudioFormat::F32LE {
        return Err(format!(
            "producer negotiated {:?} instead of F32LE",
            info.format()
        ));
    }
    Ok(AudioFormatInfo {
        sample_format: "f32le".to_owned(),
        sample_rate_hz: info.rate(),
        channels: info.channels(),
    })
}

pub(crate) fn serialize(object: Object) -> Result<Vec<u8>, String> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|err| format!("cannot serialize SPA pod: {err:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_video_formats_have_names() {
        assert_eq!(video_format_name(VideoFormat::BGRx), "BGRx");
        assert_eq!(video_format_name(VideoFormat::ABGR), "ABGR");
        assert_eq!(video_format_name(VideoFormat::NV12), "NV12");
        assert_eq!(
            video_format_name(VideoFormat::YV12),
            format!("spa:{}", VideoFormat::YV12.as_raw())
        );
    }

    #[test]
    fn negotiation_pods_serialize() {
        for bytes in [
            video_enum_format(Some((2560, 1440))),
            video_enum_format(None),
            header_meta_param(),
            audio_enum_format(None),
            audio_enum_format(Some((48_000, 2))),
            audio_enum_format(Some((44_100, 1))),
        ] {
            let bytes = bytes.expect("serialize");
            assert!(Pod::from_bytes(&bytes).is_some(), "not a valid pod");
        }
    }

    #[test]
    fn every_offered_format_has_a_layout() {
        for (format, name) in VIDEO_FORMATS {
            assert!(pixel_layout(format).is_some(), "{name}");
        }
        assert_eq!(pixel_layout(VideoFormat::NV12), None);
    }

    #[test]
    fn bgrx_rows_are_copied_and_padding_dropped() {
        // 1x2 image, source stride 8 (4 bytes padding per row).
        let src = [1, 2, 3, 4, 9, 9, 9, 9, 5, 6, 7, 8];
        let mut dst = [0u8; 8];
        assert!(convert_frame(&src, 8, 1, 2, BGRX, &mut dst));
        assert_eq!(dst, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(BGRX.target_format(), PixelFormat::Bgrx8);
    }

    #[test]
    fn rgb_orders_are_swizzled_to_bgr() {
        let mut dst = [0u8; 4];
        let rgbx = pixel_layout(VideoFormat::RGBx).expect("layout");
        assert!(convert_frame(&[10, 20, 30, 0], 4, 1, 1, rgbx, &mut dst));
        assert_eq!(dst, [30, 20, 10, 255]);
        let xbgr = pixel_layout(VideoFormat::xBGR).expect("layout");
        assert!(convert_frame(&[0, 30, 20, 10], 4, 1, 1, xbgr, &mut dst));
        assert_eq!(dst, [30, 20, 10, 255]);
    }

    #[test]
    fn straight_alpha_is_premultiplied() {
        let argb = pixel_layout(VideoFormat::ARGB).expect("layout");
        assert_eq!(argb.target_format(), PixelFormat::Bgra8);
        let mut dst = [0u8; 4];
        // A=128, R=255, G=100, B=0
        assert!(convert_frame(&[128, 255, 100, 0], 4, 1, 1, argb, &mut dst));
        assert_eq!(dst, [0, 50, 128, 128]);
    }

    #[test]
    fn rgb24_is_widened_to_bgrx() {
        // 2x1 RGB image, source stride 8 (2 bytes padding).
        let rgb = rgb24_layout(VideoFormat::RGB).expect("layout");
        let mut dst = [0u8; 8];
        assert!(convert_rgb24(
            &[10, 20, 30, 40, 50, 60, 9, 9],
            8,
            2,
            1,
            rgb,
            &mut dst
        ));
        assert_eq!(dst, [30, 20, 10, 255, 60, 50, 40, 255]);
        let bgr = rgb24_layout(VideoFormat::BGR).expect("layout");
        assert!(convert_rgb24(&[10, 20, 30], 3, 1, 1, bgr, &mut dst[..4]));
        assert_eq!(dst[..4], [10, 20, 30, 255]);
        assert!(!convert_rgb24(&[0; 5], 3, 1, 2, bgr, &mut dst));
        assert!(!convert_rgb24(&[0; 6], 2, 1, 2, bgr, &mut dst));
    }

    #[test]
    fn camera_formats_have_layouts() {
        assert_eq!(yuv_layout(VideoFormat::YUY2), Some(YuvLayout::Yuy2));
        assert_eq!(yuv_layout(VideoFormat::I420), Some(YuvLayout::I420));
        assert_eq!(yuv_layout(VideoFormat::YV12), None);
        assert_eq!(rgb24_layout(VideoFormat::RGBx), None);
    }

    #[test]
    fn short_buffers_are_rejected() {
        let mut dst = [0u8; 8];
        assert!(!convert_frame(&[0; 7], 4, 1, 2, BGRX, &mut dst));
        assert!(!convert_frame(&[0; 8], 4, 1, 2, BGRX, &mut dst[..4]));
        assert!(!convert_frame(&[0; 8], 2, 1, 2, BGRX, &mut dst));
    }

    #[test]
    fn audio_enum_format_round_trips_as_f32() {
        let bytes = audio_enum_format(Some((48_000, 2))).expect("serialize");
        let pod = Pod::from_bytes(&bytes).expect("pod");
        let mut info = AudioInfoRaw::new();
        info.parse(pod).expect("parse");
        assert_eq!(info.format(), AudioFormat::F32LE);
        assert_eq!((info.rate(), info.channels()), (48_000, 2));
        assert!(audio_enum_format(Some((48_000, 6))).is_err());
    }
}
