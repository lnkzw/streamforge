//! Borrowed camera planes and allocation-free YUV conversion.

/// YUV layouts a camera delivers (FourCC names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvLayout {
    /// Packed 4:2:2, bytes Y0 U Y1 V.
    Yuy2,
    /// Packed 4:2:2, bytes U Y0 V Y1.
    Uyvy,
    /// 4:2:0, a Y plane and one interleaved UV plane (U first).
    Nv12,
    /// 4:2:0, Y, U and V planes.
    I420,
}

impl YuvLayout {
    /// Number of planes: 1 for Yuy2/Uyvy, 2 for Nv12, 3 for I420.
    pub fn plane_count(self) -> usize {
        match self {
            Self::Yuy2 | Self::Uyvy => 1,
            Self::Nv12 => 2,
            Self::I420 => 3,
        }
    }
}

/// Luma coefficients used by a camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMatrix {
    #[default]
    Bt601,
    Bt709,
}

/// Encoded luma and chroma ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorRange {
    #[default]
    Limited,
    Full,
}

/// How YUV maps to RGB. Unspecified camera colour is BT.601 limited (ADR 0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct YuvColor {
    pub matrix: ColorMatrix,
    pub range: ColorRange,
}

/// One plane: bytes from the first row on, and distance between rows in bytes.
#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

/// Up to three borrowed planes, without allocating.
#[derive(Debug, Clone, Copy)]
pub struct Planes<'a> {
    planes: [Plane<'a>; 3],
    count: usize,
}

