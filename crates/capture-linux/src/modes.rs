//! Camera modes: what a camera node offers (its `EnumFormat` params), and the
//! formats offered back when connecting. Mode choice is shared in `capture-api`.

use std::cmp::Ordering;

use capture_api::{OfferFormat, Rational, UNSTATED_RATE, compare_rates, reduce_rate};
use pipewire::spa;
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::VideoFormat;
use spa::pod::deserialize::PodDeserializer;
use spa::pod::{ChoiceValue, Object, Property, Value};
use spa::utils::{ChoiceEnum, Fraction, Id, Rectangle, SpaTypes};

use crate::formats;

/// Sizes tried within a size range (cameras with discrete sizes list their own).
const RANGE_SIZES: [(u32, u32); 8] = [
    (3840, 2160),
    (2560, 1440),
    (1920, 1080),
    (1280, 720),
    (960, 540),
    (640, 480),
    (640, 360),
    (320, 240),
];
/// Rates tried within a rate range.
const RANGE_RATES: [u32; 8] = [60, 50, 30, 25, 24, 15, 10, 5];
/// Bound on the modes kept per param and per camera, whatever its params say.
const MAX_OFFERS: usize = 1024;
/// Bound on the exact modes offered when connecting: the chosen one and the
/// closest others. After them, one format with any size and rate per
/// pixel format catches whatever mode the camera runs in.
const MAX_CONNECT_MODES: usize = 32;

/// How a camera's frames are encoded, for the formats StreamForge converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CameraFormat {
    /// A raw SPA video format (packed RGB or YUV), as its raw id.
    Raw(u32),
    Mjpeg,
}

impl CameraFormat {
    /// Preference among the formats of one mode, lower first (ADR 0019):
    /// packed RGB, then YUY2 or UYVY, NV12, I420, MJPEG. `None` for a
    /// format that cannot be converted.
    fn convertible_rank(self) -> Option<u8> {
        match self {
            Self::Raw(raw) => {
                let format = VideoFormat(raw);
                if formats::pixel_layout(format).is_some()
                    || formats::rgb24_layout(format).is_some()
                {
                    Some(0)
                } else {
                    formats::yuv_layout(format).map(|layout| match layout {
                        frame_decode::YuvLayout::Yuy2 | frame_decode::YuvLayout::Uyvy => 1,
                        frame_decode::YuvLayout::Nv12 => 2,
                        frame_decode::YuvLayout::I420 => 3,
                    })
                }
            }
            Self::Mjpeg => Some(4),
        }
    }

    pub fn name(self) -> String {
        match self {
            Self::Raw(raw) => formats::video_format_name(VideoFormat(raw)),
            Self::Mjpeg => formats::MJPEG_NAME.to_owned(),
        }
    }
}

impl capture_api::OfferFormat for CameraFormat {
    fn rank(self) -> u8 {
        self.convertible_rank().unwrap_or(u8::MAX)
    }
}

pub(crate) type ModeOffer = capture_api::ModeOffer<CameraFormat>;

/// Every mode of a camera's `EnumFormat` params, each once and at most
/// [`MAX_OFFERS`].
pub(crate) fn camera_offers<'a>(params: impl IntoIterator<Item = &'a [u8]>) -> Vec<ModeOffer> {
    let mut offers = Vec::new();
    for param in params {
        for offer in parse_offers(param) {
            if offers.len() == MAX_OFFERS {
                return offers;
            }
            if !offers.contains(&offer) {
                offers.push(offer);
            }
        }
    }
    offers
}

