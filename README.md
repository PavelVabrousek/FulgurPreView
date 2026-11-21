# FulgurPreView

Ultra-fast, standalone astrophotography and scientific image viewer for Windows (with cross-platform architecture for macOS and Linux). Built in 100% pure Rust as a single portable executable (`fulgur-preview.exe`) requiring zero installation and zero external libraries.

> **Status:** `0.11.0-alpha` — alpha test build for Windows 11 x64.

## Key Capabilities

- **Instant Viewing & Directory Browsing:** Open any image from Windows Explorer and step through the folder with `Left` / `Right` arrows at near-zero latency thanks to background neighbour prefetching and **Blink Mode** (locked zoom, pan, and stretch across subframes).
- **Supported Formats:**
  - **Astronomical:** FITS (`.fit`, `.fits`, `.fts` — `8`, `16`, `32`-bit int, `32` & `64`-bit float, 2D/3D cubes, Multi-Extension FITS) and XISF 1.0 (`.xisf`, `.xish` — LZ4, Zstd, Zlib).
  - **Camera RAW:** DSLR and mirrorless RAWs (`.cr2`, `.cr3`, `.nef`, `.arw`, `.dng`, `.orf`, `.rw2`, `.pef`, `.raf`) via a two-stage pipeline (instant embedded preview followed by full linear CFA decoding). Linear CFA decoding currently covers uncompressed / packed RAWs, lossless-JPEG RAWs (Canon CR2, DNG), Deflate DNG and Sony ARW2; for other variants (Canon CR3, compressed Nikon NEF, compressed RAF / RW2 / ORF / PEF) only the embedded JPEG is shown, clearly labelled, and star metrics are disabled.
  - **Standard Images:** TIFF (`8`/`16`/`32`-bit), JPEG, PNG, WebP, BMP.
- **Fast Debayering & MTF Stretch:**
  - Fast 2×2 superpixel / MHC demosaicing for full-frame overviews and AVX2-accelerated VNG demosaicing for 1:1 native detail crops.
  - Automatic **MTF Auto-Stretch** (Linked and Unlinked RGB) with manual black/midtone/white sliders for linear astronomical and RAW data; standard rendered images (JPEG/PNG/8-bit TIFF) display unmodified.
- **Signal-Focused Histogram:**
  - Computed on linear pixel data with an automatic **"Zoom to Signal"** mode that focuses on the sky background and nebulosity peak, plus dedicated `Min` / `Max` sliders directly in the histogram panel (leaving the mouse wheel strictly reserved for zooming the image).
- **Real-Time Star Quality & 3×3 Tilt Analysis:**
  - Background star detection computing median **FWHM**, **HFR**, **eccentricity**, **sky level**, and **noise** on linear data (FITS, XISF, camera RAW, 16/32-bit TIFF); 8-bit processed exports (JPEG/PNG/8-bit TIFF) show a note instead of misleading star metrics.
  - Live cursor inspector (pixel X/Y, linear ADU, and local star FWHM/HFR/eccentricity on hover).
  - **3×3 Corner & Tilt Map** showing FWHM and eccentricity across 9 sensor zones.
  - **Field Curvature & Collimation Map** (`C`): fitted FWHM surface over the whole frame with min/max FWHM, curvature, tilt in X/Y, total tilt and direction, collimation offset of the sharpest point, and stars used.
  - **Eccentricity Map** (`E`): fitted star-elongation surface with centre, corners and most elongated corner, plus elongation-direction whiskers and a direction-coherence hint (tracking vs optics).
  - **px / arcsec** toggle for FWHM, HFR and tilt values (arcsec when the frame is plate-solved or the header gives the pixel scale).
- **Local Gaia DR3 Plate Solving & Sky Overlays:**
  - Plate solving never runs automatically: `S` / Solve first shows a WCS already stored in the file header (no computation), and solves on demand using local **Gaia DR3** catalogues (`.xpsd` / cache) and the `1.12 GB` blind-solving quad index shared with `FulgurStack`.
  - Displays live cursor RA/Dec, FoV, pixel scale (`″/px`), position angle, celestial RA/Dec grid, N/E compass, and DSO/star annotations, with optional atomic saving of solved WCS back to FITS/XISF headers.
- **Asteroid & Comet Identification (online, on demand):**
  - Right-click a plate-solved frame → identify known asteroids and comets within 25′ of the cursor down to V 20 for the header observation time (NASA/JPL Small-Body Identification, MPC orbits); results are drawn with names, magnitudes and motion arrows.
- **Subframe Culling:**
  - Press `X` or `Delete` to move rejected subframes into a `_Rejected` subfolder with `Ctrl+Z` Undo support.

## Documentation

- [Project Specification & Milestones (M1–M5)](PROJECT_SPEC.md)
- [Architecture Overview](docs/ARCHITECTURE.md)
- [Engineering Contract](AGENTS.md)
