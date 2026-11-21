# Architecture Decisions (ADR Log)

This document records the foundational product and engineering decisions for **FulgurPreView**.

---

## ADR-001: Standalone Repository with Adapted FulgurStack Modules (2026-11-13)
- **Context:** `FulgurStack` lives in a sibling directory and already implements FITS/XISF I/O, `cameras.json`, header reconstruction, MHC/AVX2 VNG demosaicing, hot-pixel-robust star photometry, and Gaia DR3 local + blind plate solving.
- **Decision:** Copy and adapt the required modules directly into `FulgurPreView`'s own Cargo workspace (`crates/fulgur-core`, `crates/fulgur-engine`, `crates/fulgur-catalog`, `crates/fulgur-solve`, `crates/fulgur-preview`) rather than referencing `../FulgurStack` via relative Cargo paths.
- **Consequences:** Complete repository independence; `FulgurPreView` can extend FITS support (all `BITPIX` types, MEF) and optimize single-frame interactive pipelines without risking regressions in `FulgurStack`.

## ADR-002: Two-Stage DSLR RAW Loading Pipeline (2026-11-13)
- **Context:** Decoding and demosaicing 24–60 Mpx DSLR/mirrorless RAW files takes 100–300 ms, which would slow down rapid arrow-key directory browsing, yet astrophotographers also need linear CFA data for MTF auto-stretch, FWHM measurement, and plate solving.
- **Decision:** Use a pure-Rust RAW decoder (`rawler`) in a two-stage pipeline:
  1. **Stage 1:** Immediately extract and display the embedded JPEG preview (or fast half-size superpixel) in `< 20 ms` so browsing never stalls.
  2. **Stage 2:** Decode the linear CFA sensor data on a cancellable background thread, apply MTF auto-stretch, compute the linear histogram and star metrics, and upgrade the viewport seamlessly while keeping the current zoom/pan.

## ADR-003: Two-Tier Debayering & Bounded Neighbour Prefetching (2026-11-13)
- **Context:** Full-frame VNG demosaicing on large sensors is unnecessary when viewing an image fitted to the screen, while 1:1 pixel inspection requires artifact-free VNG reconstruction.
- **Decision:**
  - Use fast 2×2 Superpixel / MHC demosaicing to build the `2048 px` overview texture in milliseconds.
  - Stream on-demand **AVX2 VNG** demosaicing only for the visible 1:1 native detail ROI when zoomed in.
  - Automatically prefetch neighbouring files (`i + 1`, `i - 1`) in a bounded in-memory LRU cache (`<= 2 GiB`).

## ADR-004: Format-Aware Auto-Stretch & Blink Mode Default (2026-11-13)
- **Context:** Linear astronomical images (FITS, XISF, RAW) appear black without a non-linear transfer function, whereas standard rendered images (JPEG, PNG, 8-bit TIFF) are already tone-mapped.
- **Decision:**
  - Enable **MTF Auto-Stretch** (with Linked and Unlinked RGB modes + manual sliders) by default for FITS, XISF, DSLR RAW, and linear 16/32-bit astro TIFFs; keep Auto-Stretch OFF by default for JPEG, PNG, WebP, BMP, and standard 8-bit TIFFs.
  - Provide a **"Lock View (Blink)"** toggle in the top bar (ON by default for images of matching dimensions) that preserves zoom, pan center, and stretch across `Left`/`Right` frame transitions.

## ADR-005: Mouse Wheel Reserved for Image Zoom & Slider-Driven Signal Histogram (2026-11-13)
- **Context:** In an image viewer, hijacking the mouse scroll wheel inside side panels disrupts fast image zooming. Linear astro histograms also concentrate 99% of the signal in a narrow peak near the black point.
- **Decision:**
  - The mouse scroll wheel is **always reserved for zooming the image** around the cursor.
  - The Histogram panel provides an automatic **"Zoom to Signal"** mode focusing on the sky background and nebulosity peak, alongside explicit **`Min` and `Max` horizontal sliders** inside the histogram widget and a `Linear / Log` Y-axis toggle.

