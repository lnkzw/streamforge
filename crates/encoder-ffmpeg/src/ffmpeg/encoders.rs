//! Which H.264 encoders to try, in order, and the options that give each
//! of them the requested rate control (ADR 0011). Pure, so it is tested on
//! every OS; whether an encoder actually opens is only known by trying.

use encoder_api::{EncoderPreference, RateControl};

/// How frames reach the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Upload {
    /// Software frames (YUV 4:2:0 or NV12, whichever the encoder takes).
    Software,
    /// NV12 frames uploaded to VAAPI surfaces first (Linux).
    Vaapi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub name: &'static str,
    pub hardware: bool,
    pub upload: Upload,
}

const fn hardware(name: &'static str, upload: Upload) -> Candidate {
    Candidate {
        name,
        hardware: true,
        upload,
    }
}

/// Hardware encoders worth trying on this OS, most common first. Each one
/// fails to open quickly when its GPU, driver or runtime is missing.
#[cfg(target_os = "linux")]
const HARDWARE: &[Candidate] = &[
    hardware("h264_vaapi", Upload::Vaapi),
    hardware("h264_nvenc", Upload::Software),
];
#[cfg(target_os = "windows")]
const HARDWARE: &[Candidate] = &[
    hardware("h264_nvenc", Upload::Software),
    hardware("h264_qsv", Upload::Software),
    hardware("h264_amf", Upload::Software),
];
#[cfg(target_os = "macos")]
const HARDWARE: &[Candidate] = &[hardware("h264_videotoolbox", Upload::Software)];
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
const HARDWARE: &[Candidate] = &[];

/// Software encoders: libx264, else OpenH264 (for builds without the GPL).
const SOFTWARE: &[Candidate] = &[
    Candidate {
        name: "libx264",
        hardware: false,
        upload: Upload::Software,
    },
    Candidate {
        name: "libopenh264",
        hardware: false,
        upload: Upload::Software,
    },
];

pub(crate) fn candidates(preference: EncoderPreference) -> Vec<Candidate> {
    match preference {
        EncoderPreference::Auto => HARDWARE.iter().chain(SOFTWARE).copied().collect(),
        EncoderPreference::Software => SOFTWARE.to_vec(),
        EncoderPreference::Hardware => HARDWARE.to_vec(),
    }
}

/// Bit rate for recordings with encoders that have no constant-quality
/// mode here (VideoToolbox, OpenH264): ample for 1080p60 screen content.
const QUALITY_BITRATE_KBPS: u32 = 16_000;

