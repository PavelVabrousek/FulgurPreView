//! Astrometry, WCS gnomonic + SIP projection, local Gaia DR3 cone solving,
//! blind quad-index fallback, and sky annotation queries.

pub mod catalogue;
pub mod detect;
pub mod fits_wcs;
pub mod small_bodies;
pub mod solve;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use fulgur_catalog::config::CatalogConfig;
use fulgur_core::{BayerPattern, LayoutChoice, Mosaic, RawFrame};
use serde::Serialize;

/// The stars of one frame as the solver needs them: zero-based pixel centres
/// in the frame's reader order, brightest first.
#[derive(Clone, Copy, Debug)]
pub struct SkyFrame<'a> {
    pub name: &'a str,
    pub width: usize,
    pub height: usize,
    /// Decimal year of the exposure, for proper motion.
    pub epoch: f64,
    pub stars: &'a [[f64; 2]],
}

/// Filter detected stars to clean point sources with typical width and roundness.
pub fn point_sources<const N: usize>(stars: &[[f64; N]]) -> Vec<[f64; 2]> {
    let middle: Vec<&[f64; N]> = stars.iter().skip(50).take(1000).collect();
    if middle.len() < 20 {
        return stars.iter().map(|s| [s[0], s[1]]).collect();
    }
    let median = |mut v: Vec<f64>| {
        let m = v.len() / 2;
        v.select_nth_unstable_by(m, f64::total_cmp);
        v[m]
    };
    let width = median(middle.iter().map(|s| s[2]).collect());
    let typical = median(middle.iter().map(|s| s[3]).collect());
    stars
        .iter()
        .filter(|s| (0.6 * width..=1.6 * width).contains(&s[2]) && s[3] <= typical + 0.2)
        .map(|s| [s[0], s[1]])
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SolveMethod {
    HeaderWcs,
    LocalGaiaCone,
    BlindQuadIndex,
}

impl SolveMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::HeaderWcs => "Header WCS",
            Self::LocalGaiaCone => "Local Gaia DR3 Cone",
            Self::BlindQuadIndex => "Blind Quad Index + Gaia DR3",
        }
    }
}

/// Bright Gaia star projected onto the current frame for viewport overlay annotations.
#[derive(Clone, Debug, Serialize)]
pub struct FieldStar {
    pub x: f32,
    pub y: f32,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub g_mag: f32,
}

/// Deep-sky object from `deep_sky.csv` projected onto the current frame.
#[derive(Clone, Debug, Serialize)]
pub struct FieldDso {
    pub x: f32,
    pub y: f32,
    pub name: String,
    pub common_name: Option<String>,
    pub kind: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub size_arcmin: Option<f32>,
    pub mag: Option<f32>,
}

impl FieldDso {
    pub fn display_label(&self) -> String {
        match &self.common_name {
            Some(alias) if !alias.is_empty() && alias != &self.name => {
                format!("{} ({})", self.name, alias)
            }
            _ => self.name.clone(),
        }
    }
}

/// Complete astrometric state for an opened or solved frame.
#[derive(Clone, Debug, Serialize)]
pub struct PlateSolveReport {
    pub wcs: solve::Wcs,
    pub method: SolveMethod,
    pub target_hint: Option<String>,
    pub matched_stars: usize,
    pub fit_rms_px: f64,
    pub held_out_rms_px: Option<f64>,
    pub poly_degree: usize,
    pub elapsed_ms: f64,
    pub field_stars: Vec<FieldStar>,
    pub field_dsos: Vec<FieldDso>,
}

impl PlateSolveReport {
    /// Convert the active `Wcs` into FITS/XISF header cards (`(keyword, value, comment)`).
    pub fn to_fits_cards(&self) -> Option<Vec<(String, String, String)>> {
        let raw_cards = fits_wcs::cards(&self.wcs, [0.0, 0.0])?;
        Some(
            raw_cards
                .into_iter()
                .map(|(k, v, c)| (k, v, c.to_owned()))
                .collect(),
        )
    }
}

/// Build a `PlateSolveReport` from an existing FITS/XISF header WCS at zero solving cost.
pub fn build_header_wcs_report(header: &fulgur_core::fits::FitsHeader) -> Option<PlateSolveReport> {
    let t0 = Instant::now();
    let wcs = fits_wcs::parse_header_wcs(header)?;
    let field_dsos = collect_field_dsos(&wcs);
    let degree = wcs.forward.degree;
    Some(PlateSolveReport {
        wcs,
        method: SolveMethod::HeaderWcs,
        target_hint: header.info.object.clone(),
        matched_stars: 0,
        fit_rms_px: 0.0,
        held_out_rms_px: None,
        poly_degree: degree,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        field_stars: Vec::new(),
        field_dsos,
    })
}