impl<'a> Planes<'a> {
    /// Separately supplied planes (e.g. one PipeWire `spa_data` per plane).
    /// The count must equal `layout.plane_count()`; sizes are checked at conversion.
    pub fn separate(layout: YuvLayout, planes: &[Plane<'a>]) -> Result<Self, ConvertError> {
        check_count(layout, planes.len())?;
        let mut result = Self {
            planes: [Plane {
                data: &[],
                stride: 0,
            }; 3],
            count: planes.len(),
        };
        result.planes[..planes.len()].copy_from_slice(planes);
        Ok(result)
    }

    /// Planes stored consecutively (V4L2 single-planar buffers). NV12 UV uses
    /// `luma_stride`; I420 U/V use `luma_stride.div_ceil(2)`. Chroma has
    /// `height.div_ceil(2)` rows. Packed layouts use `luma_stride` as row stride.
    /// Intermediate planes occupy full strides, including final-row padding;
    /// the final plane may end after its last row's samples.
    /// Planar layouts require a nonzero stride: there is no width here to infer it.
    pub fn contiguous(
        layout: YuvLayout,
        data: &'a [u8],
        luma_stride: usize,
        height: u32,
    ) -> Result<Self, ConvertError> {
        if height == 0 {
            return Err(ConvertError::EmptyPicture);
        }
        if matches!(layout, YuvLayout::Yuy2 | YuvLayout::Uyvy) {
            return Self::separate(
                layout,
                &[Plane {
                    data,
                    stride: luma_stride,
                }],
            );
        }
        if luma_stride == 0 {
            return Err(ConvertError::StrideTooSmall {
                what: "Y",
                stride: 0,
                row: 1,
            });
        }
        let y_end = mul(luma_stride, height as usize, "Y")?;
        require(data, y_end, "Y")?;
        let y = Plane {
            data: &data[..y_end],
            stride: luma_stride,
        };
        if layout == YuvLayout::Nv12 {
            return Self::separate(
                layout,
                &[
                    y,
                    Plane {
                        data: &data[y_end..],
                        stride: luma_stride,
                    },
                ],
            );
        }
        let stride = luma_stride.div_ceil(2);
        let u_len = mul(stride, height.div_ceil(2) as usize, "U")?;
        let u_end = y_end
            .checked_add(u_len)
            .ok_or(ConvertError::SizeOverflow { what: "U" })?;
        require(data, u_end, "U")?;
        Self::separate(
            layout,
            &[
                y,
                Plane {
                    data: &data[y_end..u_end],
                    stride,
                },
                Plane {
                    data: &data[u_end..],
                    stride,
                },
            ],
        )
    }
}

/// A camera picture could not be converted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// Zero width or height.
    #[error("zero width or height")]
    EmptyPicture,
    /// A stride below one row of samples.
    #[error("{what} stride {stride} is below row size {row}")]
    StrideTooSmall {
        what: &'static str,
        stride: usize,
        row: usize,
    },
    /// A plane or destination shorter than the final row requires.
    #[error("{what} needs {needed} bytes, got {got}")]
    BufferTooSmall {
        what: &'static str,
        needed: usize,
        got: usize,
    },
    /// Wrong number of planes for the selected layout.
    #[error("{layout:?} needs {expected} planes, got {got}")]
    PlaneCount {
        layout: YuvLayout,
        expected: usize,
        got: usize,
    },
    /// Stored MJPEG size differs from the negotiated picture.
    #[error("expected MJPEG size {expected:?}, got {got:?}")]
    SizeMismatch {
        expected: (u32, u32),
        got: (u32, u32),
    },
    /// Invalid/unsupported JPEG; retains the decoder message.
    #[error("JPEG: {0}")]
    Jpeg(String),
    /// Picture arithmetic exceeds addressable memory.
    #[error("{what} size overflows addressable memory")]
    SizeOverflow { what: &'static str },
}

pub(crate) fn mul(a: usize, b: usize, what: &'static str) -> Result<usize, ConvertError> {
    a.checked_mul(b)
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or(ConvertError::SizeOverflow { what })
}

pub(crate) fn require(data: &[u8], needed: usize, what: &'static str) -> Result<(), ConvertError> {
    if data.len() < needed {
        Err(ConvertError::BufferTooSmall {
            what,
            needed,
            got: data.len(),
        })
    } else {
        Ok(())
    }
}

fn check_count(layout: YuvLayout, got: usize) -> Result<(), ConvertError> {
    let expected = layout.plane_count();
    if got != expected {
        Err(ConvertError::PlaneCount {
            layout,
            expected,
            got,
        })
    } else {
        Ok(())
    }
}

fn validate(
    plane: Plane<'_>,
    row: usize,
    rows: usize,
    what: &'static str,
) -> Result<usize, ConvertError> {
    let stride = if plane.stride == 0 { row } else { plane.stride };
    if stride < row {
        return Err(ConvertError::StrideTooSmall { what, stride, row });
    }
    let needed = mul(stride, rows - 1, what)?
        .checked_add(row)
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or(ConvertError::SizeOverflow { what })?;
    require(plane.data, needed, what)?;
    Ok(stride)
}

// Q16 coefficients rounded from Kr/Kb, 255/219 and 255/224. The sum
// stays below 2^27 even for out-of-range samples; i32 intermediates are safe.
// Quantization error is < 0.004 channel levels for every 8-bit Y/U/V triple.
struct Coefficients {
    y: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
    offset: i32,
}

impl Coefficients {
    fn new(color: YuvColor) -> Self {
        let (y, rv, gu, gv, bu) = match (color.matrix, color.range) {
            (ColorMatrix::Bt601, ColorRange::Limited) => (76309, 104597, -25675, -53279, 132201),
            (ColorMatrix::Bt601, ColorRange::Full) => (65536, 91881, -22553, -46802, 116130),
            (ColorMatrix::Bt709, ColorRange::Limited) => (76309, 117489, -13975, -34925, 138438),
            (ColorMatrix::Bt709, ColorRange::Full) => (65536, 103206, -12276, -30679, 121609),
        };
        Self {
            y,
            rv,
            gu,
            gv,
            bu,
            offset: if color.range == ColorRange::Limited {
                16
            } else {
                0
            },
        }
    }