/// Codec options for `encoder` (unknown keys are ignored by FFmpeg).
pub(crate) fn options(encoder: &str, rate: RateControl, fps: u32) -> Vec<(&'static str, String)> {
    let owned = |pairs: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
        pairs.iter().map(|&(k, v)| (k, v.to_owned())).collect()
    };
    match rate {
        RateControl::Quality => match encoder {
            "libx264" => owned(&[("preset", "veryfast"), ("crf", "20")]),
            "h264_vaapi" => owned(&[("rc_mode", "CQP"), ("qp", "22")]),
            "h264_nvenc" => owned(&[
                ("preset", "p4"),
                ("tune", "hq"),
                ("rc", "vbr"),
                ("cq", "21"),
                ("b", "0"),
            ]),
            "h264_qsv" => owned(&[("preset", "veryfast"), ("global_quality", "22")]),
            "h264_amf" => owned(&[
                ("usage", "transcoding"),
                ("rc", "cqp"),
                ("qp_i", "20"),
                ("qp_p", "22"),
            ]),
            "h264_videotoolbox" => vec![
                ("b", format!("{QUALITY_BITRATE_KBPS}k")),
                ("realtime", "1".to_owned()),
            ],
            _ => vec![("b", format!("{QUALITY_BITRATE_KBPS}k"))],
        },
        RateControl::ConstantBitrate {
            kbps,
            keyframe_interval_s,
        } => {
            let rate = format!("{kbps}k");
            let buffer = format!("{}k", u64::from(kbps) * 2);
            let gop = (u64::from(fps) * u64::from(keyframe_interval_s)).to_string();
            let mut options = vec![
                ("b", rate.clone()),
                ("maxrate", rate),
                ("bufsize", buffer),
                ("g", gop.clone()),
            ];
            let extra: &[(&'static str, &str)] = match encoder {
                "libx264" => &[
                    ("preset", "veryfast"),
                    ("nal-hrd", "cbr"),
                    // Exactly every `g` frames: no scene-cut keyframes.
                    ("sc_threshold", "0"),
                ],
                "h264_vaapi" => &[("rc_mode", "CBR")],
                "h264_nvenc" => &[
                    ("preset", "p4"),
                    ("tune", "ll"),
                    ("rc", "cbr"),
                    ("no-scenecut", "1"),
                    ("forced-idr", "1"),
                ],
                "h264_qsv" => &[("preset", "veryfast")],
                "h264_amf" => &[("usage", "lowlatency"), ("rc", "cbr")],
                "h264_videotoolbox" => &[("realtime", "1")],
                _ => &[],
            };
            options.extend(owned(extra));
            if encoder == "libx264" {
                options.push(("keyint_min", gop));
            }
            options
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(preference: EncoderPreference) -> Vec<&'static str> {
        candidates(preference).iter().map(|c| c.name).collect()
    }

    #[test]
    fn auto_tries_hardware_first_and_ends_with_software() {
        let auto = candidates(EncoderPreference::Auto);
        let software = names(EncoderPreference::Software);
        assert_eq!(software, ["libx264", "libopenh264"]);
        assert!(auto.ends_with(&candidates(EncoderPreference::Software)));
        assert!(
            candidates(EncoderPreference::Hardware)
                .iter()
                .all(|c| c.hardware)
        );
        assert_eq!(
            auto.len(),
            HARDWARE.len() + SOFTWARE.len(),
            "auto = hardware + software"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            names(EncoderPreference::Hardware),
            ["h264_vaapi", "h264_nvenc"]
        );
        #[cfg(target_os = "macos")]
        assert_eq!(names(EncoderPreference::Hardware), ["h264_videotoolbox"]);
        #[cfg(target_os = "windows")]
        assert_eq!(
            names(EncoderPreference::Hardware),
            ["h264_nvenc", "h264_qsv", "h264_amf"]
        );
    }

    fn option<'a>(options: &'a [(&'static str, String)], key: &str) -> Option<&'a str> {
        options
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn quality_keeps_the_x264_recording_settings() {
        let options = options("libx264", RateControl::Quality, 60);
        assert_eq!(option(&options, "preset"), Some("veryfast"));
        assert_eq!(option(&options, "crf"), Some("20"));
        assert_eq!(option(&options, "b"), None);
    }

    #[test]
    fn constant_bitrate_sets_rate_buffer_and_keyframes_for_every_encoder() {
        let rate = RateControl::ConstantBitrate {
            kbps: 6000,
            keyframe_interval_s: 2,
        };
        for encoder in [
            "libx264",
            "libopenh264",
            "h264_vaapi",
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "h264_videotoolbox",
        ] {
            let options = options(encoder, rate, 60);
            assert_eq!(option(&options, "b"), Some("6000k"), "{encoder}");
            assert_eq!(option(&options, "maxrate"), Some("6000k"), "{encoder}");
            assert_eq!(option(&options, "bufsize"), Some("12000k"), "{encoder}");
            assert_eq!(option(&options, "g"), Some("120"), "{encoder}");
        }
        let x264 = options("libx264", rate, 60);
        assert_eq!(option(&x264, "nal-hrd"), Some("cbr"));
        assert_eq!(option(&x264, "keyint_min"), Some("120"));
        assert_eq!(option(&x264, "sc_threshold"), Some("0"));
        assert_eq!(option(&options("h264_nvenc", rate, 60), "rc"), Some("cbr"));
        assert_eq!(
            option(&options("h264_vaapi", rate, 60), "rc_mode"),
            Some("CBR")
        );
    }

    #[test]
    fn every_hardware_encoder_has_a_quality_mode() {
        for encoder in [
            "h264_vaapi",
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "h264_videotoolbox",
        ] {
            assert!(
                !options(encoder, RateControl::Quality, 60).is_empty(),
                "{encoder}"
            );
        }
    }
}
