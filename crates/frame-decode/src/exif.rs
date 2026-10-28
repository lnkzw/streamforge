//! JPEG EXIF orientation from a TIFF payload (without `Exif\0\0`).

/// Malformed or absent orientation means the stored order.
pub(crate) fn orientation(tiff: &[u8]) -> u8 {
    parse(tiff).unwrap_or(1)
}

fn parse(tiff: &[u8]) -> Option<u8> {
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let short = |offset: usize| {
        let bytes = tiff.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let long = |offset: usize| {
        let bytes = tiff.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    if short(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(long(4)?).ok()?;
    if ifd < 8 {
        return None;
    }
    let count = usize::from(short(ifd)?);
    let start = ifd.checked_add(2)?;
    // Validate the entire IFD, including the next-IFD offset, before using a tag.
    let end = start.checked_add(count.checked_mul(12)?)?.checked_add(4)?;
    tiff.get(start..end)?;
    for index in 0..count {
        let entry = start + index * 12;
        if short(entry)? == 0x0112 {
            if short(entry + 2)? != 3 || long(entry + 4)? != 1 {
                return None;
            }
            let value = u8::try_from(short(entry + 8)?).ok()?;
            return (1..=8).contains(&value).then_some(value);
        }
    }
    None
}