/// The modes in one `EnumFormat` param that StreamForge can convert.
/// Anything it cannot read gives no modes.
pub(crate) fn parse_offers(param: &[u8]) -> Vec<ModeOffer> {
    let Ok((_, Value::Object(object))) = PodDeserializer::deserialize_any_from(param) else {
        return Vec::new();
    };
    let mut media_type = None;
    let mut subtype = None;
    let mut raw_formats = Vec::new();
    let mut sizes = Vec::new();
    let mut rates = None;
    for property in &object.properties {
        match property.key {
            key if key == FormatProperties::MediaType.as_raw() => {
                media_type = single_id(&property.value);
            }
            key if key == FormatProperties::MediaSubtype.as_raw() => {
                subtype = single_id(&property.value);
            }
            key if key == FormatProperties::VideoFormat.as_raw() => {
                raw_formats = ids(&property.value);
            }
            key if key == FormatProperties::VideoSize.as_raw() => {
                sizes = sizes_of(&property.value);
            }
            key if key == FormatProperties::VideoFramerate.as_raw() => {
                rates = Some(rates_of(&property.value));
            }
            _ => {}
        }
    }
    if media_type != Some(MediaType::Video.as_raw()) {
        return Vec::new();
    }
    let formats: Vec<CameraFormat> = match subtype {
        Some(raw) if raw == MediaSubtype::Raw.as_raw() => {
            raw_formats.into_iter().map(CameraFormat::Raw).collect()
        }
        Some(raw) if raw == MediaSubtype::Mjpg.as_raw() => vec![CameraFormat::Mjpeg],
        _ => Vec::new(),
    };
    // No rate, or none we can name: the camera picks its own.
    let rates = rates
        .filter(|rates| !rates.is_empty())
        .unwrap_or_else(|| vec![UNSTATED_RATE]);
    let mut offers = Vec::new();
    for format in formats
        .into_iter()
        .filter(|f| f.convertible_rank().is_some())
    {
        for &(width, height) in &sizes {
            for &fps in &rates {
                if offers.len() == MAX_OFFERS {
                    return offers;
                }
                offers.push(ModeOffer {
                    width,
                    height,
                    fps,
                    format,
                });
            }
        }
    }
    offers
}

fn single_id(value: &Value) -> Option<u32> {
    match value {
        Value::Id(Id(id)) => Some(*id),
        Value::Choice(ChoiceValue::Id(choice)) => match &choice.1 {
            ChoiceEnum::None(Id(id)) => Some(*id),
            _ => None,
        },
        _ => None,
    }
}

