# FulgurPreView — Project Specification & Implementation Contract (v1.0)

**Version:** 1.0  
**Date:** 2026-11-13  
**Language:** English (source code, identifiers, documentation, specifications); Czech for owner communication.  
**Primary Platform:** Windows 11 x86_64 (single portable `.exe` with statically linked CRT, zero installation, zero external C/C++ runtime DLLs). Designed for clean compilation and future releases on macOS Apple Silicon (`aarch64-apple-darwin`) and Linux (`x86_64-unknown-linux-gnu`).  
**Repository Policy:** 100% standalone Rust workspace. Relevant modules from `FulgurStack` (`fits`, `xisf`, `cameras.json`, `derive`, `debayer`/`vng`, `stars`, `sky`, `fulgur-catalog`, `fulgur-solve`) are copied and adapted directly into `FulgurPreView` crates so the two repositories have no cross-directory Cargo dependencies.

---

## 1. Executive Summary & Product Vision

**FulgurPreView** is an ultra-fast, zero-install desktop image viewer and real-time diagnostic tool built specifically for astrophotographers. Its primary workflow is:
1. **Instant Launch:** Double-clicking an astronomical or standard image file in Windows Explorer (or passing a file/folder on the command line) opens the image in milliseconds.
2. **Instant Directory Navigation & Blinking:** Pressing `Left` / `Right` arrows steps through all supported images in the same directory with near-zero latency (`< 10 ms` on prefetched neighbours), optionally locking zoom, pan, and stretch parameters (**Blink Mode**) to compare subframes effortlessly.
3. **Comprehensive Format Support:**
   - **Astronomical formats:** FITS (`.fit`, `.fits`, `.fts` — `BITPIX` `8`, `16`, `32`, `-32`, `-64`, 2D/3D cubes, and Multi-Extension FITS / MEF) and XISF 1.0 (`.xisf`, `.xish` — monolithic and distributed, all codecs: LZ4/LZ4HC, Zstd, Zlib).
   - **Camera RAW formats:** DSLR and mirrorless RAWs (`.cr2`, `.cr3`, `.nef`, `.arw`, `.dng`, `.orf`, `.rw2`, `.pef`, `.raf`) via a pure-Rust decoder (`rawler`) with a two-stage loading pipeline (instant embedded JPEG/fast preview followed by full linear CFA decoding in the background).
   - **Standard image formats:** TIFF (`8`/`16`/`32`-bit), JPEG, PNG, WebP, BMP.
4. **Real-Time Scientific Diagnostics:**
   - **Histogram with Signal Zoom:** Interactive histogram computed on linear data with an automatic **"Zoom to Signal"** mode that focuses the X-axis on the sky-background and nebulosity peak, plus dedicated `Min` / `Max` range sliders directly in the histogram panel.
   - **Star Quality & Sensor Tilt:** Automatic background computation of frame-wide median **FWHM**, **HFR**, **eccentricity**, **sky background**, **noise**, and **star count**, plus a live cursor inspector (pixel X/Y, R/G/B values, and single-star PSF metrics on hover) and a **3×3 Corner/Tilt Map** (FWHM and eccentricity across 9 sensor zones).
   - **Astrometry / Plate Solving:** Instant WCS readout if present in headers, or on-demand local plate solving using local **Gaia DR3** catalogues (`.xpsd` / cached tiles shared with `FulgurStack`) with automatic fallback to the `1.12 GB` **blind-solving quad index**. Provides live RA/Dec under cursor, field dimensions, pixel scale (`″/px`), position angle, celestial grid overlay, N/E compass, and DSO/star annotations, plus explicit atomic saving of solved WCS back to FITS/XISF headers.
   - **Fast Subframe Culling:** Single-key rejection (`X` or `Delete`) moves poor subframes into a `_Rejected` subfolder in the current directory with full `Ctrl+Z` Undo support.

---

## 2. Core Architectural & Domain Rules (Binding Guardrails)