/// Populate `field_dsos` inside the `wcs` footprint from the built-in `deep_sky.csv` catalog.
pub fn collect_field_dsos(wcs: &solve::Wcs) -> Vec<FieldDso> {
    let (c_ra, c_dec) = wcs.centre_sky();
    let (fov_w, fov_h) = wcs.fov_deg();
    let radius_deg = (fov_w.hypot(fov_h) * 0.65).clamp(0.15, 20.0);
    let mut out = Vec::new();
    for obj in fulgur_catalog::dso::in_cone(c_ra, c_dec, radius_deg) {
        if let Some(px) = wcs.sky_to_pixel(obj.ra, obj.dec)
            && wcs.inside(px, 24.0)
        {
            out.push(FieldDso {
                x: px[0] as f32,
                y: px[1] as f32,
                name: obj.name,
                common_name: obj.alias,
                kind: "DSO".to_owned(),
                ra_deg: obj.ra,
                dec_deg: obj.dec,
                size_arcmin: None,
                mag: None,
            });
        }
    }
    out.truncate(80);
    out
}

/// Project the brightest catalogue stars inside `wcs` for viewport annotation.
pub fn collect_field_stars(
    wcs: &solve::Wcs,
    cat_stars: &[catalogue::GaiaStar],
    epoch: f64,
    max_stars: usize,
) -> Vec<FieldStar> {
    let mut out = Vec::with_capacity(max_stars.min(cat_stars.len()));
    for s in cat_stars {
        let (ra, dec) = s.at(epoch);
        if let Some(px) = wcs.sky_to_pixel(ra, dec)
            && wcs.inside(px, 0.0)
        {
            out.push(FieldStar {
                x: px[0] as f32,
                y: px[1] as f32,
                ra_deg: ra,
                dec_deg: dec,
                g_mag: s.g as f32,
            });
            if out.len() >= max_stars {
                break;
            }
        }
    }
    out
}

/// Query local Gaia DR3 stars around `(ra, dec)` from `.xpsd` or the local tile cache.
pub fn query_local_gaia_cone(
    cfg: &CatalogConfig,
    ra_deg: f64,
    dec_deg: f64,
    radius_deg: f64,
    g_max: f64,
) -> Result<Vec<catalogue::GaiaStar>, String> {
    if let Some(xpsd_dir) = cfg.xpsd_dir() {
        let cat = fulgur_catalog::xpsd::XpsdCatalogue::open(&xpsd_dir)?;
        let stars = cat.cone(ra_deg, dec_deg, radius_deg, g_max)?;
        if !stars.is_empty() {
            return Ok(stars
                .iter()
                .map(catalogue::GaiaStar::from_catalog)
                .collect());
        }
    }
    if let Some(cache_dir) = cfg.cache_dir() {
        let stars = fulgur_catalog::download::cone(&cache_dir, ra_deg, dec_deg, radius_deg, g_max)?;
        if !stars.is_empty() {
            return Ok(stars
                .iter()
                .map(catalogue::GaiaStar::from_catalog)
                .collect());
        }
    }
    Err("No local Gaia DR3 .xpsd or cached tiles found for this sky region.".to_owned())
}

/// Extract coordinate hints `(ra_deg, dec_deg, description)` from header cards,
/// `OBJECT` name, or file/directory names.
pub fn discover_hints(
    path: &Path,
    header: &fulgur_core::fits::FitsHeader,
) -> Vec<(f64, f64, String)> {
    let mut hints: Vec<(f64, f64, String)> = Vec::new();
    let mut push_unique = |ra: f64, dec: f64, label: String| {
        if hints
            .iter()
            .all(|(r, d, _)| catalogue::separation(*r, *d, ra, dec) > 0.05)
        {
            hints.push((ra, dec, label));
        }
    };

    if let Some((ra, dec)) = header.approx_ra_dec_deg() {
        push_unique(ra, dec, format!("Header ({ra:.3}°, {dec:+.3}°)"));
    }

    if let Some(obj_name) = &header.info.object
        && let Some(dso) = fulgur_catalog::dso::find(obj_name)
    {
        push_unique(dso.ra, dso.dec, format!("OBJECT '{}'", dso.display_name()));
    }

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let header_objs: Vec<String> = header.info.object.iter().cloned().collect();
    let file_names: Vec<String> = (!file_name.is_empty())
        .then_some(file_name)
        .into_iter()
        .collect();
    let folder_names: Vec<String> = (!parent_name.is_empty())
        .then_some(parent_name)
        .into_iter()
        .collect();
    if let Some(guess) =
        fulgur_catalog::guess::guess_target(&header_objs, &file_names, &folder_names)
        && let Some(dso) = fulgur_catalog::dso::find(&guess.catalogue)
    {
        push_unique(
            dso.ra,
            dso.dec,
            format!("{} ({})", guess.matched, guess.from),
        );
    }

    hints
}

