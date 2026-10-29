//! Shared camera mode choice (ADR 0019).

use std::cmp::Ordering;

use crate::{CameraMode, CameraRequest, Rational};

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
/// The rate of a mode whose camera does not state its rates; it runs at its own.
pub const UNSTATED_RATE: Rational = Rational { num: 0, den: 1 };

/// A camera's pixel format, as far as the mode choice needs it.
pub trait OfferFormat: Copy + Eq {
    /// Preference among the formats of one mode, lower first (ADR 0019:
    /// packed RGB, then YUY2 or UYVY, NV12, I420, MJPEG). Offers must only
    /// hold formats the backend can deliver.
    fn rank(self) -> u8;
}

/// One size, rate and format a camera can deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeOffer<F> {
    pub width: u32,
    pub height: u32,
    pub fps: Rational,
    pub format: F,
}

impl<F> ModeOffer<F> {
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// Reduced by the gcd; `0/x` and `x/0` unchanged.
pub fn reduce_rate(rate: Rational) -> Rational {
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

/// Order of two rates by value (0/1, the unstated rate, is the lowest).
pub fn compare_rates(a: Rational, b: Rational) -> Ordering {
    (u64::from(a.num) * u64::from(b.den)).cmp(&(u64::from(b.num) * u64::from(a.den)))
}

/// The numeric value of a rate, for measuring distance between modes.
pub fn rate_value(rate: Rational) -> f64 {
    f64::from(rate.num) / f64::from(rate.den)
}

/// The modes a camera lists: each size and rate once, largest first, then fastest.
pub fn listed_modes<F>(offers: &[ModeOffer<F>]) -> Vec<CameraMode> {
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
            .then(compare_rates(b.fps, a.fps))
    });
    modes
}

