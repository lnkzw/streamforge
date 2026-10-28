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

## Camera benchmark input

`camera-1080p-422.jpg` is a synthetic 1920×1080 baseline JPEG, quality 85,
4:2:2 (2×1 luma sampling), with standard Huffman tables. It combines smooth
ramps, colour blocks and fine detail; no camera or third-party image is needed.
It is about 63 KiB. From the repository root, set `scratch` to a scratch directory
outside the repository, then regenerate it with:

```sh
python3 - "$scratch/camera.ppm" <<'PY'
import sys
from pathlib import Path
colors = [(235,40,40), (40,210,50), (40,60,230),
          (220,200,40), (40,200,210), (200,40,200)]
Path(sys.argv[1]).write_bytes(b'P6\n1920 1080\n255\n' + bytes(
    (colors[(x//320+y//180)%6][c] + x//16 + y//12 + ((x^y)//32)%24)//2
    for y in range(1080) for x in range(1920) for c in range(3)))
PY
cjpeg -quality 85 -sample 2x1 \
    -outfile crates/frame-decode/tests/data/camera-1080p-422.jpg "$scratch/camera.ppm"
```

`src/mjpeg_dht.bin` is one DHT segment containing JPEG Annex K.3's four
standard Huffman tables (DC/AC, luminance/chrominance). No optimized tables
are used. To reproduce it from the existing `422.jpg` fixture:

```sh
python3 - <<'PY'
from pathlib import Path
root = Path('crates/frame-decode')
b = (root/'tests/data/422.jpg').read_bytes()
o, tables = 2, []
while b[o:o+2] != b'\xff\xda':
    n = int.from_bytes(b[o+2:o+4], 'big')
    if b[o+1] == 0xc4:
        tables.append(b[o+4:o+2+n])
    o += 2+n
body = b''.join(tables)
assert len(body) == 416
(root/'src/mjpeg_dht.bin').write_bytes(
    b'\xff\xc4' + (len(body)+2).to_bytes(2, 'big') + body)
PY
```

Run `cargo run --release -p frame-decode --example convert_bench [frames]`
for warm mean/p99 times. The benchmark decodes this fixture without encoding.
`cargo test -p frame-decode --test alloc -- --nocapture` reports per-frame
allocation requests/bytes after warm-up (including reallocations, excluding
frees; bytes are total requested, not peak live memory). The allocator's unsafe
forwarding exists only in this separate test binary, never in the library.