### 2.1 Standalone Pure-Rust Portability
- **Single `.exe` Distribution:** Release builds on Windows MUST use static CRT linking (`-C target-feature=+crt-static`) and import only standard OS system DLLs. No external installer, runtime, or DLL folder is permitted.
- **Pure Rust Only:** No C/C++ wrapper crates (`cfitsio`, `libraw-sys`, `opencv`) are allowed. All FITS, XISF, RAW, and standard image decoding must compile with standard `cargo build --release`.

### 2.2 File Safety & Explicit Disk Operations
- **Read-Only by Default:** Merely opening, inspecting, stretching, or plate-solving an image MUST NEVER modify, touch, or lock the source file on disk.
- **Two Explicit User-Triggered Disk Actions Only:**
  1. **Subframe Culling (`X` / `Delete`):** Moves the currently viewed file into `<current_dir>/_Rejected/<filename>` (creating `_Rejected` if needed) and advances immediately to the next image. Pressing `Ctrl+Z` moves the last rejected file back to its original path and selects it. Never overwrite an existing file in `_Rejected` without a safe suffix.
  2. **Save WCS to Header (Explicit Button):** Available only for FITS and XISF files after a valid plate solution is obtained. Writes the updated header/file to `<filename>.tmp`, flushes to disk, and atomically replaces the original file.

### 2.3 Two-Tier Debayering & Two-Stage RAW Loading
- **Bayer Pattern Identification:** Determine CFA layout from primary headers (`BAYERPAT`, XISF CFA properties, RAW CFA metadata), header aliases (`COLORTYP`, `CFAPAT`), filename tokens, or the embedded camera database (`cameras.json` shared from `FulgurStack`), with manual override (`Auto`, `Mono`, `RGGB`, `GRBG`, `GBRG`, `BGGR`) in the top bar.
- **Fast Overview vs. 1:1 Native Detail:**
  - Full-frame overview textures (`PREVIEW_EDGE = 2048` px) use ultra-fast **2×2 Superpixel** or **MHC** demosaicing so full-screen display is ready within milliseconds.
  - Zooming in (`>= 1:1` detail viewport) asynchronously computes the visible region of interest (ROI) with **AVX2-accelerated VNG** demosaicing and hot-pixel suppression.
- **Two-Stage DSLR RAW Pipeline:**
  - **Stage 1 (Instant, `< 20 ms`):** Extract and display the embedded full-size JPEG preview (or fast half-resolution superpixel) immediately so rapid arrow-key browsing never stalls.
  - **Stage 2 (Background Worker):** Decode the raw 12/14/16-bit sensor CFA plane into linear `f32` ADU, apply the camera's Bayer pattern, compute the linear MTF auto-stretch, histogram, and star quality metrics, and seamlessly upgrade the viewport to the linear astro view (indicated by a subtle `RAW Linear` badge in the status bar).

### 2.4 Display Stretch & Blink Mode Rules
- **Format-Aware Auto-Stretch:**
  - **Linear Astronomical & RAW files** (`.fit`, `.fits`, `.fts`, `.xisf`, `.xish`, DSLR RAWs, and 16/32-bit linear TIFFs with low median): **MTF Auto-Stretch** is **ON by default**.
  - **Standard Rendered Images** (`.jpg`, `.jpeg`, `.png`, `.webp`, `.bmp`, standard 8-bit `.tif`): Displayed **1:1 as stored (Auto-Stretch OFF by default)**.
- **Stretch Controls:**
  - Supports **Linked RGB** (single combined MTF stretch preserving colour balance) and **Unlinked RGB** (independent per-channel MTF stretch neutralizing sky gradients/filters), plus manual **Black Point**, **Midtones**, and **White Point** sliders.
- **Blink Mode (View Lock):**
  - A top-bar toggle **"Lock View (Blink)"** is **ON by default** when stepping between images of identical pixel dimensions.
  - When active, stepping with `Left` / `Right` preserves the exact zoom factor, pan center, and stretch mode so stars and defects can be compared in place.