/// Map the solver's internal string errors to the workspace error type: the catalogue and
/// solver crates are kept in step with FulgurStack and report `String` errors.
fn solver_error(message: String) -> fulgur_core::Error {
    if message == "Cancelled" {
        fulgur_core::Error::Cancelled
    } else {
        fulgur_core::Error::Message(message)
    }
}

/// Load a frame from `path` as a single-plane `RawFrame` suitable for star detection and plate solving.
pub fn load_raw_for_solving(
    path: &Path,
    hdu_index: usize,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> fulgur_core::Result<RawFrame> {
    load_raw_for_solving_inner(path, hdu_index, layout, cancel).map_err(solver_error)
}

fn load_raw_for_solving_inner(
    path: &Path,
    hdu_index: usize,
    layout: LayoutChoice,
    cancel: &AtomicBool,
) -> Result<RawFrame, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_owned());
    }
    if fulgur_core::fits::is_astro_input(path) {
        let mut header =
            fulgur_core::fits::read_header_hdu(path, hdu_index).map_err(|e| e.to_string())?;
        header.info.apply_layout(layout);
        if header.info.planes >= 3 {
            let w = header.info.width;
            let h = header.info.height;
            let rgb = fulgur_core::fits::read_rgb_roi(path, &header, [0, 0, w, h], cancel)
                .map_err(|e| e.to_string())?;
            let pixels: Vec<f32> = rgb
                .pixels
                .iter()
                .map(|p| 0.25 * p[0] + 0.50 * p[1] + 0.25 * p[2])
                .collect();
            header.info.mosaic = Mosaic::Mono;
            header.info.bayer_pattern = None;
            return Ok(RawFrame { header, pixels });
        }
        let mut raw = fulgur_core::fits::read_raw_from_header_cancellable(path, header, cancel)
            .map_err(|e| e.to_string())?;
        raw.header.info.apply_layout(layout);
        return Ok(raw);
    }

    if fulgur_core::raw::is_raw_image(path) {
        let stage2 = fulgur_core::raw::read_raw_stage2_linear(path, layout, cancel)
            .map_err(|e| e.to_string())?;
        return match stage2 {
            fulgur_core::raw::RawStage2Output::Cfa(raw) => Ok(raw),
            fulgur_core::raw::RawStage2Output::LinearRgb(std_frame) => {
                let pixels: Vec<f32> = std_frame
                    .rgb
                    .pixels
                    .iter()
                    .map(|p| 0.25 * p[0] + 0.50 * p[1] + 0.25 * p[2])
                    .collect();
                let header =
                    fulgur_core::fits::FitsHeader::from_synthetic(std_frame.info, std_frame.cards);
                Ok(RawFrame { header, pixels })
            }
        };
    }

    if fulgur_core::standard::is_standard_image(path) {
        let std_img =
            fulgur_core::standard::read_standard(path, cancel).map_err(|e| e.to_string())?;
        let (w, h) = (std_img.rgb.width, std_img.rgb.height);
        let pixels: Vec<f32> = std_img
            .rgb
            .pixels
            .iter()
            .map(|p| 0.25 * p[0] + 0.50 * p[1] + 0.25 * p[2])
            .collect();
        let header = fulgur_core::fits::FitsHeader::from_synthetic(std_img.info, std_img.cards);
        let mut raw = RawFrame { header, pixels };
        raw.header.info.width = w;
        raw.header.info.height = h;
        raw.header.info.mosaic = Mosaic::Mono;
        raw.header.info.bayer_pattern = Option::<BayerPattern>::None;
        return Ok(raw);
    }

    Err("Unsupported file format for plate solving.".to_owned())
}

/// Process-wide cache of the loaded blind-solve quad index (~1.1 GB on disk).
///
/// The index is stored here as soon as it is loaded, independent of the solve result, so a
/// solve that is cancelled or fails after loading does not throw the loaded index away.
static BLIND_INDEX: std::sync::Mutex<Option<(std::path::PathBuf, Arc<fulgur_solve::Index>)>> =
    std::sync::Mutex::new(None);