## ADR-006: Background FWHM/HFR, Cursor Star Inspector & 3×3 Tilt Map (2026-11-13)
- **Context:** Astrophotographers need instant feedback on focus, tracking, coma, and sensor tilt while browsing subframes.
- **Decision:**
  - Automatically run hot-pixel-robust star detection in the background on every loaded astro frame to display median FWHM, HFR, eccentricity, star count, sky background, and noise.
  - Show live `(X, Y)` and `(R, G, B)` ADU under the mouse cursor, snapping to hovered stars to report individual star FWHM/HFR/eccentricity.
  - Provide a **3×3 Sensor Tilt & Corner Map** displaying median FWHM, HFR, and eccentricity across 9 sensor zones.

## ADR-007: Shared Gaia Catalogues, On-Demand Solving & Explicit File Operations (2026-11-13)
- **Context:** Users of `FulgurStack` already have PixInsight Gaia DR3 `.xpsd` files, a Gaia cache, and the `1.12 GB` blind-solving quad index configured in `%LOCALAPPDATA%\FulgurStack`.
- **Decision:**
  - Automatically read `%LOCALAPPDATA%\FulgurStack` settings to share Gaia DR3 and blind-index paths out of the box.
  - Use existing header WCS immediately; trigger local Gaia + blind fallback solving on demand (`S` key / Solve button) with RA/Dec grid, N/E compass, and DSO/star annotations.
  - Keep the viewer strictly read-only except for two explicit user actions:
    1. **Save WCS to File:** Atomic `.tmp` write-and-rename on explicit button click for FITS/XISF.
    2. **Subframe Culling (`X` / `Delete`):** Move rejected frame to `<current_dir>/_Rejected/` with `Ctrl+Z` Undo support.

## ADR-008: Self-Contained Pure-Rust RAW Decoder & Live 256-Bin Signal Histogram (2026-11-14)
- **Context:** External RAW crates often pull heavy dependencies or require network crates not cached in the workspace. Additionally, static 256-bin histograms over `[0..1]` lose resolution when zoomed into narrow sky-background peaks (`< 1%` of full scale).
- **Decision:**
  - Implement a zero-external-C-dependency pure-Rust RAW module (`crates/fulgur-core/src/raw.rs`) supporting `.cr2`, `.cr3`, `.nef`, `.arw`, `.dng`, `.orf`, `.rw2`, `.pef`, and `.raf` with Stage-1 embedded JPEG extraction (`< 15 ms`) and Stage-2 CFA decoding (unpacked/packed 8–16-bit strips/tiles, ITU-T.81 Lossless JPEG `SOF3` predictors 1..7 + Canon CR2 unslicing, Deflate CFA, Sony ARW2 delta blocks, DNG linearization, and black/white level normalization).
  - Precompute both full-range (`[0..1]`) and signal-zoomed (`[med - 6σ .. med + 24σ]`) 256-bin histograms in `OverviewFrame`, and dynamically re-bin `OverviewFrame::pixels` in `< 0.2 ms` whenever the user drags the `Min` / `Max` histogram range sliders.

## ADR-009: FulgurStack-Parity Star Photometry, Radial Profile HUD & 3×3 Sensor Tilt Map (2026-11-14)
- **Context:** Milestone 3 requires frame-wide FWHM/HFR/eccentricity metrics that match `FulgurStack` inspection values on `SeestarS50 Subs`, plus a live Cursor Star Inspector with radial profile and a 3×3 Sensor Tilt & Corner Map without slowing down initial image display.
- **Decision:**
  - Port `FulgurStack`'s Green-CFA / Mono photometry (`Sky` 16×16 robust grid, `defects` hot-pixel suppression, `natural_profile` guard, Lanczos-3 `Green` half-pixel quadrature HFR and moment FWHM) into `crates/fulgur-engine/src/stars.rs`.
  - Compute global medians on the first `CATALOG_STARS = 250` stars (guaranteeing exact parity with `FulgurStack`) while measuring up to `MAX_INSPECT_STARS = 450` stars to populate the 9-zone `SensorTiltMap` and `Star::radial_profile`.
  - Run `analyze_frame_stars` asynchronously in `FrameCache`'s `StarBackgroundJob` after `OverviewFrame` is displayed, caching `Arc<StarQualityReport>` per frame so `Left`/`Right` Blink Mode navigation shows star metrics and tilt maps at zero latency.

