use super::*;

const LAYOUTS: [YuvLayout; 4] = [
    YuvLayout::Yuy2,
    YuvLayout::Uyvy,
    YuvLayout::Nv12,
    YuvLayout::I420,
];
const COLORS: [YuvColor; 4] = [
    YuvColor {
        matrix: ColorMatrix::Bt601,
        range: ColorRange::Limited,
    },
    YuvColor {
        matrix: ColorMatrix::Bt601,
        range: ColorRange::Full,
    },
    YuvColor {
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
    },
    YuvColor {
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Full,
    },
];

fn reference(y: u8, u: u8, v: u8, color: YuvColor) -> [u8; 4] {
    let (kr, kb) = match color.matrix {
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt709 => (0.2126, 0.0722),
    };
    let (y, u, v) = match color.range {
        ColorRange::Limited => (
            (f64::from(y) - 16.0) * 255.0 / 219.0,
            (f64::from(u) - 128.0) * 255.0 / 224.0,
            (f64::from(v) - 128.0) * 255.0 / 224.0,
        ),
        ColorRange::Full => (f64::from(y), f64::from(u) - 128.0, f64::from(v) - 128.0),
    };
    let r = y + 2.0 * (1.0 - kr) * v;
    let b = y + 2.0 * (1.0 - kb) * u;
    let g = y
        - 2.0 * kb * (1.0 - kb) / (1.0 - kr - kb) * u
        - 2.0 * kr * (1.0 - kr) / (1.0 - kr - kb) * v;
    let byte = |f: f64| f.round().clamp(0.0, 255.0) as u8;
    [byte(b), byte(g), byte(r), 255]
}

fn encode_rgb(rgb: [u8; 3], color: YuvColor) -> [u8; 3] {
    let (kr, kb) = match color.matrix {
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt709 => (0.2126, 0.0722),
    };
    let [r, g, b] = rgb.map(f64::from);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let u = (b - y) / (2.0 * (1.0 - kb));
    let v = (r - y) / (2.0 * (1.0 - kr));
    let (y, u, v) = if color.range == ColorRange::Limited {
        (
            16.0 + y * 219.0 / 255.0,
            128.0 + u * 224.0 / 255.0,
            128.0 + v * 224.0 / 255.0,
        )
    } else {
        (y, 128.0 + u, 128.0 + v)
    };
    [y, u, v].map(|f| f.round().clamp(0.0, 255.0) as u8)
}

// Shared 4:2:0 samples are repeated on both packed rows, so every layout
// describes exactly the same picture, including the unused odd pair's luma.
struct Picture {
    w: usize,
    h: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}
