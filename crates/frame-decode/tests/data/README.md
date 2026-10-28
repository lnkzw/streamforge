# Decoder fixtures

All images use six flat 8×8 blocks in a 24×16 image: red, green, blue across
the top, white, black, yellow across the bottom. JPEG tests inject EXIF in code.
Other PNGs are built in tests with the `png` encoder. Adam7 is a fixture because
that encoder does not interlace pixel data.

Run these commands in a scratch directory for `blocks.ppm`
(set `repo` to the repository root first); no generated PPM belongs in the repo.
The tools used are libjpeg-turbo `cjpeg` and ImageMagick `magick`.

```sh
python3 - <<'PY'
from pathlib import Path
colors = [(255,0,0), (0,255,0), (0,0,255), (255,255,255), (0,0,0), (255,255,0)]
Path('blocks.ppm').write_bytes(b'P6\n24 16\n255\n' + bytes(
    c for y in range(16) for x in range(24) for c in colors[(y//8)*3+x//8]))
PY
data="$repo/crates/frame-decode/tests/data"
/usr/bin/cjpeg -quality 100 -sample 1x1 -outfile "$data/blocks.jpg" blocks.ppm
/usr/bin/cjpeg -quality 100 -sample 1x1 -progressive -outfile "$data/progressive.jpg" blocks.ppm
/usr/bin/cjpeg -quality 100 -grayscale -outfile "$data/gray.jpg" blocks.ppm
/usr/bin/cjpeg -quality 100 -sample 2x2 -outfile "$data/420.jpg" blocks.ppm
/usr/bin/cjpeg -quality 100 -sample 2x1 -outfile "$data/422.jpg" blocks.ppm
/usr/bin/cjpeg -quality 100 -sample 1x1 -arithmetic -outfile "$data/arithmetic.jpg" blocks.ppm
/usr/bin/cjpeg -quality 100 -rgb -outfile "$data/rgb.jpg" blocks.ppm
/usr/bin/magick blocks.ppm -colorspace CMYK "$data/cmyk.jpg"
/usr/bin/magick blocks.ppm -interlace PNG "PNG24:$data/adam7.png"
```

`cmyk.jpg` exercises Adobe YCCK (ImageMagick's CMYK JPEG encoding).
`rgb.jpg` stores RGB without YCbCr.
`arithmetic.jpg` exercises the unsupported-format error. Files are below 1 KiB.
ImageMagick may add creation timestamps; regenerated PNG bytes can differ while
the decoded pixels remain exact.