## ADR-010: Two-Stage Local Gaia + Blind Quad Plate Solving, Sky Overlays & Atomic WCS Save (2026-11-14)
- **Context:** Milestone 4 requires zero-latency parsing of existing FITS/XISF WCS headers, sub-2-second local Gaia DR3 cone solving with automatic blind quad-index fallback, interactive celestial overlays (curved RA/Dec grid, N/E compass, DSO/Gaia star annotations), and an explicit atomic WCS header save for FITS and XISF files without ever mutating pixel data.
- **Decision:**
  - Port `fulgur-catalog`, `fulgur-solve`, and `fulgur-engine::sky` into the standalone workspace, automatically discovering Gaia DR3 `.xpsd`, tile cache, and `fulgurstack_gaia_g13.fsidx` paths from `%LOCALAPPDATA%\FulgurStack\settings.json`.
  - Parse `TAN` and `TAN-SIP` headers (`CRVAL1/2`, `CRPIX1/2`, `CDi_j` / `PCi_j` / `CDELTi` / `CROTA2`, `A_ORDER` / `B_ORDER` / `AP_ORDER` / `BP_ORDER`) into `OverviewFrame::header_wcs` on load so already-solved frames immediately display celestial coordinates, RA/Dec grids, N/E compass, and DSO annotations.
  - Cache the loaded `Arc<fulgur_solve::Index>` inside `FrameCache` across solves so subsequent blind solves do not re-read the `1.12 GB` index from disk.
  - Implement `fulgur_core::fits::save_wcs_to_file` and `fulgur_core::xisf::save_wcs_to_xisf` using `.tmp` write, `sync_all()`, and atomic `std::fs::rename`, streaming pixel attachments bit-for-bit (and automatically shifting XISF attachment offsets if XML header expansion crosses the first attachment boundary).