fn ids(value: &Value) -> Vec<u32> {
    let mut found = match value {
        Value::Id(Id(id)) => vec![*id],
        Value::Choice(ChoiceValue::Id(choice)) => match &choice.1 {
            ChoiceEnum::None(Id(id)) => vec![*id],
            ChoiceEnum::Enum {
                default,
                alternatives,
            } => std::iter::once(default)
                .chain(alternatives)
                .map(|Id(id)| *id)
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    dedup_in_order(&mut found);
    found
}

fn sizes_of(value: &Value) -> Vec<(u32, u32)> {
    let valid = |&(w, h): &(u32, u32)| {
        (1..=formats::MAX_DIMENSION).contains(&w) && (1..=formats::MAX_DIMENSION).contains(&h)
    };
    let rect = |r: &Rectangle| (r.width, r.height);
    fn within(
        min: Rectangle,
        max: Rectangle,
        step: Option<Rectangle>,
    ) -> impl Fn(&(u32, u32)) -> bool {
        move |&(w, h)| {
            let aligned =
                |value: u32, min: u32, step: u32| step == 0 || (value - min).is_multiple_of(step);
            (min.width..=max.width).contains(&w)
                && (min.height..=max.height).contains(&h)
                && step.is_none_or(|s| {
                    aligned(w, min.width, s.width) && aligned(h, min.height, s.height)
                })
        }
    }
    let mut found: Vec<(u32, u32)> = match value {
        Value::Rectangle(r) => vec![rect(r)],
        Value::Choice(ChoiceValue::Rectangle(choice)) => match &choice.1 {
            ChoiceEnum::None(r) => vec![rect(r)],
            ChoiceEnum::Enum {
                default,
                alternatives,
            } => std::iter::once(default)
                .chain(alternatives)
                .map(rect)
                .collect(),
            ChoiceEnum::Range { default, min, max } => std::iter::once(rect(default))
                .chain(RANGE_SIZES.into_iter().filter(within(*min, *max, None)))
                .collect(),
            ChoiceEnum::Step {
                default,
                min,
                max,
                step,
            } => std::iter::once(rect(default))
                .chain(
                    RANGE_SIZES
                        .into_iter()
                        .filter(within(*min, *max, Some(*step))),
                )
                .collect(),
            ChoiceEnum::Flags { .. } => Vec::new(),
        },
        _ => Vec::new(),
    };
    found.retain(valid);
    dedup_in_order(&mut found);
    found
}

fn rates_of(value: &Value) -> Vec<Rational> {
    let rational = |f: &Fraction| Rational {
        num: f.num,
        den: f.denom,
    };
    let within = |min: &Fraction, max: &Fraction, step: Option<&Fraction>| {
        let (min, max, step) = (rational(min), rational(max), step.map(rational));
        move |rate: &Rational| {
            compare_rates(*rate, min) != Ordering::Less
                && compare_rates(*rate, max) != Ordering::Greater
                && step.is_none_or(|step| on_step(*rate, min, step))
        }
    };
    let mut found: Vec<Rational> = match value {
        Value::Fraction(f) => vec![rational(f)],
        Value::Choice(ChoiceValue::Fraction(choice)) => match &choice.1 {
            ChoiceEnum::None(f) => vec![rational(f)],
            ChoiceEnum::Enum {
                default,
                alternatives,
            } => std::iter::once(default)
                .chain(alternatives)
                .map(rational)
                .collect(),
            ChoiceEnum::Range { default, min, max } => std::iter::once(rational(default))
                .chain(
                    RANGE_RATES
                        .into_iter()
                        .map(Rational::integer)
                        .filter(within(min, max, None)),
                )
                .collect(),
            ChoiceEnum::Step {
                default,
                min,
                max,
                step,
            } => std::iter::once(rational(default))
                .chain(
                    RANGE_RATES
                        .into_iter()
                        .map(Rational::integer)
                        .filter(within(min, max, Some(step))),
                )
                .collect(),
            ChoiceEnum::Flags { .. } => Vec::new(),
        },
        _ => Vec::new(),
    };
    // 0/1 means a variable rate; a camera mode needs a nominal one.
    found.retain(|rate| rate.num > 0 && rate.den > 0);
    for rate in &mut found {
        *rate = reduce_rate(*rate);
    }
    dedup_in_order(&mut found);
    found
}

fn dedup_in_order<T: PartialEq + Copy>(values: &mut Vec<T>) {
    let mut kept = Vec::with_capacity(values.len());
    for value in values.drain(..) {
        if !kept.contains(&value) {
            kept.push(value);
        }
    }
    *values = kept;
}

/// Whether `rate` (at least `min`) is `min` plus a whole number of `step`s.
/// A zero step allows any rate.
fn on_step(rate: Rational, min: Rational, step: Rational) -> bool {
    if step.num == 0 || step.den == 0 || rate.den == 0 || min.den == 0 {
        return true;
    }
    // (rate - min) / step = (rn·md − mn·rd)·sd / (rd·md·sn)
    let (rn, rd) = (u128::from(rate.num), u128::from(rate.den));
    let (mn, md) = (u128::from(min.num), u128::from(min.den));
    let (sn, sd) = (u128::from(step.num), u128::from(step.den));
    let Some(diff) = (rn * md).checked_sub(mn * rd) else {
        return false;
    };
    (diff * sd).is_multiple_of(rd * md * sn)
}

fn fps_value(rate: Rational) -> f64 {
    f64::from(rate.num) / f64::from(rate.den)
}

/// The exact modes to connect with: `chosen` first, then the camera's
/// other modes, closest first (at most [`MAX_CONNECT_MODES`]).
pub(crate) fn connect_order(offers: &[ModeOffer], chosen: ModeOffer) -> Vec<ModeOffer> {
    let mut rest: Vec<ModeOffer> = offers.iter().copied().filter(|o| *o != chosen).collect();
    let fps_distance = |o: &ModeOffer| (fps_value(o.fps) - fps_value(chosen.fps)).abs();
    rest.sort_by(|a, b| {
        let other_mode = |o: &ModeOffer| !(o.size() == chosen.size() && o.fps == chosen.fps);
        other_mode(a)
            .cmp(&other_mode(b))
            .then(
                a.area()
                    .abs_diff(chosen.area())
                    .cmp(&b.area().abs_diff(chosen.area())),
            )
            .then(fps_distance(a).total_cmp(&fps_distance(b)))
            .then(a.format.rank().cmp(&b.format.rank()))
    });
    let mut order = vec![chosen];
    order.extend(rest);
    order.truncate(MAX_CONNECT_MODES);
    order
}

/// The `EnumFormat` pods to connect with: the exact modes of
/// [`connect_order`], then each pixel format the camera offers with any
/// size and rate, by rank, so a camera that already runs in another mode
/// for another application still connects.
pub(crate) fn connect_pods(
    offers: &[ModeOffer],
    chosen: ModeOffer,
) -> Result<Vec<Vec<u8>>, String> {
    let mut pods = connect_order(offers, chosen)
        .iter()
        .map(format_pod)
        .collect::<Result<Vec<_>, _>>()?;
    let mut formats: Vec<CameraFormat> = Vec::new();
    for offer in offers {
        if !formats.contains(&offer.format) {
            formats.push(offer.format);
        }
    }
    formats.sort_by_key(|format| format.rank());
    for format in formats {
        pods.push(any_mode_pod(format)?);
    }
    Ok(pods)
}

/// `EnumFormat` of one pixel format at any size and rate.
fn any_mode_pod(format: CameraFormat) -> Result<Vec<u8>, String> {
    serialize_format(format, None)
}

/// `EnumFormat` naming exactly one mode (without a rate for the unstated one).
pub(crate) fn format_pod(offer: &ModeOffer) -> Result<Vec<u8>, String> {
    serialize_format(offer.format, Some(offer))
}

fn serialize_format(format: CameraFormat, mode: Option<&ModeOffer>) -> Result<Vec<u8>, String> {
    let (subtype, raw) = match format {
        CameraFormat::Raw(raw) => (MediaSubtype::Raw, Some(raw)),
        CameraFormat::Mjpeg => (MediaSubtype::Mjpg, None),
    };
    let mut properties = vec![
        Property::new(
            FormatProperties::MediaType.as_raw(),
            Value::Id(Id(MediaType::Video.as_raw())),
        ),
        Property::new(
            FormatProperties::MediaSubtype.as_raw(),
            Value::Id(Id(subtype.as_raw())),
        ),
    ];
    if let Some(raw) = raw {
        properties.push(Property::new(
            FormatProperties::VideoFormat.as_raw(),
            Value::Id(Id(raw)),
        ));
    }
    if let Some(offer) = mode {
        properties.push(Property::new(
            FormatProperties::VideoSize.as_raw(),
            Value::Rectangle(Rectangle {
                width: offer.width,
                height: offer.height,
            }),
        ));
        if offer.fps != UNSTATED_RATE {
            properties.push(Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Fraction(Fraction {
                    num: offer.fps.num,
                    denom: offer.fps.den,
                }),
            ));
        }
    }
    formats::serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use capture_api::CameraRequest;
    use spa::pod::Pod;
    use spa::utils::{Choice as SpaChoice, ChoiceFlags};

    const YUY2: CameraFormat = CameraFormat::Raw(spa::sys::SPA_VIDEO_FORMAT_YUY2);
    const NV12: CameraFormat = CameraFormat::Raw(spa::sys::SPA_VIDEO_FORMAT_NV12);
    const RGB: CameraFormat = CameraFormat::Raw(spa::sys::SPA_VIDEO_FORMAT_RGB);

    fn offer(width: u32, height: u32, fps: u32, format: CameraFormat) -> ModeOffer {
        ModeOffer {
            width,
            height,
            fps: Rational::integer(fps),
            format,
        }
    }

    fn request(size: Option<(u32, u32)>, fps: Option<u32>) -> CameraRequest {
        CameraRequest {
            camera_id: "camera".into(),
            size,
            fps: fps.map(Rational::integer),
        }
    }

    /// A typical USB webcam: YUY2 is slow at large sizes, MJPEG fast.
    fn webcam() -> Vec<ModeOffer> {
        vec![
            offer(640, 480, 30, YUY2),
            offer(1280, 720, 10, YUY2),
            offer(1920, 1080, 5, YUY2),
            offer(640, 480, 30, CameraFormat::Mjpeg),
            offer(1280, 720, 30, CameraFormat::Mjpeg),
            offer(1920, 1080, 30, CameraFormat::Mjpeg),
            offer(1920, 1080, 15, CameraFormat::Mjpeg),
        ]
    }

    fn enum_format(properties: Vec<Property>) -> Vec<u8> {
        formats::serialize(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties,
        })
        .expect("serialize")
    }

    fn id_property(key: FormatProperties, id: u32) -> Property {
        Property::new(key.as_raw(), Value::Id(Id(id)))
    }

    #[test]
    fn a_cameras_params_give_each_mode_once() {
        let yuy2 = format_pod(&offer(640, 480, 30, YUY2)).expect("pod");
        let mjpeg = format_pod(&offer(640, 480, 30, CameraFormat::Mjpeg)).expect("pod");
        let offers = camera_offers([&yuy2[..], &mjpeg[..], &yuy2[..]]);
        assert_eq!(
            offers,
            [
                offer(640, 480, 30, YUY2),
                offer(640, 480, 30, CameraFormat::Mjpeg)
            ]
        );
        let many: Vec<Vec<u8>> = (1..=1100)
            .map(|width| format_pod(&offer(width, 480, 30, YUY2)).expect("pod"))
            .collect();
        assert_eq!(
            camera_offers(many.iter().map(Vec::as_slice)).len(),
            MAX_OFFERS
        );
    }

    #[test]
    fn connect_order_starts_with_the_choice_and_keeps_the_rest() {
        let offers = webcam();
        let chosen = offer(1280, 720, 30, CameraFormat::Mjpeg);
        let order = connect_order(&offers, chosen);
        assert_eq!(order.len(), offers.len());
        assert_eq!(order[0], chosen);
        assert_eq!(order[1], offer(1280, 720, 10, YUY2));
        for offer in &offers {
            assert!(order.contains(offer));
        }
    }

    #[test]
    fn discrete_v4l2_params_are_parsed() {
        // As spa-v4l2 enumerates a webcam: one param per format and size,
        // the rates as an enum.
        let bytes = enum_format(vec![
            id_property(FormatProperties::MediaType, MediaType::Video.as_raw()),
            id_property(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw()),
            id_property(
                FormatProperties::VideoFormat,
                spa::sys::SPA_VIDEO_FORMAT_YUY2,
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Rectangle(Rectangle {
                    width: 640,
                    height: 480,
                }),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(SpaChoice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Fraction { num: 30, denom: 1 },
                        alternatives: vec![
                            Fraction { num: 30, denom: 1 },
                            Fraction { num: 15, denom: 1 },
                            Fraction { num: 0, denom: 1 },
                        ],
                    },
                ))),
            ),
        ]);
        assert_eq!(
            parse_offers(&bytes),
            [offer(640, 480, 30, YUY2), offer(640, 480, 15, YUY2)]
        );
    }

    #[test]
    fn ranges_are_expanded_and_unsupported_formats_dropped() {
        // As videotestsrc offers itself: RGB or UYVY, any size and rate.
        let bytes = enum_format(vec![
            id_property(FormatProperties::MediaType, MediaType::Video.as_raw()),
            id_property(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw()),
            Property::new(
                FormatProperties::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(SpaChoice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(spa::sys::SPA_VIDEO_FORMAT_RGB),
                        alternatives: vec![
                            Id(spa::sys::SPA_VIDEO_FORMAT_RGB),
                            Id(spa::sys::SPA_VIDEO_FORMAT_UYVY),
                            Id(spa::sys::SPA_VIDEO_FORMAT_YV12),
                        ],
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(SpaChoice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle {
                            width: 320,
                            height: 240,
                        },
                        min: Rectangle {
                            width: 1,
                            height: 1,
                        },
                        max: Rectangle {
                            width: 1280,
                            height: 720,
                        },
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(SpaChoice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: 25, denom: 1 },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction { num: 30, denom: 1 },
                    },
                ))),
            ),
        ]);
        let offers = parse_offers(&bytes);
        let formats: Vec<_> = offers.iter().map(|o| o.format).collect();
        assert!(!formats.contains(&CameraFormat::Raw(spa::sys::SPA_VIDEO_FORMAT_YV12)));
        let rgb: Vec<_> = offers
            .iter()
            .filter(|o| o.format == RGB)
            .map(|o| (o.width, o.height, o.fps.num))
            .collect();
        let sizes = [(320, 240), (1280, 720), (960, 540), (640, 480), (640, 360)];
        let rates = [25, 30, 24, 15, 10, 5];
        let expected: Vec<_> = sizes
            .iter()
            .flat_map(|&(w, h)| rates.iter().map(move |&r| (w, h, r)))
            .collect();
        assert_eq!(rgb, expected);
        assert_eq!(offers.len(), 2 * expected.len());
    }

    #[test]
    fn stepwise_sizes_keep_only_aligned_ones() {
        let bytes = enum_format(vec![
            id_property(FormatProperties::MediaType, MediaType::Video.as_raw()),
            id_property(FormatProperties::MediaSubtype, MediaSubtype::Mjpg.as_raw()),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(SpaChoice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Step {
                        default: Rectangle {
                            width: 640,
                            height: 480,
                        },
                        min: Rectangle {
                            width: 160,
                            height: 120,
                        },
                        max: Rectangle {
                            width: 1920,
                            height: 1080,
                        },
                        step: Rectangle {
                            width: 160,
                            height: 120,
                        },
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Fraction(Fraction { num: 30, denom: 1 }),
            ),
        ]);
        let sizes: Vec<_> = parse_offers(&bytes).iter().map(ModeOffer::size).collect();
        // 960x540 is off the 120-pixel height step: (540 - 120) % 120 != 0.
        assert_eq!(
            sizes,
            [
                (640, 480),
                (1920, 1080),
                (1280, 720),
                (640, 360),
                (320, 240)
            ]
        );
    }

    #[test]
    fn params_without_a_rate_give_modes_at_the_unstated_rate() {
        // As spa-libcamera enumerates: format and size, no frame rate.
        let bytes = enum_format(vec![
            id_property(FormatProperties::MediaType, MediaType::Video.as_raw()),
            id_property(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw()),
            id_property(
                FormatProperties::VideoFormat,
                spa::sys::SPA_VIDEO_FORMAT_NV12,
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Rectangle(Rectangle {
                    width: 1920,
                    height: 1080,
                }),
            ),
        ]);
        let offers = parse_offers(&bytes);
        assert_eq!(
            offers,
            [ModeOffer {
                fps: UNSTATED_RATE,
                ..offer(1920, 1080, 1, NV12)
            }]
        );
        let choice = capture_api::choose_mode(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer, offers[0]);
        assert!(choice.warnings.is_empty());
        let choice = capture_api::choose_mode(&offers, &request(None, Some(30))).expect("choice");
        assert_eq!(
            choice.warnings,
            [
                "the camera does not state its frame rates at 1920x1080; it runs at its own \
                 rate, not necessarily 30 fps"
            ]
        );
        // Connecting names no rate either.
        let pod = format_pod(&offers[0]).expect("pod");
        assert_eq!(parse_offers(&pod), offers);
    }

    #[test]
    fn rate_steps_keep_only_rates_on_the_step() {
        let stepped = |default: Fraction, min: Fraction, max: Fraction, step: Fraction| {
            let bytes = enum_format(vec![
                id_property(FormatProperties::MediaType, MediaType::Video.as_raw()),
                id_property(FormatProperties::MediaSubtype, MediaSubtype::Mjpg.as_raw()),
                Property::new(
                    FormatProperties::VideoSize.as_raw(),
                    Value::Rectangle(Rectangle {
                        width: 640,
                        height: 480,
                    }),
                ),
                Property::new(
                    FormatProperties::VideoFramerate.as_raw(),
                    Value::Choice(ChoiceValue::Fraction(SpaChoice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Step {
                            default,
                            min,
                            max,
                            step,
                        },
                    ))),
                ),
            ]);
            parse_offers(&bytes)
                .iter()
                .map(|o| (o.fps.num, o.fps.den))
                .collect::<Vec<_>>()
        };
        let whole = |num| Fraction { num, denom: 1 };
        assert_eq!(
            stepped(whole(5), whole(5), whole(55), whole(10)),
            [(5, 1), (25, 1), (15, 1)]
        );
        let half = Fraction { num: 15, denom: 2 };
        assert_eq!(
            stepped(half, half, whole(60), half),
            [(15, 2), (60, 1), (30, 1), (15, 1)]
        );
    }

    #[test]
    fn connect_pods_end_with_any_mode_of_each_format() {
        let many: Vec<ModeOffer> = (1..=100)
            .map(|width| offer(width * 8, 480, 30, YUY2))
            .chain([offer(640, 480, 30, CameraFormat::Mjpeg)])
            .collect();
        let chosen = many[40];
        assert_eq!(connect_order(&many, chosen).len(), MAX_CONNECT_MODES);
        let pods = connect_pods(&many, chosen).expect("pods");
        assert_eq!(pods.len(), MAX_CONNECT_MODES + 2);
        assert_eq!(parse_offers(&pods[0]), [chosen]);
        // The last two name a format but no size: any mode of it.
        for pod in &pods[MAX_CONNECT_MODES..] {
            assert!(Pod::from_bytes(pod).is_some());
            assert!(parse_offers(pod).is_empty());
        }
    }

    #[test]
    fn audio_and_garbage_params_give_no_modes() {
        let audio = enum_format(vec![
            id_property(FormatProperties::MediaType, MediaType::Audio.as_raw()),
            id_property(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw()),
        ]);
        assert!(parse_offers(&audio).is_empty());
        assert!(parse_offers(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn format_pods_round_trip() {
        for offer in [
            offer(1280, 720, 30, YUY2),
            offer(1920, 1080, 30, CameraFormat::Mjpeg),
        ] {
            let bytes = format_pod(&offer).expect("pod");
            assert_eq!(parse_offers(&bytes), [offer]);
        }
    }
}
