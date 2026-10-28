//! Camera modes: what a camera node offers (its `EnumFormat` params), which
//! mode a request gets (ADR 0019 "Mode choice"), and the formats offered
//! back when connecting.

use std::cmp::Ordering;

use capture_api::{CameraMode, CameraRequest, Rational};
use pipewire::spa;
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::VideoFormat;
use spa::pod::deserialize::PodDeserializer;
use spa::pod::{ChoiceValue, Object, Property, Value};
use spa::utils::{ChoiceEnum, Fraction, Id, Rectangle, SpaTypes};

use crate::formats;

/// The canvas a camera mode is chosen for when the request names none.
const CANVAS: (u32, u32) = (1920, 1080);
/// Without a requested rate: the highest one up to this.
const MAX_DEFAULT_FPS: u32 = 60;
/// A size without a requested one runs at least this fast if it can:
/// 30 fps, counting NTSC's 30000/1001.
const MIN_DEFAULT_FPS: Rational = Rational {
    num: 30000,
    den: 1001,
};
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
/// Bound on the formats offered when connecting: the chosen one and fallbacks.
const MAX_CONNECT_FORMATS: usize = 64;

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
    fn rank(self) -> Option<u8> {
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

/// One size, rate and format a camera can deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModeOffer {
    pub width: u32,
    pub height: u32,
    pub fps: Rational,
    pub format: CameraFormat,
}