impl Picture {
    fn random(w: usize, h: usize) -> Self {
        let mut state = 0x5814_abedu32;
        let mut sample = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 24) as u8
        };
        let n = w.div_ceil(2) * h.div_ceil(2);
        Self {
            w,
            h,
            y: (0..w * h).map(|_| sample()).collect(),
            u: (0..n).map(|_| sample()).collect(),
            v: (0..n).map(|_| sample()).collect(),
        }
    }
    fn storage(&self, layout: YuvLayout, pad: usize) -> (Vec<Vec<u8>>, Vec<usize>) {
        let (w, h) = (self.w, self.h);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let geometry = match layout {
            YuvLayout::Yuy2 | YuvLayout::Uyvy => vec![(cw * 4, h)],
            YuvLayout::Nv12 => vec![(w, h), (cw * 2, ch)],
            YuvLayout::I420 => vec![(w, h), (cw, ch), (cw, ch)],
        };
        let strides: Vec<_> = geometry.iter().map(|(row, _)| row + pad).collect();
        // Deliberately omit padding after the last row of every plane.
        let mut data: Vec<_> = geometry
            .iter()
            .zip(&strides)
            .map(|(&(row, rows), &s)| vec![0xa5; s * (rows - 1) + row])
            .collect();
        for y in 0..h {
            for x in 0..w {
                let yy = self.y[y * w + x];
                let uv = (y / 2) * cw + x / 2;
                let (u, v) = (self.u[uv], self.v[uv]);
                match layout {
                    YuvLayout::Yuy2 | YuvLayout::Uyvy => {
                        let o = y * strides[0] + (x / 2) * 4;
                        let p = if layout == YuvLayout::Yuy2 {
                            [yy, u, yy, v]
                        } else {
                            [u, yy, v, yy]
                        };
                        if x % 2 == 0 {
                            data[0][o..o + 4].copy_from_slice(&p);
                        } else {
                            data[0][o + if layout == YuvLayout::Yuy2 { 2 } else { 3 }] = yy;
                        }
                    }
                    YuvLayout::Nv12 => {
                        data[0][y * strides[0] + x] = yy;
                        let o = (y / 2) * strides[1] + (x / 2) * 2;
                        data[1][o..o + 2].copy_from_slice(&[u, v]);
                    }
                    YuvLayout::I420 => {
                        data[0][y * strides[0] + x] = yy;
                        data[1][(y / 2) * strides[1] + x / 2] = u;
                        data[2][(y / 2) * strides[2] + x / 2] = v;
                    }
                }
            }
        }
        (data, strides)
    }
    fn expected(&self, color: YuvColor) -> Vec<u8> {
        (0..self.h)
            .flat_map(|y| {
                (0..self.w).flat_map(move |x| {
                    let uv = (y / 2) * self.w.div_ceil(2) + x / 2;
                    reference(self.y[y * self.w + x], self.u[uv], self.v[uv], color)
                })
            })
            .collect()
    }
}

fn borrowed<'a>(data: &'a [Vec<u8>], strides: &[usize]) -> Vec<Plane<'a>> {
    data.iter()
        .zip(strides)
        .map(|(d, &stride)| Plane { data: d, stride })
        .collect()
}

#[test]
fn reference_colors_exact_for_every_layout_matrix_and_range() {
    assert_eq!(YuvColor::default(), COLORS[0]);
    for color in COLORS {
        for rgb in [
            [0, 0, 0],
            [255, 255, 255],
            [128, 128, 128],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
        ] {
            let [y, u, v] = encode_rgb(rgb, color);
            let picture = Picture {
                w: 2,
                h: 2,
                y: vec![y; 4],
                u: vec![u],
                v: vec![v],
            };
            let expected = reference(y, u, v, color).repeat(4);
            for layout in LAYOUTS {
                let (data, strides) = picture.storage(layout, 0);
                let p = Planes::separate(layout, &borrowed(&data, &strides)).unwrap();
                let mut dst = [0; 16];
                convert_yuv(layout, &p, 2, 2, color, &mut dst, 8).unwrap();
                assert_eq!(dst.as_slice(), expected, "{layout:?} {color:?} {rgb:?}");
            }
        }
    }
}

#[test]
fn random_pictures_odd_dimensions_padding_and_zero_strides() {
    for (w, h) in [(1, 1), (3, 3), (5, 2), (2, 5), (62, 34)] {
        let picture = Picture::random(w, h);
        for color in COLORS {
            let expected = picture.expected(color);
            for pad in [0, 7] {
                let ds = w * 4 + pad;
                let mut previous = None;
                for layout in LAYOUTS {
                    let (data, mut strides) = picture.storage(layout, pad);
                    if pad == 0 {
                        strides.fill(0);
                    }
                    let p = Planes::separate(layout, &borrowed(&data, &strides)).unwrap();
                    let mut dst = vec![0xa5; ds * (h - 1) + w * 4];
                    convert_yuv(
                        layout,
                        &p,
                        w as u32,
                        h as u32,
                        color,
                        &mut dst,
                        if pad == 0 { 0 } else { ds },
                    )
                    .unwrap();
                    let tight: Vec<_> = dst
                        .chunks(ds)
                        .flat_map(|r| r[..w * 4].iter().copied())
                        .collect();
                    for (i, (&a, &e)) in tight.iter().zip(&expected).enumerate() {
                        assert!(
                            a.abs_diff(e) <= 1,
                            "{layout:?} {color:?} {w}x{h} byte {i}: {a} != {e}"
                        );
                        if i % 4 == 3 {
                            assert_eq!(a, 255);
                        }
                    }
                    for row in dst.chunks(ds).take(h - 1) {
                        assert!(row[w * 4..].iter().all(|&b| b == 0xa5));
                    }
                    if let Some(p) = previous {
                        assert_eq!(tight, p);
                    }
                    previous = Some(tight);
                }
            }
        }
    }
}