    #[inline(always)]
    fn pixel(&self, y: u8, u: u8, v: u8) -> [u8; 4] {
        let y = (i32::from(y) - self.offset) * self.y + (1 << 15);
        let u = i32::from(u) - 128;
        let v = i32::from(v) - 128;
        let channel = |n: i32| (n >> 16).clamp(0, 255) as u8;
        [
            channel(y + self.bu * u),
            channel(y + self.gu * u + self.gv * v),
            channel(y + self.rv * v),
            255,
        ]
    }
}

/// Convert YUV to BGRX (X = 255), with nearest-sample chroma, no allocation.
/// Zero plane/destination strides mean tightly packed. All bounds are checked
/// before writing; final rows need only their samples, not trailing padding.
/// Odd dimensions use the last pair's chroma. Short/overflowing input errors.
pub fn convert_yuv(
    layout: YuvLayout,
    planes: &Planes<'_>,
    width: u32,
    height: u32,
    color: YuvColor,
    dst: &mut [u8],
    dst_stride: usize,
) -> Result<(), ConvertError> {
    if width == 0 || height == 0 {
        return Err(ConvertError::EmptyPicture);
    }
    check_count(layout, planes.count)?;
    let (w, h) = (width as usize, height as usize);
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let dst_row = mul(w, 4, "dst")?;
    let ds = validate(
        Plane {
            data: dst,
            stride: dst_stride,
        },
        dst_row,
        h,
        "dst",
    )?;
    let mut strides = [0; 3];
    let rows = match layout {
        YuvLayout::Yuy2 | YuvLayout::Uyvy => {
            [(mul(cw, 4, "packed")?, h, "packed"), (0, 0, ""), (0, 0, "")]
        }
        YuvLayout::Nv12 => [(w, h, "Y"), (mul(cw, 2, "UV")?, ch, "UV"), (0, 0, "")],
        YuvLayout::I420 => [(w, h, "Y"), (cw, ch, "U"), (cw, ch, "V")],
    };
    for i in 0..planes.count {
        strides[i] = validate(planes.planes[i], rows[i].0, rows[i].1, rows[i].2)?;
    }
    let c = Coefficients::new(color);
    // Dispatch outside the row loops; packed byte ordering is monomorphized.
    match layout {
        YuvLayout::Yuy2 => packed::<false>(planes.planes[0].data, strides[0], w, h, dst, ds, &c),
        YuvLayout::Uyvy => packed::<true>(planes.planes[0].data, strides[0], w, h, dst, ds, &c),
        YuvLayout::Nv12 | YuvLayout::I420 => {
            for y in 0..h {
                let ys = &planes.planes[0].data[y * strides[0]..][..w];
                let out = &mut dst[y * ds..][..dst_row];
                let chroma = y / 2;
                if layout == YuvLayout::Nv12 {
                    let uv = &planes.planes[1].data[chroma * strides[1]..][..cw * 2];
                    for ((yy, uv), out) in ys
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .zip(uv.as_chunks::<2>().0)
                        .zip(out.as_chunks_mut::<8>().0)
                    {
                        out[..4].copy_from_slice(&c.pixel(yy[0], uv[0], uv[1]));
                        out[4..].copy_from_slice(&c.pixel(yy[1], uv[0], uv[1]));
                    }
                    if !w.is_multiple_of(2) {
                        out[dst_row - 4..].copy_from_slice(&c.pixel(
                            ys[w - 1],
                            uv[cw * 2 - 2],
                            uv[cw * 2 - 1],
                        ));
                    }
                } else {
                    let us = &planes.planes[1].data[chroma * strides[1]..][..cw];
                    let vs = &planes.planes[2].data[chroma * strides[2]..][..cw];
                    for (((yy, &u), &v), out) in ys
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .zip(us)
                        .zip(vs)
                        .zip(out.as_chunks_mut::<8>().0)
                    {
                        out[..4].copy_from_slice(&c.pixel(yy[0], u, v));
                        out[4..].copy_from_slice(&c.pixel(yy[1], u, v));
                    }
                    if !w.is_multiple_of(2) {
                        out[dst_row - 4..].copy_from_slice(&c.pixel(
                            ys[w - 1],
                            us[cw - 1],
                            vs[cw - 1],
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn packed<const UYVY: bool>(
    data: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    dst: &mut [u8],
    ds: usize,
    c: &Coefficients,
) {
    for y in 0..h {
        let input = &data[y * stride..][..w.div_ceil(2) * 4];
        let out = &mut dst[y * ds..][..w * 4];
        for (pair, out) in input
            .as_chunks::<4>()
            .0
            .iter()
            .zip(out.as_chunks_mut::<8>().0)
        {
            let [y0, u, y1, v] = if UYVY {
                [pair[1], pair[0], pair[3], pair[2]]
            } else {
                *pair
            };
            out[..4].copy_from_slice(&c.pixel(y0, u, v));
            out[4..].copy_from_slice(&c.pixel(y1, u, v));
        }
        if !w.is_multiple_of(2) {
            let pair = &input[input.len() - 4..];
            let (y, u, v) = if UYVY {
                (pair[1], pair[0], pair[2])
            } else {
                (pair[0], pair[1], pair[3])
            };
            out[w * 4 - 4..].copy_from_slice(&c.pixel(y, u, v));
        }
    }
}

#[cfg(test)]
mod tests;