### 2.5 Mouse Wheel & Histogram Interaction Contract
- **Mouse Wheel Reservation (CRITICAL OWNER RULE):**
  - The mouse scroll wheel is **ALWAYS reserved for zooming the image viewport** (centered on the cursor position). Scrolling the wheel must never be hijacked by the histogram or side panels to change histogram bounds.
- **Histogram Panel Controls:**
  - Computed from the underlying **linear pixel data** (with an optional toggle to overlay the MTF transfer curve or view post-stretch distribution).
  - Provides a **"Zoom to Signal"** mode (default ON for linear astro images) that automatically clips the horizontal X-axis display range to the active signal region `[black_floor .. median + N * sigma]` where sky background and nebulosity live.
  - Provides explicit **`Min` and `Max` horizontal sliders** directly inside the histogram panel for manual X-axis zooming/panning, plus a **Linear / Log Y-axis** toggle.

### 2.6 Memory Ceiling & Neighbour Prefetching
- **Bounded Prefetch Cache:** While displaying image `i` in the directory list, background threads automatically prefetch `i + 1` and `i - 1` (and up to `±2` when RAM permits) into a bounded LRU cache (`<= 2 GiB` RAM ceiling).
- **Instant Cancellation:** Stepping rapidly through a folder immediately cancels obsolete background decode, 1:1 VNG detail, star-analysis, or RAW Stage-2 tasks so the currently selected frame always gets full CPU priority.

---

## 3. Feature Specifications

### 3.1 Metadata Inspector
- Displays all available metadata grouped cleanly with an instant **Search / Filter** box:
  - **Summary Card:** Camera/Instrument, Telescope, Object, Exposure, Gain/ISO, Filter, Sensor Temperature, Dimensions, Bit Depth / Sample Format, Bayer Pattern (with source provenance: header vs. derived from `cameras.json`), and Capture Timestamp (`DATE-OBS`).
  - **Full Header Table:** All FITS 80-byte cards (key, value, comment), XISF properties, or EXIF tags for RAW/JPEG/TIFF.
  - **MEF / Cube Selector:** When a FITS or XISF file contains multiple image extensions (HDUs) or planes, provides a quick HDU/plane switcher.
  - **Privacy:** Redacts `SITELAT` / `SITELONG` in exported text/clipboard summaries unless explicitly revealed by the user.

### 3.2 Real-Time Star Quality, Cursor Inspector & 3×3 Tilt Map
- **Background Frame Analysis:** Automatically runs on the linear green-CFA or mono plane after image load (without blocking display):
  - Detects stars with hot-pixel suppression (adapted from `fulgur-pipeline::stars`).
  - Reports frame-wide median **FWHM (px and arcsec when pixel scale is known)**, **HFR (px)**, **Eccentricity**, **Star Count**, **Sky Background (ADU)**, and **Sky Noise (ADU)**.
- **Live Cursor Inspector:**
  - Status bar and/or floating HUD shows image coordinates `(X, Y)`, raw linear ADU values `(R, G, B)` or `Mono`, and normalized `[0..1]` values.
  - Hovering near a detected star snaps to the star and shows its local **FWHM**, **HFR**, **Eccentricity**, peak ADU, and radial profile preview.
- **3×3 Sensor Tilt & Corner Map:**
  - Divides the sensor into a `3 × 3` grid (Top-Left, Top, Top-Right, Left, Center, Right, Bottom-Left, Bottom, Bottom-Right).
  - Displays median **FWHM**, **HFR**, **Eccentricity**, and star count in each of the 9 cells, color-coded by deviation from the center cell to immediately reveal coma, field curvature, or sensor tilt.