#[test]
fn fixed_point_is_within_one_for_every_eight_bit_triple() {
    // Exhaustive coefficient proof, including luma/chroma outside nominal range.
    for color in COLORS {
        let c = Coefficients::new(color);
        for y in 0..=255 {
            for u in 0..=255 {
                for v in 0..=255 {
                    let got = c.pixel(y, u, v);
                    let expected = reference(y, u, v, color);
                    for i in 0..4 {
                        assert!(
                            got[i].abs_diff(expected[i]) <= 1,
                            "{color:?} {y} {u} {v}: {got:?} != {expected:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn contiguous_matches_separate_with_odd_height_and_padded_stride() {
    for layout in LAYOUTS {
        for (w, h, stride) in [(3, 3, 8usize), (5, 2, 16), (2, 5, 12)] {
            let pic = Picture::random(w, h);
            let row = if matches!(layout, YuvLayout::Yuy2 | YuvLayout::Uyvy) {
                w.div_ceil(2) * 4
            } else {
                w
            };
            let (mut data, _) = pic.storage(layout, 0);
            let strides = match layout {
                YuvLayout::Yuy2 | YuvLayout::Uyvy => vec![stride],
                YuvLayout::Nv12 => vec![stride, stride],
                YuvLayout::I420 => vec![stride, stride.div_ceil(2), stride.div_ceil(2)],
            };
            let rows = match layout {
                YuvLayout::Yuy2 | YuvLayout::Uyvy => vec![(row, h)],
                YuvLayout::Nv12 => vec![(w, h), (w.div_ceil(2) * 2, h.div_ceil(2))],
                YuvLayout::I420 => vec![
                    (w, h),
                    (w.div_ceil(2), h.div_ceil(2)),
                    (w.div_ceil(2), h.div_ceil(2)),
                ],
            };
            for i in 0..data.len() {
                let (row, n) = rows[i];
                let mut padded = vec![0xa5; strides[i] * n];
                for (src, dst) in data[i].chunks(row).zip(padded.chunks_mut(strides[i])) {
                    dst[..row].copy_from_slice(src);
                }
                data[i] = padded;
            }
            let mut joined: Vec<_> = data.iter().flatten().copied().collect();
            let last = data.len() - 1;
            joined.truncate(joined.len() - (strides[last] - rows[last].0));
            let contiguous = Planes::contiguous(layout, &joined, stride, h as u32).unwrap();
            let separate = Planes::separate(layout, &borrowed(&data, &strides)).unwrap();
            let mut a = vec![0; w * h * 4];
            let mut b = a.clone();
            convert_yuv(
                layout,
                &contiguous,
                w as u32,
                h as u32,
                COLORS[0],
                &mut a,
                0,
            )
            .unwrap();
            convert_yuv(layout, &separate, w as u32, h as u32, COLORS[0], &mut b, 0).unwrap();
            assert_eq!(a, b);
        }
    }
}

#[test]
fn errors_validate_every_plane_and_destination_before_writing() {
    for layout in LAYOUTS {
        let pic = Picture::random(3, 3);
        let (data, strides) = pic.storage(layout, 5);
        let planes = borrowed(&data, &strides);
        let p = Planes::separate(layout, &planes).unwrap();
        let mut dst = vec![0xa5; 44]; // stride 16, final row 12 bytes
        for (w, h) in [(0, 3), (3, 0)] {
            assert_eq!(
                convert_yuv(layout, &p, w, h, COLORS[0], &mut dst, 16),
                Err(ConvertError::EmptyPicture)
            );
        }
        assert_eq!(
            convert_yuv(layout, &p, 3, 3, COLORS[0], &mut dst, 11),
            Err(ConvertError::StrideTooSmall {
                what: "dst",
                stride: 11,
                row: 12
            })
        );
        assert_eq!(
            convert_yuv(layout, &p, 3, 3, COLORS[0], &mut dst[..43], 16),
            Err(ConvertError::BufferTooSmall {
                what: "dst",
                needed: 44,
                got: 43
            })
        );
        for i in 0..planes.len() {
            let mut short = planes.clone();
            short[i].data = &short[i].data[..short[i].data.len() - 1];
            let p = Planes::separate(layout, &short).unwrap();
            assert!(matches!(
                convert_yuv(layout, &p, 3, 3, COLORS[0], &mut dst, 16),
                Err(ConvertError::BufferTooSmall { .. })
            ));
            short[i].data = &[];
            let p = Planes::separate(layout, &short).unwrap();
            assert!(matches!(
                convert_yuv(layout, &p, 3, 3, COLORS[0], &mut dst, 16),
                Err(ConvertError::BufferTooSmall { .. })
            ));
            let mut overflow_stride = planes.clone();
            overflow_stride[i].stride = usize::MAX;
            let p = Planes::separate(layout, &overflow_stride).unwrap();
            assert!(matches!(
                convert_yuv(layout, &p, 3, 3, COLORS[0], &mut dst, 16),
                Err(ConvertError::SizeOverflow { .. })
            ));
            let mut small_stride = planes.clone();
            let row = match (layout, i) {
                (YuvLayout::Yuy2 | YuvLayout::Uyvy, _) => 8,
                (_, 0) => 3,
                (YuvLayout::Nv12, _) => 4,
                _ => 2,
            };
            small_stride[i].stride = row - 1;
            let p = Planes::separate(layout, &small_stride).unwrap();
            assert!(matches!(
                convert_yuv(layout, &p, 3, 3, COLORS[0], &mut dst, 16),
                Err(ConvertError::StrideTooSmall { .. })
            ));
        }
        for count in [0, layout.plane_count() + 1] {
            let bad = vec![
                Plane {
                    data: &[],
                    stride: 0
                };
                count
            ];
            assert_eq!(
                Planes::separate(layout, &bad).unwrap_err(),
                ConvertError::PlaneCount {
                    layout,
                    expected: layout.plane_count(),
                    got: count
                }
            );
        }
        assert!(convert_yuv(layout, &p, u32::MAX, u32::MAX, COLORS[0], &mut dst, 0).is_err());
        assert!(matches!(
            convert_yuv(layout, &p, 3, 3, COLORS[0], &mut dst, usize::MAX),
            Err(ConvertError::SizeOverflow { .. })
        ));
        assert!(dst.iter().all(|&b| b == 0xa5));
    }
    let p = Planes::separate(
        YuvLayout::Yuy2,
        &[Plane {
            data: &[],
            stride: 0,
        }],
    )
    .unwrap();
    assert_eq!(
        convert_yuv(YuvLayout::Nv12, &p, 1, 1, COLORS[0], &mut [0; 4], 0),
        Err(ConvertError::PlaneCount {
            layout: YuvLayout::Nv12,
            expected: 2,
            got: 1
        })
    );
}

#[test]
fn contiguous_errors_and_overflow() {
    for layout in LAYOUTS {
        assert_eq!(
            Planes::contiguous(layout, &[], 0, 0).unwrap_err(),
            ConvertError::EmptyPicture
        );
        if layout.plane_count() > 1 {
            assert!(matches!(
                Planes::contiguous(layout, &[], 0, 3),
                Err(ConvertError::StrideTooSmall { .. })
            ));
            assert!(matches!(
                Planes::contiguous(layout, &[], 8, 3),
                Err(ConvertError::BufferTooSmall { .. })
            ));
            assert!(matches!(
                Planes::contiguous(layout, &[], usize::MAX, u32::MAX),
                Err(ConvertError::SizeOverflow { .. })
            ));
        }
    }
    assert!(matches!(
        Planes::contiguous(YuvLayout::I420, &[0; 24], 8, 3),
        Err(ConvertError::BufferTooSmall { what: "U", .. })
    ));
}