/// The mode a request gets, with warnings when it differs from the request.
#[derive(Debug, Clone, PartialEq)]
pub struct ModeChoice<F> {
    pub offer: ModeOffer<F>,
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
/// - Format among that size and rate: by [`OfferFormat::rank`].
pub fn choose_mode<F: OfferFormat>(
    offers: &[ModeOffer<F>],
    request: &CameraRequest,
) -> Option<ModeChoice<F>> {
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
    let at_size: Vec<&ModeOffer<F>> = offers.iter().filter(|o| o.size() == size).collect();
    let fps = match request.fps {
        Some(wanted) if wanted.num > 0 && wanted.den > 0 => {
            let fps = at_size.iter().map(|o| o.fps).min_by(|a, b| {
                let distance = |r: &Rational| (rate_value(*r) - rate_value(wanted)).abs();
                distance(a).total_cmp(&distance(b))
            })?;
            if fps == UNSTATED_RATE {
                warnings.push(format!(
                    "the camera does not state its frame rates at {}x{}; it runs at its own \
                     rate, not necessarily {} fps",
                    size.0,
                    size.1,
                    rate_text(wanted)
                ));
            } else if compare_rates(fps, wanted) != Ordering::Equal {
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
        .filter(|o| compare_rates(o.fps, fps) == Ordering::Equal)
        .min_by_key(|o| o.format.rank())?;
    Some(ModeChoice {
        offer: *offer,
        warnings,
    })
}

fn closest_size<F>(offers: &[ModeOffer<F>], (w, h): (u32, u32)) -> Option<(u32, u32)> {
    let wanted = u64::from(w) * u64::from(h);
    offers.iter().map(ModeOffer::size).min_by_key(|&(ow, oh)| {
        let area = u64::from(ow) * u64::from(oh);
        (area.abs_diff(wanted), ow.abs_diff(w) + oh.abs_diff(h))
    })
}

fn default_size<F>(offers: &[ModeOffer<F>]) -> Option<(u32, u32)> {
    let fits = |o: &&ModeOffer<F>| o.width <= CANVAS.0 && o.height <= CANVAS.1;
    let largest = |o: &&ModeOffer<F>| (o.area(), o.width);
    offers
        .iter()
        .filter(fits)
        .filter(|o| compare_rates(o.fps, MIN_DEFAULT_FPS) != Ordering::Less)
        .max_by_key(largest)
        .or_else(|| offers.iter().filter(fits).max_by_key(largest))
        .or_else(|| offers.iter().min_by_key(|o| (o.area(), o.width)))
        .map(ModeOffer::size)
}

fn default_rate(rates: impl Iterator<Item = Rational> + Clone) -> Option<Rational> {
    let max = Rational::integer(MAX_DEFAULT_FPS);
    rates
        .clone()
        .filter(|r| compare_rates(*r, max) != Ordering::Greater)
        .max_by(|a, b| compare_rates(*a, *b))
        .or_else(|| rates.min_by(|a, b| compare_rates(*a, *b)))
}

/// `30` or `30000/1001`.
pub fn rate_text(rate: Rational) -> String {
    if rate.den == 1 {
        rate.num.to_string()
    } else {
        format!("{}/{}", rate.num, rate.den)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Fmt {
        Rgb,
        Yuy2,
        Nv12,
        I420,
        Mjpeg,
    }

    impl OfferFormat for Fmt {
        fn rank(self) -> u8 {
            match self {
                Self::Rgb => 0,
                Self::Yuy2 => 1,
                Self::Nv12 => 2,
                Self::I420 => 3,
                Self::Mjpeg => 4,
            }
        }
    }

    fn offer(width: u32, height: u32, fps: u32, format: Fmt) -> ModeOffer<Fmt> {
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
    fn webcam() -> Vec<ModeOffer<Fmt>> {
        vec![
            offer(640, 480, 30, Fmt::Yuy2),
            offer(1280, 720, 10, Fmt::Yuy2),
            offer(1920, 1080, 5, Fmt::Yuy2),
            offer(640, 480, 30, Fmt::Mjpeg),
            offer(1280, 720, 30, Fmt::Mjpeg),
            offer(1920, 1080, 30, Fmt::Mjpeg),
            offer(1920, 1080, 15, Fmt::Mjpeg),
        ]
    }

    #[test]
    fn default_is_the_largest_size_up_to_1080p_at_30_fps_or_more() {
        let choice = choose_mode(&webcam(), &request(None, None)).expect("choice");
        assert_eq!(choice.offer, offer(1920, 1080, 30, Fmt::Mjpeg));
        assert!(choice.warnings.is_empty());
    }

    #[test]
    fn default_prefers_a_fast_smaller_size_over_a_slow_large_one() {
        let offers = [
            offer(1920, 1080, 5, Fmt::Yuy2),
            offer(1280, 720, 30, Fmt::Yuy2),
        ];
        let choice = choose_mode(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.size(), (1280, 720));
    }

    #[test]
    fn default_falls_back_to_the_largest_size_up_to_1080p_at_any_rate() {
        let offers = [
            offer(3840, 2160, 30, Fmt::Yuy2),
            offer(1920, 1080, 15, Fmt::Yuy2),
            offer(640, 480, 15, Fmt::Yuy2),
        ];
        let choice = choose_mode(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer, offer(1920, 1080, 15, Fmt::Yuy2));
    }

    #[test]
    fn default_falls_back_to_the_smallest_size_above_1080p() {
        let offers = [
            offer(3840, 2160, 30, Fmt::Yuy2),
            offer(2560, 1440, 30, Fmt::Yuy2),
        ];
        let choice = choose_mode(&offers, &request(None, None)).expect("choice");
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
                ..offer(1920, 1080, 1, Fmt::Yuy2)
            },
            offer(1280, 720, 60, Fmt::Yuy2),
        ];
        let choice = choose_mode(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.size(), (1920, 1080));
        // 29 fps is not 30.
        let offers = [
            offer(1920, 1080, 29, Fmt::Yuy2),
            offer(1280, 720, 60, Fmt::Yuy2),
        ];
        let choice = choose_mode(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.size(), (1280, 720));
    }

    #[test]
    fn default_rate_is_the_highest_up_to_60() {
        let offers = [
            offer(1280, 720, 30, Fmt::Yuy2),
            offer(1280, 720, 60, Fmt::Yuy2),
            offer(1280, 720, 120, Fmt::Yuy2),
        ];
        let choice = choose_mode(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.fps, Rational::integer(60));
        let fast = [
            offer(640, 480, 90, Fmt::Yuy2),
            offer(640, 480, 120, Fmt::Yuy2),
        ];
        let choice = choose_mode(&fast, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.fps, Rational::integer(90));
    }

    #[test]
    fn a_requested_mode_is_used_exactly() {
        let choice = choose_mode(&webcam(), &request(Some((1280, 720)), Some(10))).expect("choice");
        assert_eq!(choice.offer, offer(1280, 720, 10, Fmt::Yuy2));
        assert!(choice.warnings.is_empty());
    }

    #[test]
    fn a_missing_mode_gets_the_closest_with_a_warning() {
        let choice = choose_mode(&webcam(), &request(Some((1366, 768)), Some(25))).expect("choice");
        assert_eq!(choice.offer, offer(1280, 720, 30, Fmt::Mjpeg));
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
        let choice = choose_mode(&webcam(), &request(None, Some(15))).expect("choice");
        assert_eq!(choice.offer, offer(1920, 1080, 15, Fmt::Mjpeg));
    }

    #[test]
    fn formats_of_one_mode_are_ranked() {
        let offers = [
            offer(640, 480, 30, Fmt::Mjpeg),
            offer(640, 480, 30, Fmt::I420),
            offer(640, 480, 30, Fmt::Nv12),
            offer(640, 480, 30, Fmt::Yuy2),
            offer(640, 480, 30, Fmt::Rgb),
        ];
        let mut remaining = offers.to_vec();
        for expected in [Fmt::Rgb, Fmt::Yuy2, Fmt::Nv12] {
            let choice = choose_mode(&remaining, &request(None, None)).expect("choice");
            assert_eq!(choice.offer.format, expected);
            remaining.retain(|o| o.format != expected);
        }
        let choice = choose_mode(&remaining, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.format, Fmt::I420);
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
                ..offer(1280, 720, 1, Fmt::Yuy2)
            },
            offer(1280, 720, 25, Fmt::Yuy2),
        ];
        let choice = choose_mode(&offers, &request(None, None)).expect("choice");
        assert_eq!(choice.offer.fps, ntsc);
        let choice = choose_mode(&offers, &request(None, Some(30))).expect("choice");
        assert_eq!(choice.offer.fps, ntsc);
        assert_eq!(
            choice.warnings,
            ["the camera has no 30 fps mode at 1280x720; using 30000/1001"]
        );
    }

    #[test]
    fn no_offers_no_choice() {
        assert_eq!(choose_mode::<Fmt>(&[], &request(None, None)), None);
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
    fn rates_are_formatted_and_reduced() {
        assert_eq!(rate_text(Rational::integer(30)), "30");
        assert_eq!(
            rate_text(Rational {
                num: 30000,
                den: 1001
            }),
            "30000/1001"
        );
        assert_eq!(
            reduce_rate(Rational {
                num: 60000,
                den: 2002
            }),
            Rational {
                num: 30000,
                den: 1001
            }
        );
        for rate in [Rational { num: 0, den: 30 }, Rational { num: 30, den: 0 }] {
            assert_eq!(reduce_rate(rate), rate);
        }
    }

    #[test]
    fn an_unstated_rate_warns_for_a_requested_rate() {
        let offers = [ModeOffer {
            fps: UNSTATED_RATE,
            ..offer(1920, 1080, 1, Fmt::Nv12)
        }];
        let choice = choose_mode(&offers, &request(None, Some(30))).expect("choice");
        assert_eq!(choice.offer, offers[0]);
        assert_eq!(
            choice.warnings,
            [
                "the camera does not state its frame rates at 1920x1080; it runs at its own rate, not necessarily 30 fps"
            ]
        );
    }
}