### 3.3 Astrometry / Plate Solving & Sky Overlays
- **Shared Catalogue Configuration:**
  - Automatically reads `%LOCALAPPDATA%\FulgurStack` settings to reuse configured paths for **PixInsight Gaia DR3 `.xpsd` files**, the local **Gaia download cache**, and the **`1.12 GB` blind-solving quad index** (`fulgur-solve`), while allowing independent configuration in `FulgurPreView` settings.
- **Solving Workflow:**
  1. **Header WCS:** If the opened FITS/XISF already carries a valid WCS (`CRVAL1/2`, `CD` or `PC`+`CDELT`, SIP/PV polynomial), parse it on open at zero cost and use its plate scale for star metrics, but keep all sky overlays (grid, compass, objects, RA/Dec readout, Astrometry summary) hidden until the user presses Solve for that frame (ADR-024).
  2. **On-Demand Solve (`S` key or "Solve" button):** Plate solving never runs automatically. On a frame with a still hidden header WCS, the first press shows that WCS instantly without computing; any further press (or a frame without header WCS) runs a real solve:
     - If approximate `RA`/`DEC` or a recognizable `OBJECT` name (via built-in `deep_sky.csv`) is present in headers, run fast local Gaia cone solving (`fulgur-pipeline::sky::solve`).
     - If no coordinates are known or local cone solving fails, automatically fall back to the local **Blind Solver** (`fulgur-solve` quad index, `0.5°..12°` FoV) followed by Gaia polynomial refinement.
- **Astrometric Outputs & Overlays:**
  - **Live Sky Readout:** Right Ascension (`HH:MM:SS.s`) and Declination (`±DD°MM′SS″`) under the mouse cursor, field center RA/Dec, Field of View (`W° × H°` or `arcmin`), Pixel Scale (`″/px`), Position Angle (`PA °`), and parity/mirroring indicator.
  - **Toggleable Visual Overlays:**
    - Celestial **RA/Dec coordinate grid** with labels.
    - **Compass Rose (N / E vectors)** showing sky orientation.
    - **Object Annotations:** Crosshairs/ellipses and labels for DSOs from the built-in `deep_sky.csv` catalog and bright Gaia stars inside the field.
  - **Save WCS to File:** Explicit button to atomically write the solved FITS WCS cards into the current FITS or XISF file.

---

## 4. Milestones & Acceptance Gates

Development proceeds strictly one milestone at a time. Each milestone requires automated release-mode test verification and owner review before advancing.

### Milestone 1 (M1): Core Foundation, Multi-Format Reader & Ultra-Fast Viewer UI
- **Scope:**
  - Initialize the standalone Cargo workspace (`fulgur-preview-core`, `fulgur-preview-gui`, `fulgur-preview-cli`).
  - Port and extend FITS reader (`BITPIX` `8`, `16`, `32`, `-32`, `-64`, 2D/3D, MEF HDU listing), XISF 1.0 reader, `cameras.json`, and header derivation (`derive.rs`).
  - Add standard image decoding (`TIFF` 8/16/32-bit, `JPEG`, `PNG`, `WebP`, `BMP`) and EXIF extraction.
  - Implement fast 2×2 Superpixel / MHC overview debayering + on-demand AVX2 VNG 1:1 detail viewport.
  - Build the dark `egui` viewer window: command-line file opening, drag-and-drop, directory scanning (natural filename and date sorting), `Left`/`Right` navigation, background neighbour prefetching, **Blink Mode** (locked zoom/pan/stretch),Linked/Unlinked MTF Auto-Stretch + manual sliders, Fullscreen (`F11`), Metadata side panel with search, and `_Rejected` culling (`Delete`/`X` + `Ctrl+Z` Undo).
- **Acceptance Gate G1:**
  - Single portable `fulgur-preview.exe` (`+crt-static`, zero external CRT DLLs).
  - Opens Seestar S50 / ASI2600MC FITS and XISF files in `< 50 ms` for overview; switches between prefetched frames in `< 10 ms`.
  - Mouse wheel zooms the image exclusively; Blink Mode preserves exact viewport across frames.