/// Return the blind index at `path`, loading it at most once per process (per path).
fn shared_blind_index(path: &Path) -> Result<Arc<fulgur_solve::Index>, String> {
    let mut slot = BLIND_INDEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_path, index)) = slot.as_ref()
        && cached_path == path
    {
        return Ok(index.clone());
    }
    let loaded = fulgur_solve::Index::load(path)
        .map_err(|e| format!("Failed to load blind index {}: {e}", path.display()))?;
    let index = Arc::new(loaded);
    *slot = Some((path.to_path_buf(), index.clone()));
    Ok(index)
}

/// Execute on-demand plate solving on `raw`:
/// 1. Try fast local Gaia DR3 cone solving if coordinate/target hints are present.
/// 2. Fall back automatically to the local blind quad index (`fulgur-solve`),
///    then refine with a polynomial distortion model against local Gaia DR3 stars.
pub fn solve_raw_frame(
    path: &Path,
    raw: &RawFrame,
    cfg: &CatalogConfig,
    cached_index: Option<Arc<fulgur_solve::Index>>,
    cancel: &AtomicBool,
) -> fulgur_core::Result<(PlateSolveReport, Option<Arc<fulgur_solve::Index>>)> {
    solve_raw_frame_inner(path, raw, cfg, cached_index, cancel).map_err(solver_error)
}

