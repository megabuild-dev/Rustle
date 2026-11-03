# Rustle brand assets

A leaf lifted by the wind: light enough to ride on a 1 GB server. "Rustle" is also the sound leaves make.

| File | Use |
|---|---|
| `logo-mark.svg` | The leaf alone, transparent; works on light and dark backgrounds |
| `logo-mark-mono.svg` | One colour (`currentColor`), for print or single-colour contexts |
| `logo-horizontal.svg` / `logo-horizontal-dark.svg` | Leaf + wordmark for light / dark backgrounds (text is outlined, no font needed) |
| `icon.svg`, `favicon.svg` | App icon on the dashboard navy; `favicon.svg` is also the dashboard favicon |
| `icon-512.png`, `icon-192.png`, `apple-touch-icon.png`, `favicon-32.png`, `logo-mark-512.png` | Raster exports |
| `social-preview.png` | 1280 x 640 image for the repository (Settings, Social preview) and link cards |

## Colours

| | |
|---|---|
| Leaf gradient | `#10b981` to `#5eead4` |
| Dashboard accent | `#34d399` |
| Navy | `#0b1020` |
| Ink on light / dark | `#0b1020` / `#f1f5f9` |

Typeface: [Inter](https://rsms.me/inter/) (SIL Open Font License). Keep clear space around the mark of at least the width of the
widest wind stroke, and do not recolour the leaf outside the palette.

## Regenerating

```bash
python3 -m pip install fonttools
python3 assets/brand/generate.py     # SVGs
node assets/brand/render.js          # PNGs and the social preview (playwright, Inter installed)
```

The social preview must be uploaded by hand: repository Settings, General, Social preview (GitHub has no API for it).