### Milestone 2 (M2): Two-Stage DSLR RAW Support & Interactive Signal Histogram
- **Scope:**
  - Integrate pure-Rust DSLR/mirrorless RAW decoding (`rawler` for `.cr2`, `.cr3`, `.nef`, `.arw`, `.dng`, `.orf`, `.rw2`, `.pef`, `.raf`).
  - Implement Stage-1 instant embedded JPEG preview and Stage-2 background linear CFA decoding + auto-stretch transition.
  - Implement the **Histogram Panel**: linear data computation, per-channel R/G/B + Luminance curves, **Zoom to Signal** auto-framing around the sky/nebulosity peak, explicit `Min`/`Max` range sliders inside the panel (never hijacking the mouse wheel), and Linear/Log Y-axis toggle.
- **Acceptance Gate G2:**
  - Verified opening and rapid browsing of DSLR RAW files alongside FITS/XISF/JPEG/TIFF.
  - Histogram accurately isolates the linear astro signal peak and responds smoothly to `Min`/`Max` sliders while mouse wheel remains dedicated to image zoom.

### Milestone 3 (M3): Real-Time Star Quality (FWHM/HFR), Cursor Inspector & 3×3 Tilt Map
- **Scope:**
  - Port and adapt hot-pixel-robust star detection and photometry (`stars.rs`) for asynchronous single-frame execution on load.
  - Display global frame metrics (median FWHM, HFR, eccentricity, star count, sky background, noise).
  - Implement the live **Cursor Inspector** (pixel X/Y, linear ADU values, and hovered star FWHM/HFR/eccentricity/profile).
  - Implement the **3×3 Sensor Tilt & Corner Map** widget visualizing FWHM/HFR/eccentricity across the 9 sensor zones.
- **Acceptance Gate G3:**
  - Star metrics match `FulgurStack` inspection values on DS-B (`SeestarS50 Subs`) frames.
  - Hovering over stars in the viewer immediately reports individual star FWHM/HFR/eccentricity; the 3×3 grid renders cleanly in the Analysis panel.

### Milestone 4 (M4): Local & Blind Gaia Plate Solving, Sky Overlays & Atomic WCS Save
- **Scope:**
  - Port `fulgur-catalog`, `fulgur-solve`, and `fulgur-pipeline::sky` modules into the workspace.
  - Auto-discover Gaia DR3 `.xpsd` paths, cache, and blind index from `%LOCALAPPDATA%\FulgurStack` (with local override in Settings).
  - Parse existing FITS/XISF WCS immediately on load; implement on-demand Local + Blind Solving (`S` key / Solve button).
  - Render live cursor RA/Dec, FoV/scale/PA summary, celestial **RA/Dec grid**, **N/E compass**, and **DSO/star annotations**.
  - Implement explicit **"Save WCS to File"** with atomic `.tmp` write-and-rename for FITS and XISF.
- **Acceptance Gate G4:**
  - Solved WCS matches `FulgurStack` accuracy (`RMS < 0.4 px` on DS-B / Seestar / ASI2600 frames) in `< 1–2 s`.
  - Celestial grid, N/E compass, and DSO annotations align accurately on the image; saved WCS opens and validates in PixInsight.

### Milestone 5 (M5): Windows Packaging, File Associations Helper, Polish & Cross-Platform Readiness
- **Scope:**
  - Portable ZIP distribution package (`FulgurPreView-<version>-<commit>-windows-x64.zip`) with static CRT verification, application icon, and Czech user guide (`CTI_ME.txt`).
  - Optional one-click Windows "Open With" registration helper and keyboard shortcut cheat-sheet overlay (`F1` / `?`).
  - Cross-platform verification checks for macOS (`aarch64-apple-darwin`) and Linux (`x86_64-unknown-linux-gnu`) compilation.
- **Acceptance Gate G5:**
  - Owner manual review of the packaged Windows release `.exe`.