fn solve_raw_frame_inner(
    path: &Path,
    raw: &RawFrame,
    cfg: &CatalogConfig,
    cached_index: Option<Arc<fulgur_solve::Index>>,
    cancel: &AtomicBool,
) -> Result<(PlateSolveReport, Option<Arc<fulgur_solve::Index>>), String> {
    let t0 = Instant::now();
    let width = raw.header.info.width;
    let height = raw.header.info.height;
    let epoch = raw
        .header
        .info
        .date_obs
        .as_deref()
        .and_then(catalogue::decimal_year)
        .unwrap_or(catalogue::GAIA_EPOCH);

    let is_bayer = raw.header.info.mosaic == Mosaic::Bayer;
    let (detected_full, _) = detect::bright_stars(&raw.pixels, width, height, is_bayer);
    if detected_full.len() < 6 {
        return Err(format!(
            "Only {} stars detected on frame (at least 6 required for plate solving).",
            detected_full.len()
        ));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_owned());
    }

    let pts: Vec<[f64; 2]> = point_sources(&detected_full);
    let sky_frame = SkyFrame {
        name: &path.to_string_lossy(),
        width,
        height,
        epoch,
        stars: &pts,
    };

    let hints = discover_hints(path, &raw.header);
    let scale_hint = raw.header.pixel_scale_arcsec();

    // ------------------------------------------------------------------
    // Stage 1: Fast Local Gaia DR3 Cone Solving around Hint Coordinates
    // ------------------------------------------------------------------
    for (ra_hint, dec_hint, hint_label) in &hints {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        let diag_px = ((width * width + height * height) as f64).sqrt();
        let fov_est_deg = scale_hint
            .map(|s| s * diag_px / 3600.0)
            .unwrap_or(2.0)
            .clamp(0.6, 8.0);
        let cone_radius = (fov_est_deg * 1.6).clamp(1.2, 6.0);
        let g_max = if fov_est_deg < 1.8 { 17.5 } else { 15.5 };

        if let Ok(gaia_stars) = query_local_gaia_cone(cfg, *ra_hint, *dec_hint, cone_radius, g_max)
            && gaia_stars.len() >= 15
        {
            let arc_stars = Arc::new(gaia_stars);
            let cache = solve::BlindCache::new(arc_stars.clone());
            if let Ok(sol) = solve::solve(&sky_frame, &[(*ra_hint, *dec_hint)], &cache, 1.5, None) {
                let field_stars = collect_field_stars(&sol.wcs, &arc_stars, epoch, 120);
                let field_dsos = collect_field_dsos(&sol.wcs);
                return Ok((
                    PlateSolveReport {
                        wcs: sol.wcs,
                        method: SolveMethod::LocalGaiaCone,
                        target_hint: Some(hint_label.clone()),
                        matched_stars: sol.matched,
                        fit_rms_px: sol.fit_rms_px,
                        held_out_rms_px: Some(sol.held_out_rms_px),
                        poly_degree: sol.degree,
                        elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
                        field_stars,
                        field_dsos,
                    },
                    cached_index,
                ));
            }
        }
    }

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_owned());
    }

    // ------------------------------------------------------------------
    // Stage 2: Blind Solver Fallback (Quad Index) + Gaia Refinement
    // ------------------------------------------------------------------
    let index_arc = match cached_index {
        Some(idx) => idx,
        None => {
            let Some(idx_path) = cfg.index_file_path() else {
                return Err(
                    "Local cone solve did not match and blind index (fulgurstack_gaia_g13.fsidx) was not found. Check catalogue paths in Astrometry settings."
                        .to_owned(),
                );
            };
            shared_blind_index(&idx_path)?
        }
    };

    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".to_owned());
    }

    // The quad search wants the frame's star positions, brightest first.
    // A header pixel scale narrows the field-size range it searches; if that
    // hint turns out wrong, retry over the solver's full default range.
    let default_opts = fulgur_solve::SolveOptions::default();
    let hinted_opts = scale_hint.map(|s| {
        let long_side_deg = s * width.max(height) as f64 / 3600.0;
        let (lo, hi) = fulgur_solve::FIELD_LIMITS_DEG;
        fulgur_solve::SolveOptions {
            field_deg: ((long_side_deg * 0.5).max(lo), (long_side_deg * 2.0).min(hi)),
            ..default_opts.clone()
        }
    });

    let blind_sol = hinted_opts
        .as_ref()
        .and_then(|opts| fulgur_solve::solve(&index_arc, width, height, &pts, opts, cancel))
        .or_else(|| fulgur_solve::solve(&index_arc, width, height, &pts, &default_opts, cancel))
        .ok_or_else(|| {
            if cancel.load(Ordering::Relaxed) {
                "Cancelled".to_owned()
            } else {
                "Blind solver could not find a verified sky match.".to_owned()
            }
        })?;

    // Refine blind solution with polynomial TAN-SIP fit against local Gaia DR3 stars if available
    let diag_px = ((width * width + height * height) as f64).sqrt();
    let cone_radius = (blind_sol.arcsec_per_px * diag_px / 3600.0 * 0.75).clamp(0.8, 8.0);
    let g_max = if cone_radius < 1.5 { 17.5 } else { 15.0 };

    let cone_stars = query_local_gaia_cone(cfg, blind_sol.ra, blind_sol.dec, cone_radius, g_max)
        .unwrap_or_else(|_| {
            // Fallback: extract cone stars directly from the loaded blind index
            extract_cone_from_index(&index_arc, blind_sol.ra, blind_sol.dec, cone_radius)
        });

    let arc_stars = Arc::new(cone_stars);

    let prior = solve::Prior {
        arcsec_per_px: blind_sol.arcsec_per_px,
        parity: if blind_sol.mirrored { -1.0 } else { 1.0 },
        centre: (blind_sol.ra, blind_sol.dec),
    };

    if arc_stars.len() >= 15 {
        let cache = solve::BlindCache::new(arc_stars.clone());
        if let Ok(refined) = solve::solve(
            &sky_frame,
            &[(blind_sol.ra, blind_sol.dec)],
            &cache,
            1.5,
            Some(&prior),
        ) {
            let field_stars = collect_field_stars(&refined.wcs, &arc_stars, epoch, 120);
            let field_dsos = collect_field_dsos(&refined.wcs);
            return Ok((
                PlateSolveReport {
                    wcs: refined.wcs,
                    method: SolveMethod::BlindQuadIndex,
                    target_hint: hints.first().map(|h| h.2.clone()),
                    matched_stars: refined.matched,
                    fit_rms_px: refined.fit_rms_px,
                    held_out_rms_px: Some(refined.held_out_rms_px),
                    poly_degree: refined.degree,
                    elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
                    field_stars,
                    field_dsos,
                },
                Some(index_arc),
            ));
        }
    }

    // fulgur-solve returns only a centre, scale and parity (no rotation), so a
    // WCS needs the Gaia refinement above to succeed.
    Err(format!(
        "Blind solver placed the frame near RA {:.3}°, Dec {:+.3}° ({:.2}″/px), but refinement against {} local catalogue stars failed.",
        blind_sol.ra,
        blind_sol.dec,
        blind_sol.arcsec_per_px,
        arc_stars.len()
    ))
}

fn extract_cone_from_index(
    index: &fulgur_solve::Index,
    ra_deg: f64,
    dec_deg: f64,
    radius_deg: f64,
) -> Vec<catalogue::GaiaStar> {
    let mut out = Vec::new();
    for s in index.stars() {
        let (ra, dec) = s.radec();
        if catalogue::separation(ra_deg, dec_deg, ra, dec) <= radius_deg {
            out.push(catalogue::GaiaStar {
                source_id: 0,
                ra,
                dec,
                pmra: 0.0,
                pmdec: 0.0,
                g: f64::from(s.g),
            });
        }
    }
    out.sort_by(|a, b| a.g.total_cmp(&b.g));
    out
}