impl ModeOffer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

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
    let mut rates = Vec::new();
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
                rates = rates_of(&property.value);
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
    let mut offers = Vec::new();
    for format in formats.into_iter().filter(|f| f.rank().is_some()) {
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
    let within = |min: &Fraction, max: &Fraction| {
        let (min, max) = (rational(min), rational(max));
        move |rate: &Rational| {
            compare(*rate, min) != Ordering::Less && compare(*rate, max) != Ordering::Greater
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
            ChoiceEnum::Range { default, min, max }
            | ChoiceEnum::Step {
                default, min, max, ..
            } => std::iter::once(rational(default))
                .chain(
                    RANGE_RATES
                        .into_iter()
                        .map(Rational::integer)
                        .filter(within(min, max)),
                )
                .collect(),
            ChoiceEnum::Flags { .. } => Vec::new(),
        },
        _ => Vec::new(),
    };
    // 0/1 means a variable rate; a camera mode needs a nominal one.
    found.retain(|rate| rate.num > 0 && rate.den > 0);
    for rate in &mut found {
        *rate = reduce(*rate);
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

pub(crate) fn reduce(rate: Rational) -> Rational {
    if rate.num == 0 || rate.den == 0 {
        return rate;
    }
    let (mut a, mut b) = (rate.num, rate.den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Rational {
        num: rate.num / a,
        den: rate.den / a,
    }
}

/// Order of two positive rates by value.
fn compare(a: Rational, b: Rational) -> Ordering {
    (u64::from(a.num) * u64::from(b.den)).cmp(&(u64::from(b.num) * u64::from(a.den)))
}

fn fps_value(rate: Rational) -> f64 {
    f64::from(rate.num) / f64::from(rate.den)
}

/// The modes a camera lists: each size and rate once, largest first, then fastest.
pub(crate) fn listed_modes(offers: &[ModeOffer]) -> Vec<CameraMode> {
    let mut modes: Vec<CameraMode> = Vec::new();
    for offer in offers {
        let mode = CameraMode {
            width: offer.width,
            height: offer.height,
            fps: offer.fps,
        };
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes.sort_by(|a, b| {
        let area = |m: &CameraMode| u64::from(m.width) * u64::from(m.height);
        area(b)
            .cmp(&area(a))
            .then(b.width.cmp(&a.width))
            .then(compare(b.fps, a.fps))
    });
    modes
}

/// The mode a request gets, with warnings when it differs from the request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Choice {
    pub offer: ModeOffer,
    pub warnings: Vec<String>,
}

/// Pick a mode for `request` (ADR 0019 "Mode choice"); `None` without offers.
///
/// - Size: the requested one, else the closest (by area, then by width and
///   height). Without one: the largest up to the canvas (1920x1080) that runs
///   at 30 fps or more, else the largest up to the canvas, else the smallest
///   above it.
/// - Rate among that size: the requested one, else the closest. Without one:
///   the highest up to 60, else the lowest above.
/// - Format among that size and rate: by [`CameraFormat::rank`].
pub(crate) fn choose(offers: &[ModeOffer], request: &CameraRequest) -> Option<Choice> {
    let mut warnings = Vec::new();
    let size = match request.size {
        Some(wanted) => {
            let size = closest_size(offers, wanted)?;
            if size != wanted {
                warnings.push(format!(
                    "the camera has no {}x{} mode; using {}x{}",
                    wanted.0, wanted.1, size.0, size.1
                ));
            }
            size
        }
        None => default_size(offers)?,
    };
    let at_size: Vec<&ModeOffer> = offers.iter().filter(|o| o.size() == size).collect();
    let fps = match request.fps {
        Some(wanted) if wanted.num > 0 && wanted.den > 0 => {
            let fps = at_size.iter().map(|o| o.fps).min_by(|a, b| {
                let distance = |r: &Rational| (fps_value(*r) - fps_value(wanted)).abs();
                distance(a).total_cmp(&distance(b))
            })?;
            if compare(fps, wanted) != Ordering::Equal {
                warnings.push(format!(
                    "the camera has no {} fps mode at {}x{}; using {}",
                    rate_text(wanted),
                    size.0,
                    size.1,
                    rate_text(fps)
                ));
            }
            fps
        }
        _ => default_rate(at_size.iter().map(|o| o.fps))?,
    };
    let offer = at_size
        .into_iter()
        .filter(|o| compare(o.fps, fps) == Ordering::Equal)
        .min_by_key(|o| o.format.rank())?;
    Some(Choice {
        offer: *offer,
        warnings,
    })
}

fn closest_size(offers: &[ModeOffer], (w, h): (u32, u32)) -> Option<(u32, u32)> {
    let wanted = u64::from(w) * u64::from(h);
    offers.iter().map(ModeOffer::size).min_by_key(|&(ow, oh)| {
        let area = u64::from(ow) * u64::from(oh);
        (area.abs_diff(wanted), ow.abs_diff(w) + oh.abs_diff(h))
    })
}

fn default_size(offers: &[ModeOffer]) -> Option<(u32, u32)> {
    let fits = |o: &&ModeOffer| o.width <= CANVAS.0 && o.height <= CANVAS.1;
    let largest = |o: &&ModeOffer| (o.area(), o.width);
    offers
        .iter()
        .filter(fits)
        .filter(|o| compare(o.fps, MIN_DEFAULT_FPS) != Ordering::Less)
        .max_by_key(largest)
        .or_else(|| offers.iter().filter(fits).max_by_key(largest))
        .or_else(|| offers.iter().min_by_key(|o| (o.area(), o.width)))
        .map(ModeOffer::size)
}

fn default_rate(rates: impl Iterator<Item = Rational> + Clone) -> Option<Rational> {
    let max = Rational::integer(MAX_DEFAULT_FPS);
    rates
        .clone()
        .filter(|r| compare(*r, max) != Ordering::Greater)
        .max_by(|a, b| compare(*a, *b))
        .or_else(|| rates.min_by(|a, b| compare(*a, *b)))
}

pub(crate) fn rate_text(rate: Rational) -> String {
    if rate.den == 1 {
        rate.num.to_string()
    } else {
        format!("{}/{}", rate.num, rate.den)
    }
}

/// The offers to connect with: `chosen` first, then the camera's other
/// modes, closest first, so a camera that already runs in another mode for
/// another application still connects.
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
    order.truncate(MAX_CONNECT_FORMATS);
    order
}

/// `EnumFormat` naming exactly one mode.
pub(crate) fn format_pod(offer: &ModeOffer) -> Result<Vec<u8>, String> {
    let (subtype, raw) = match offer.format {
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
    properties.push(Property::new(
        FormatProperties::VideoSize.as_raw(),
        Value::Rectangle(Rectangle {
            width: offer.width,
            height: offer.height,
        }),
    ));
    properties.push(Property::new(
        FormatProperties::VideoFramerate.as_raw(),
        Value::Fraction(Fraction {
            num: offer.fps.num,
            denom: offer.fps.den,
        }),
    ));
    formats::serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn default_is_the_largest_size_up_to_1080p_at_30_fps_or_more() {
        let choice = choose(&webcam(), &request(None, None)).expect("choice");
        assert_eq!(choice.offer, offer(1920, 1080, 30, CameraFormat::Mjpeg));
        assert!(choice.warnings.is_empty());
    }

    #[test]
    fn default_prefers_a_fast_smaller_size_over_a_slow_large_one() {
        let offers = [offer(1920, 1080, 5, YUY2), offer(1280, 720, 30, YUY2)];
        let choice = choose(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.size(), (1280, 720));
    }

    #[test]
    fn default_falls_back_to_the_largest_size_up_to_1080p_at_any_rate() {
        let offers = [
            offer(3840, 2160, 30, YUY2),
            offer(1920, 1080, 15, YUY2),
            offer(640, 480, 15, YUY2),
        ];
        let choice = choose(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer, offer(1920, 1080, 15, YUY2));
    }

    #[test]
    fn default_falls_back_to_the_smallest_size_above_1080p() {
        let offers = [offer(3840, 2160, 30, YUY2), offer(2560, 1440, 30, YUY2)];
        let choice = choose(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.size(), (2560, 1440));
    }

    #[test]
    fn ntsc_rates_count_as_30_fps() {
        let ntsc = Rational {
            num: 30000,
            den: 1001,
        };
        let offers = [
            ModeOffer {
                fps: ntsc,
                ..offer(1920, 1080, 1, YUY2)
            },
            offer(1280, 720, 60, YUY2),
        ];
        let choice = choose(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.size(), (1920, 1080));
        // 29 fps is not 30.
        let offers = [offer(1920, 1080, 29, YUY2), offer(1280, 720, 60, YUY2)];
        let choice = choose(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.size(), (1280, 720));
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
    fn default_rate_is_the_highest_up_to_60() {
        let offers = [
            offer(1280, 720, 30, YUY2),
            offer(1280, 720, 60, YUY2),
            offer(1280, 720, 120, YUY2),
        ];
        let choice = choose(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.fps, Rational::integer(60));
        let fast = [offer(640, 480, 90, YUY2), offer(640, 480, 120, YUY2)];
        let choice = choose(&fast, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.fps, Rational::integer(90));
    }

    #[test]
    fn a_requested_mode_is_used_exactly() {
        let choice = choose(&webcam(), &request(Some((1280, 720)), Some(10))).expect("choice");
        assert_eq!(choice.offer, offer(1280, 720, 10, YUY2));
        assert!(choice.warnings.is_empty());
    }

    #[test]
    fn a_missing_mode_gets_the_closest_with_a_warning() {
        let choice = choose(&webcam(), &request(Some((1366, 768)), Some(25))).expect("choice");
        assert_eq!(choice.offer, offer(1280, 720, 30, CameraFormat::Mjpeg));
        assert_eq!(
            choice.warnings,
            [
                "the camera has no 1366x768 mode; using 1280x720",
                "the camera has no 25 fps mode at 1280x720; using 30",
            ]
        );
    }

    #[test]
    fn a_requested_rate_without_a_size_uses_the_default_size() {
        let choice = choose(&webcam(), &request(None, Some(15))).expect("choice");
        assert_eq!(choice.offer, offer(1920, 1080, 15, CameraFormat::Mjpeg));
    }

    #[test]
    fn formats_of_one_mode_are_ranked() {
        let offers = [
            offer(640, 480, 30, CameraFormat::Mjpeg),
            offer(
                640,
                480,
                30,
                CameraFormat::Raw(spa::sys::SPA_VIDEO_FORMAT_I420),
            ),
            offer(640, 480, 30, NV12),
            offer(640, 480, 30, YUY2),
            offer(640, 480, 30, RGB),
        ];
        let mut remaining = offers.to_vec();
        for expected in [RGB, YUY2, NV12] {
            let choice = choose(&remaining, &request(None, None)).expect("choice");
            assert_eq!(choice.offer.format, expected);
            remaining.retain(|o| o.format != expected);
        }
        let choice = choose(&remaining, &request(None, None)).expect("choice");
        assert_eq!(
            choice.offer.format,
            CameraFormat::Raw(spa::sys::SPA_VIDEO_FORMAT_I420)
        );
    }

    #[test]
    fn fractional_rates_compare_by_value() {
        let ntsc = Rational {
            num: 30000,
            den: 1001,
        };
        let offers = [
            ModeOffer {
                fps: ntsc,
                ..offer(1280, 720, 1, YUY2)
            },
            offer(1280, 720, 25, YUY2),
        ];
        let choice = choose(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.fps, ntsc);
        let choice = choose(&offers, &request(None, Some(30))).expect("choice");
        assert_eq!(choice.offer.fps, ntsc);
        assert_eq!(
            choice.warnings,
            ["the camera has no 30 fps mode at 1280x720; using 30000/1001"]
        );
    }

    #[test]
    fn no_offers_no_choice() {
        assert_eq!(choose(&[], &request(None, None)), None);
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
    fn listed_modes_are_unique_largest_first() {
        let modes = listed_modes(&webcam());
        let sizes: Vec<_> = modes
            .iter()
            .map(|m| (m.width, m.height, m.fps.num))
            .collect();
        assert_eq!(
            sizes,
            [
                (1920, 1080, 30),
                (1920, 1080, 15),
                (1920, 1080, 5),
                (1280, 720, 30),
                (1280, 720, 10),
                (640, 480, 30),
            ]
        );
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
