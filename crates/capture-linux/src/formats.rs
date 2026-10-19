//! SPA format negotiation helpers and name mappings.

use std::io::Cursor;

use capture_api::{AudioFormatInfo, Rational, VideoFormatInfo};
use pipewire::spa;
use spa::param::ParamType;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::param::video::{VideoFormat, VideoInfoRaw};
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

const MAX_DIMENSION: u32 = 8192;
const MAX_FRAMERATE: u32 = 360;

pub(crate) fn video_format_name(format: VideoFormat) -> String {
    VIDEO_FORMATS
        .iter()
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

/// `EnumFormat` for interleaved 32-bit float audio at the source's native rate
/// and channel count.
pub(crate) fn audio_enum_format() -> Result<Vec<u8>, String> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    })
}

pub(crate) fn parse_video_format(param: &Pod) -> Result<VideoFormatInfo, String> {
    let (media_type, media_subtype) = format_utils::parse_format(param)
        .map_err(|err| format!("cannot parse negotiated format: {err}"))?;
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return Err("producer negotiated a non-raw video format".to_owned());
    }
    let mut info = VideoInfoRaw::new();
    info.parse(param)
        .map_err(|err| format!("cannot parse raw video format: {err}"))?;
    Ok(VideoFormatInfo {
        pixel_format: video_format_name(info.format()),
        width: info.size().width,
        height: info.size().height,
        framerate: Rational {
            num: info.framerate().num,
            den: info.framerate().denom,
        },
    })
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
        rate: info.rate(),
        channels: info.channels(),
    })
}

fn serialize(object: Object) -> Result<Vec<u8>, String> {
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
        assert_eq!(
            video_format_name(VideoFormat::NV12),
            format!("spa:{}", VideoFormat::NV12.as_raw())
        );
    }

    #[test]
    fn negotiation_pods_serialize() {
        for bytes in [
            video_enum_format(Some((2560, 1440))),
            video_enum_format(None),
            header_meta_param(),
            audio_enum_format(),
        ] {
            let bytes = bytes.expect("serialize");
            assert!(Pod::from_bytes(&bytes).is_some(), "not a valid pod");
        }
    }

    #[test]
    fn audio_enum_format_round_trips_as_f32() {
        let bytes = audio_enum_format().expect("serialize");
        let pod = Pod::from_bytes(&bytes).expect("pod");
        let mut info = AudioInfoRaw::new();
        info.parse(pod).expect("parse");
        assert_eq!(info.format(), AudioFormat::F32LE);
    }
}