## ADR-011: Robustness Fixes from the M1–M4 Code Review (2026-11-16)
- **Context:** A critical review of M1–M4 found silent-failure paths: a failed RAW Stage-2 decode restarted every UI frame, compressed RAWs (CR3 / CRX, Nikon compressed NEF, …) silently fell back to a de-gamma'd JPEG labelled as linear data, abandoned loads piled up as detached threads holding full frames, float-FITS `0..1` normalization was decided per buffer (overview vs 1:1 ROI disagreed), WCS save silently fell back to another HDU, header-derived allocations were unbounded, culling moves could overwrite on Windows, and release-build panics left no trace.
- **Decision:**
  - Every background job ends in one recorded state (result, error, or cancelled by the cache). A worker that disappears without a result (panic) is recorded as an error. RAW Stage-2 failures are kept per key (`stage2_failed`) so they are never retried in a loop.
  - New `RawStage::Stage2PreviewOnly` for RAWs whose sensor data cannot be decoded; the UI labels it and star metrics are refused (`RAW_PREVIEW_ONLY_STARS_MESSAGE`).
  - At most `MAX_CONCURRENT_DECODES = 3` full-frame decodes hold buffers at once (`DecodeSlot`); waiting jobs exit on cancel without allocating. Loads started while a previous load is still running wait `NAVIGATION_DEBOUNCE = 60 ms` first. Bulk reads use 8 MiB cancellable chunks (`read_exact_cancellable`, `read_file_cancellable`).
  - Float FITS scaling (`adu_scale`) is decided once per HDU at header read from 16 evenly spaced rows and applied by every reader.
  - WCS save uses the reader's HDU enumeration, errors on an unknown index, writes to a uniquely named sibling temp file (keeping the extension), and verifies that the pixel tail is byte-identical and the file re-parses before the atomic rename.
  - Header-derived allocations are bounded by `MAX_DECODE_BYTES = 2 GiB` / `MAX_DECODE_PIXELS = 200 Mpx` and use `try_reserve`.
  - Culling uses no-replace moves (`MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on Windows, hard link + unlink elsewhere).
  - A panic hook appends reports to `%LOCALAPPDATA%\FulgurPreView\crash.log` and shows a message box only for UI-thread panics.
- **Consequences:** Cold opens while key-repeat navigation is active start ~60 ms later; in exchange, skipped frames are never read and peak memory is bounded. Persistent (cross-session) culling undo and decoders for CR3 / compressed NEF / Sony lossless ARW remain open work.

## ADR-012: Remaining Review Findings — Reuse, Isolation, Folder Watching (2026-11-17)
- **Context:** After ADR-011, the review still listed: stale PixInsight astrometry after XISF WCS save (06), every frame decoded three times (08), UI stretch rendering sharing rayon's global pool with background work (09), no reaction to frames added or rewritten during a session (12, 13), incomplete GPS redaction (14), a disk-reading `Default` impl (15), the blind index lost on a cancelled solve (16), culling undo lost on exit, and four copies of the background-job pattern (20).
- **Decision:**
  - XISF WCS save also removes the image's `PCL:AstrometricSolution:*` properties (PixInsight prefers them over FITS keywords) and shifts attachment offsets for both quote styles.
  - Overview loaders return the decoded single-plane source (`load_frame_overview_with_source`); the cache keeps it (768 MiB LRU) and hands it to star analysis and plate solving. RGB / standard images keep the path-based fallback.
  - Prefetch, star analysis and plate solving run on a dedicated rayon pool (`fulgur-bg-*`, cores − 1 threads).
  - A polling folder watcher (`watch.rs`, 1.5 s) runs off the UI thread. Files are added once size and modification time are unchanged across two scans; files rewritten on disk are dropped from the cache (clearing recorded failures) and reload. Our own WCS save updates the known signature so it does not trigger a reload. Optional **Follow New** jumps to arriving frames when the last frame is shown. Polling was chosen over OS notifications because it behaves the same on local disks, USB drives and network shares.
  - GPS redaction covers `SITEELEV`, `SITEALT`, `OBSGEO-X/Y/Z/H`, `OBSLAT/OBSLONG/OBSELEV`, `ALT-OBS` and XISF `Observation:Location:Elevation`.
  - `CatalogConfig` has no `Default`; `discover()` is explicit.
  - The blind index is cached process-wide as soon as it loads.
  - Culling undo is journaled to `%LOCALAPPDATA%\FulgurPreView\culling_undo.json` (last 500 steps; entries whose rejected file disappeared are dropped).
  - All background tasks use one `jobs::Job<K, T>` type with a uniform done / pending / stopped poll.
- **Consequences:** Up to 768 MiB extra RAM for decoded planes when browsing very large frames (bounded). The UI repaints every 500 ms while a folder is open to pick up new frames. Still open: GPU-shader stretch, splitting `app.rs` into per-panel modules, unifying `Result<_, String>` in `sky` with `fulgur_core::Error`, sharing code with FulgurStack (needs an owner decision), and RAW decoders for CR3 / compressed NEF / Sony lossless ARW (needs sample files).

## ADR-013: Separate Repositories with an Automated FulgurStack Parity Test (2026-11-17)
- **Context:** FulgurPreView and FulgurStack share about 6,000 lines of copied code (readers, catalogue, solver, star photometry) that had started to drift. Options were a third shared repository consumed by both, or keeping the copies with a guard against drift.
- **Decision (owner):** Keep the two repositories separate (ADR-001 stays). Add `crates/fulgur-engine/tests/fulgurstack_parity.rs`, which compares FulgurPreView's star count, median HFR, moment FWHM and eccentricity with reference values produced by FulgurStack's `inspect` command (`tests/fixtures/fulgurstack_parity_seestar.csv`, FulgurStack commit 552682c) on the 32 Seestar S50 subs. Tolerance is 1e-5 px; star counts must match exactly.
- **Consequences:** At creation the two implementations agree to within 5e-7 on all 32 frames. Any change to the copied photometry in either project shows up as a failing test; the fixture is regenerated (procedure in the test header) only after an intentional change. The test data is not committed: the comparison runs when `FULGUR_PARITY_DATA` or the sibling `../FulgurStack/SeestarS50 Subs` exists and is skipped with a note otherwise.

## ADR-014: Field Curvature, Tilt & Collimation Map (2026-11-17)
- **Context:** The owner asked for a CCD-Inspector-style analysis of field curvature and collimation. The 3×3 tilt map (ADR-009) only gives nine zone medians.
- **Decision:**
  - `fulgur-engine::field::FieldAnalysis` fits the per-star moment FWHM of all measured stars (up to 450) with a quadratic surface in frame coordinates normalized by the half-diagonal, with 3 rounds of 2.5σ (MAD) outlier clipping; at least 20 stars are required. It is computed with the star report (`StarQualityReport::field`), so it costs no extra file I/O.
  - Reported values: min / max fitted FWHM in the frame (″ when the pixel scale is known), curvature (mean corner FWHM vs sharpest point, %), tilt X (right − left edge) and Y (bottom − top edge) from the linear terms, total tilt (steepest edge-to-edge difference vs sharpest FWHM, %) with direction to the softer side (0° = right, 90° = up), collimation (distance of the paraboloid's minimum from the frame centre, px and % of half-diagonal; flagged when the surface has no interior minimum), and stars used.
  - UI: **Map [C]** toggle in the Star Quality panel (key `C`) draws a colour map of the surface (blue = sharp, red = soft) with 4 FWHM contours, a crosshair at the sharpest point, a ring at the frame centre, a tilt arrow and the summary values; the panel lists the values with a Copy button.
- **Consequences:** Numbers follow the usual meaning of the terms but are not identical to CCD Inspector's, which defines them slightly differently. FWHM per star comes from the existing photometry, so the FulgurStack parity test (ADR-013) is unaffected.

## ADR-015: Length Units Toggle & Eccentricity Map (2026-11-17)
- **Context:** The owner asked for (1) a switch showing FWHM, HFR and similar values either in pixels or in arcseconds for frames with a known scale, and (2) a field map for star eccentricity like the FWHM map (ADR-014).
- **Decision:**
  - **Units:** `px` / `arcsec ″` toggle in the Star Quality panel (default px). The scale is the plate solution's (`wcs.scale()`, also from header WCS) when present, otherwise the header-derived pixel scale; the arcsec option is disabled with an explanation when no scale is known. The choice applies to the panel FWHM/HFR, the 3×3 tilt cells and tooltips, corner tilt, the field summary, the FWHM map contour labels and summary box, the status-bar badges and the hovered-star panel. `fulgur-engine::field::format_length` is the single formatter.
  - **Eccentricity map:** `fulgur-engine::field::EccentricityMap` uses the same robust quadratic surface fit (shared `fit_surface`) on per-star eccentricity, and reports min / max, centre, mean corners vs centre and the most elongated corner. The overlay code (`field_map.rs`) is generic over a `MapView`; the map is chosen with **Map: Off / FWHM [C] / Ecc [E]** in the panel or the keys `C` / `E`. Contours are labelled `e 0.46` etc. The map's summary box sits in the top-right corner so it never overlaps the hovered-star panel.
- **Consequences:** Eccentricity is unitless and unaffected by the toggle. Elongation direction (whisker plot) is not available yet: per-star orientation is not measured by the photometry.

## ADR-016: Elongation Direction, Unified Solver Errors, UI Module Split, CPU Stretch Kept (2026-11-17)
- **Elongation direction:** `Star::elongation_angle_deg` is the long-axis direction from the second moments (degrees `[0, 180)`, counter-clockwise on screen; verified by a test with stars rendered at known angles). `EccentricityMap` adds a 12-cell-wide whisker grid (doubled-angle, eccentricity-weighted mean per cell), the frame's mean direction and a direction coherence (1 = all stars parallel, typical of tracking / guiding / wind; low = no common direction, e.g. radial optical patterns). The eccentricity map (key `E`) draws the whiskers. On the Seestar S50 subs the elongation points consistently at ~150–165° (coherence up to 0.8), as expected from alt-az tracking.
- **Errors:** `fulgur_core::Error::Message(String)` carries run-time messages. The public plate-solving API (`load_raw_for_solving`, `solve_raw_frame`) now returns `fulgur_core::Result`; the copied catalogue / solver crates keep their `String` errors internally so they stay in step with FulgurStack (ADR-013), and the engine maps them at the boundary (`"Cancelled"` → `Error::Cancelled`).
- **UI split:** `app.rs` (3,200 lines) became `app/mod.rs` (state, actions, shortcuts, `update`) plus `toolbar`, `histogram_panel`, `star_panel`, `astrometry_panel`, `metadata_panel`, `status_bar` and `chrome` modules, each an `impl FulgurPreViewApp` block. No behaviour change.
- **GPU stretch not implemented:** measured on this PC (24 threads), rendering the stretched 1080×1920 overview on the CPU takes 5.9 ms median / 6.6 ms p95, unchanged while star analysis runs on the background pool (ADR-012). That is well inside a 16 ms frame, so a GL shader path (float textures, OpenGL 2.0 compatibility risk) would add complexity without a visible benefit. Revisit if overviews grow beyond 2048 px or slider lag is reported.

## ADR-017: Median-Centred Manual Stretch Sliders (2026-11-17)
- **Context:** The manual Black / White sliders were linear over `[0, 1]`. On linear astro frames the sky background sits within a few percent of the range, so almost all slider travel was spent on useless bright values (owner feedback).
- **Decision:** `slider_scale::MedianSliderScale` maps slider position to value with an `asinh` curve centred on the frame's luminance median, with the background noise (MAD sigma, floor 2e-4) as the linear zone: finest control at the median, logarithmic towards 0 and 1, both ends still reachable. Black and White use it; Midtone uses the same curve centred on the frame's automatic (Linked) midtone. The slider shows and accepts the real value (typed values are converted back to a position).
- **Consequences:** For a typical Seestar frame (median 0.04, sigma 0.003) about a third of the travel lies below the median and ±1 sigma around it covers ~18 % of the travel instead of 0.6 %. The scale follows each frame's statistics, so the same value can sit at a different slider position on another frame.
- **Addendum (unlinked channels):** The manual sliders no longer collapse R, G and B onto one value. They show the channel mean (black / white) and geometric mean (midtone); with **Link RGB** off (default when starting from Unlinked auto stretch), a change shifts black / white of every channel by the same amount and scales every midtone by the same factor, so an unlinked colour balance is preserved. **Link RGB** on sets all channels to the shown values. **Copy Auto Linked** / **Copy Auto Unlinked** start from either automatic stretch (`slider_scale::ChannelPoints`).

## ADR-018: Per-Channel Signal Levels, Wheel Browse Mode, Status Bar Order (2026-11-17)
- **Signal levels:** `fulgur-engine::levels` measures, at full resolution, the median and noise per channel (R/G/B for Bayer CFA, `Mono`, or R/G/B/L for RGB images), on up to 500k samples per channel (~5 ms for a Seestar frame). Noise is `1.4826 · MAD(x[i] − x[i+step]) / √2` over neighbouring same-colour pixels, so gradients, nebulosity and stars do not inflate it. The values are part of the star report (`StarQualityReport::levels`) and shown as a table (Median / Noise / SNR) under the histogram; until the star analysis finishes, the overview statistics are shown and labelled as such. The median matches FulgurStack's background (2308 ADU on the first Seestar sub). The noise is per pixel; FulgurStack's sky noise (63 ADU) is measured on its averaged green plane and is therefore ~1/√2 of the green per-pixel value (89 ADU).
- **Mouse wheel:** owner decision to relax the wheel rule (AGENTS.md updated): **Wheel: Zoom / Browse** toggle in the toolbar and key `W`. In Browse mode one wheel notch (40 pt of raw scroll, touchpad deltas accumulate) moves one frame, scrolling down = next; Ctrl+wheel and pinch still zoom. Default stays Zoom.
- **Status bar:** reordered to quality block (stars, FWHM, HFR, eccentricity, background ± noise) → live cursor block (X/Y, ADU, RA/Dec, hovered star; a placeholder keeps its place) → static file block (name, format, size, BITPIX, Bayer, RAW stage). Load timings and cache usage moved to the file block's tooltip.

## ADR-019: Asteroid & Comet Identification (Online, On Demand) (2026-11-17)
- **Context:** The owner wants to identify asteroids and comets around the cursor on plate-solved frames (right-click), for the observation time in the header. MPC's MPChecker has no documented machine interface (automated fetches are refused); NASA/JPL's Small-Body Identification API and IMCCE SkyBoT offer documented JSON services built on the MPC orbit catalogue.
- **Decision (owner parameters: 25′ around the cursor, V ≤ 20, header location else Prague):**
  - `fulgur-engine::sky::small_bodies` queries `https://ssd-api.jpl.nasa.gov/sb_ident.api` with `two-pass=true` (numerically integrated positions). Measured: ~23 s per query; the one-pass mode is not faster (18 s) and its errors are tens of arcseconds, so two-pass is used. The API searches a box; results outside the 25′ circle are dropped and the rest sorted by distance.
  - Time = `DATE-OBS` (UTC) + `EXPTIME`/2. Missing `DATE-OBS` → clear message, no query.
  - Location = `SITELAT` / `SITELONG` (`SITEELEV`) rounded to 0.1° (~10 km) before it leaves the computer (privacy; negligible parallax effect except for very close NEOs); otherwise Prague (50.0755° N, 14.4378° E, 0.2 km).
  - HTTPS via `ureq` 2 with `rustls` (TLS in Rust; `ring` compiles its bundled C/assembly statically, no external runtime library), so the app stays one portable `.exe`. This is the app's first network access and happens only on an explicit menu click.
  - UI: right-click on the image → "Identify asteroids & comets here (25′, V ≤ 20)" (disabled with a hint on unsolved frames). A spinner runs at the clicked position until the query finishes; then the dashed 25′ circle, each object (circle, name, V, one-hour motion arrow projected through the WCS; comets green) and a result table in the Astrometry panel (object, V, distance, motion, time and location used, Clear). One search is kept per frame.
  - Tests run offline on a recorded JPL response (`tests/fixtures/jpl_sb_ident_m33.json`); a live test is `#[ignore]` (`cargo test -- --ignored live_jpl`).
- **Consequences:** Requires internet for this feature only; failures (offline, HTTP errors) are shown at the clicked position and in the panel. Stacked masters have an ambiguous time and smeared moving objects, so the feature targets single subs.

## ADR-020: Auto-Stretch for Sparse Maps (2026-11-17)
- **Context:** FulgurStack's high-rejection map (87 % of pixels exactly 0) looked inverted when zoomed out: median and MAD are 0, so the MTF midtone collapsed to ~5e-6 and every non-zero value became white. The zoomed-out overview averages ~3×3 pixels, so 68 % of it was non-zero (white), while the 1:1 view (only 12.6 % non-zero) looked right.
- **Decision:** `compute_auto_stretch` detects sparse channels (median equals the minimum and MAD sigma is 0) and stretches the values above the base level instead: black = base, white = 99.5th percentile of the higher values, midtone so the typical higher value maps to mid-grey; the linked variant averages the per-channel points. Ordinary images are unchanged.
- **Consequences:** On `master_v128_high_rejection.xisf` the zoomed-out view now shows rejection density as grey levels (white 1.2 %, black 31.8 %, mean 72/255) with zeros black, consistent with the 1:1 view. `master_v128.xisf` and the coverage map get the same stretch as before.


## ADR-021: Float & Mono High-Depth TIFF Decoding (2026-11-17)
- **Context:** PixInsight saves stacked masters as single-channel 32-bit IEEE float TIFFs (uncompressed, one row per strip). The `image` 0.25 TIFF path supports float only as RGB/RGBA and rejected these with "does not support the color type `Unknown(32)`"; the app hid that reason behind "Could not decode standard image file." The reader also forced auto-stretch off for every TIFF, contrary to ADR-004.
- **Decision:**
  - When `image` cannot decode a TIFF, read it directly with the pure-Rust `tiff` 0.10 crate (the same crate `image` uses, no new C dependency): Gray/GrayA/RGB/RGBA in u8/u16/u32/u64, i8–i64, f16/f32/f64, mapped onto the 0..65535 ADU scale (floats in 0..1 scaled ×65535, NaN → 0; mono is replicated to RGB like 16-bit mono TIFFs).
  - Decode errors now carry the underlying decoder message.
  - Default stretch: floating-point TIFFs always open auto-stretched (processing masters are linear); integer 16/32-bit TIFFs auto-stretch when the green median is below 25 % of full scale; 8-bit TIFF/JPEG/PNG/WebP/BMP stay unstretched.
- **Consequences:** The 6248×4176 NGC 1333 L master (104 MB) opens in ~0.35 s with auto-stretch; peak RAM ~400 MB because mono data is expanded to three f32 planes like other standard images.

## ADR-022: No Star Metrics on 8-bit Processed Exports (2026-11-17)
- **Context:** On a 2000 px, 8-bit stretched export of the Taurus Molecular Cloud mosaic the panel showed FWHM 46.98″. The px → ″ conversion was right (plate-solve scale 8.62″/px), but the px FWHM was not: after a 3× downscale typical stars are ~1.8 px and fail the 18-sample support rule, and bright stars are clipped at 255, so only 42 of 1820 bloated mid-brightness stars were measured (5.45 px median). The full-size version of the same image measures 5.56 px at ~2.80″/px ≈ 15.6″.
- **Decision (owner's choice):** For 8-bit standard images (JPEG, PNG, WebP, BMP, 8-bit TIFF) star photometry is skipped; the report carries only the per-channel levels plus a reason, and the Star Quality panel and status-bar star badge say that star metrics need linear data (FITS, XISF, camera RAW or 16/32-bit TIFF). The measurement algorithm is unchanged for all other inputs (parity with FulgurStack).
- **Consequences:** No misleading FWHM/HFR/tilt on exports, and less CPU per 8-bit frame. 16/32-bit processed TIFFs are still measured.

## ADR-023: RAM-Aware Frame Budget for Drizzled Masters (2026-11-20)
- **Context:** FulgurStack's 2× drizzle masters (e.g. `master_v133.xisf`, 12496×8352×3 Float32 = 1.17 GiB) were refused by a fixed 1 GiB per-frame cap ("Image exceeds single-frame memory bound."). With the cap raised they open in ~1 s at ~1.6 GB peak RAM (1.74 GB with star analysis; ~3.8 GB while two equally large neighbours are prefetched).
- **Decision (owner's choice, option C):**
  - The fixed cap is replaced by a RAM-aware budget: a frame's decoded working size (≥ 4 bytes per sample) may use at most half of the RAM available at that moment, never more than 8 GiB (`MAX_FRAME_BYTES`); 4 GiB when free RAM cannot be read. The XISF index geometry sanity check uses the same 8 GiB ceiling.
  - A frame that does not fit gets a plain message: "Image needs X GB of memory, only Y GB is available (one image may use half of the Z GB of free RAM)…".
  - Neighbour prefetching stays on for large files. A failed prefetch only stops further prefetching of that file; it is no longer recorded as the file's final error, so opening it later runs a fresh load (a transient memory shortage never sticks).
  - Optics-derived scale (`XPIXSZ` / `FOCALLEN`, used only without `PIXSCALE` / CD / CDELT) is divided by FulgurStack's `DRIZZLE` factor. Binning is not applied because `XPIXSZ` already includes it (MaxIm DL / N.I.N.A. convention).
- **Consequences:** 2× and 3× drizzle masters from a 26 Mpx sensor open on machines with enough free RAM; on low-memory machines the user sees how much is needed instead of a technical error.

## ADR-024: Header WCS Hidden Until Solve (2026-11-20)
- **Context:** Plate solving already ran only on the Solve button or `S`, but files that carry a WCS in their header (FulgurStack masters, PixInsight-solved files) showed the RA/Dec grid and object labels immediately on open, which read as an automatic solve. The owner wants astrometry off by default.
- **Decision (owner's choice, option B; supersedes PROJECT_SPEC 3.3 "activate immediately"):**
  - Header WCS is still parsed on open (zero cost) and its plate scale is used for star metrics, but the grid, compass, object annotations, cursor RA/Dec and the Astrometry summary stay hidden until the user presses Solve for that frame.
  - Solve on a frame with a hidden header WCS shows it instantly (status: "Showing the WCS stored in the file header"); a further Solve runs a real local Gaia / blind solve; Solve while solving cancels. Frames without header WCS solve as before.
  - The choice is per frame (cache key) and survives navigation and cache eviction; Blink mode does not change it. Opening, navigating, prefetching and star analysis never start a solve.
  - Solve tooltips describe the next action (show header WCS / re-solve / solve / cancel); "Save WCS to Header" is disabled for a WCS that already comes from the header.
- **Consequences:** No sky overlays appear unless asked for; already solved files still give correct ″/px in the star panel.
