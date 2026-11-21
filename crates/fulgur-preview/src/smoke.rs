//! Automated Milestone 1 (M1) end-to-end test scenarios and latency benchmarks.

use crate::cache::{CacheKey, FrameCache};
use crate::directory::{DirectoryPlaylist, SortOrder};
use fulgur_catalog::config::CatalogConfig;
use fulgur_core::culling::CullingHistory;
use fulgur_core::fits::save_wcs_to_file;
use fulgur_core::raw::{RawStage, build_synthetic_dng_raw};
use fulgur_core::telemetry::peak_rss_bytes;
use fulgur_core::{Error, LayoutChoice, Mosaic, Result};
use fulgur_engine::sky::catalogue::{self, GaiaStar};
use fulgur_engine::sky::fits_wcs::cards as wcs_to_cards;
use fulgur_engine::sky::solve::{BlindCache, Prior, solve as solve_wcs_field};
use fulgur_engine::sky::{SkyFrame, load_raw_for_solving, solve_raw_frame};
use fulgur_engine::stars::analyze_frame_stars;
use fulgur_engine::stretch::{
    ChannelView, StretchMode, StretchParams, load_detail_roi_rgb8, load_frame_overview,
    load_raw_stage1_overview, load_raw_stage2_overview,
};
use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub name: &'static str,
    pub passed: bool,
    pub latency_ms: f64,
    pub details: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmokeTestReport {
    pub milestone: &'static str,
    pub status: &'static str,
    pub scenarios: Vec<ScenarioResult>,
    pub real_folder_frames_tested: usize,
    pub avg_cold_open_ms: Option<f64>,
    pub avg_prefetched_switch_ms: Option<f64>,
    pub peak_ram_mb: Option<f64>,
}

fn make_fits_card(key: &str, val: &str) -> [u8; 80] {
    let mut card = [b' '; 80];
    let text = if key == "END" {
        "END".to_owned()
    } else {
        format!("{key:<8}= {val:>20}")
    };
    let bytes = text.as_bytes();
    card[..bytes.len().min(80)].copy_from_slice(&bytes[..bytes.len().min(80)]);
    card
}

fn write_header_block(cards: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(k, v) in cards {
        out.extend_from_slice(&make_fits_card(k, v));
    }
    out.extend_from_slice(&make_fits_card("END", ""));
    while out.len() % 2880 != 0 {
        out.push(b' ');
    }
    out
}

fn pad_data_block(mut data: Vec<u8>) -> Vec<u8> {
    while data.len() % 2880 != 0 {
        data.push(0);
    }
    data
}

/// Run all Milestone 1 automated verification scenarios and optional real-folder benchmark.
pub fn run_m1_verification(real_target: Option<&Path>) -> Result<SmokeTestReport> {
    let cancel = AtomicBool::new(false);
    let temp = tempfile::tempdir()?;
    let dir = temp.path();
    let mut scenarios = Vec::new();

    // Scenario 1: Seestar / 16-bit Bayer CFA FITS (BITPIX=16, BZERO=32768, GRBG) + Camera DB lookup
    {
        let t0 = Instant::now();
        let path = dir.join("Light_1.fits");
        let (w, h) = (640_usize, 480_usize);
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "640"),
            ("NAXIS2", "480"),
            ("BZERO", "32768"),
            ("BSCALE", "1"),
            ("INSTRUME", "'ZWO Seestar S50'"),
            ("BAYERPAT", "'GRBG'"),
            ("OBJECT", "'M 42'"),
            ("EXPTIME", "10.0"),
            ("GAIN", "80"),
            ("CCD-TEMP", "15.5"),
            ("SITELAT", "50.08"),
            ("SITELONG", "14.42"),
        ]);
        let mut data = Vec::with_capacity(w * h * 2);
        for y in 0..h {
            for x in 0..w {
                let star = if (x as isize - 320).pow(2) + (y as isize - 240).pow(2) < 25 {
                    25000_u16
                } else {
                    1200_u16 + ((x + y) % 40) as u16
                };
                let stored = (i32::from(star) - 32768) as i16;
                data.extend_from_slice(&stored.to_be_bytes());
            }
        }
        bytes.extend_from_slice(&pad_data_block(data));
        std::fs::write(&path, &bytes)?;
        let before_meta = std::fs::metadata(&path)?;

        let ov = load_frame_overview(&path, 0, LayoutChoice::Auto, &cancel)?;
        let rgb8 = ov.render_rgb8(ov.unlinked_stretch);
        let detail = load_detail_roi_rgb8(
            &path,
            0,
            LayoutChoice::Auto,
            [w, h],
            [280, 200, 80, 80],
            ov.norm_min,
            ov.norm_span,
            ov.unlinked_stretch,
            &cancel,
        )?;
        let after_meta = std::fs::metadata(&path)?;
        let passed = ov.native_size == [w, h]
            && ov.info.mosaic == Mosaic::Bayer
            && ov.default_stretch
            && rgb8.len() == ov.width * ov.height * 3
            && detail.len() == 80 * 80 * 3
            && before_meta.len() == after_meta.len();
        scenarios.push(ScenarioResult {
            name: "S1_FITS_16bit_Bayer_GRBG_and_VNG_ROI",
            passed,
            latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
            details: format!(
                "640x480 BITPIX=16 BZERO=32768 GRBG overview + 80x80 1:1 VNG ROI, read-only verified ({} cards)",
                ov.cards.len()
            ),
        });
    }

    // Scenario 2: All FITS BITPIX variants (8, 32, -32 RGB cube, -64) & Multi-Extension FITS (MEF)
    {
        let t0 = Instant::now();
        // BITPIX = -32 3D RGB cube
        let rgb_path = dir.join("Light_2.fits");
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "-32"),
            ("NAXIS", "3"),
            ("NAXIS1", "64"),
            ("NAXIS2", "64"),
            ("NAXIS3", "3"),
        ]);
        let mut data = Vec::with_capacity(64 * 64 * 3 * 4);
        for c in 0..3 {
            for i in 0..(64 * 64) {
                let val = (i as f32 / 4095.0) * (0.2 + 0.25 * c as f32);
                data.extend_from_slice(&val.to_be_bytes());
            }
        }
        bytes.extend_from_slice(&pad_data_block(data));
        std::fs::write(&rgb_path, bytes)?;
        let ov_rgb = load_frame_overview(&rgb_path, 0, LayoutChoice::Auto, &cancel)?;

        // Multi-Extension FITS (MEF) with 2 image extensions
        let mef_path = dir.join("Light_10_mef.fits");
        let mut mef_bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "8"),
            ("NAXIS", "0"),
            ("EXTEND", "T"),
        ]);
        for (ext_name, w, h, bp) in [("EXT_A", 32_usize, 24_usize, 8_i32), ("EXT_B", 48, 36, -64)] {
            mef_bytes.extend_from_slice(&write_header_block(&[
                ("XTENSION", "'IMAGE   '"),
                ("BITPIX", &bp.to_string()),
                ("NAXIS", "2"),
                ("NAXIS1", &w.to_string()),
                ("NAXIS2", &h.to_string()),
                ("EXTNAME", &format!("'{ext_name}'")),
            ]));
            let mut ext_data = Vec::new();
            if bp == 8 {
                ext_data.resize(w * h, 128_u8);
            } else {
                for i in 0..(w * h) {
                    ext_data.extend_from_slice(&((i as f64) / 1000.0).to_be_bytes());
                }
            }
            mef_bytes.extend_from_slice(&pad_data_block(ext_data));
        }
        std::fs::write(&mef_path, mef_bytes)?;
        let ov_ext0 = load_frame_overview(&mef_path, 0, LayoutChoice::Mono, &cancel)?;
        let ov_ext1 = load_frame_overview(&mef_path, 1, LayoutChoice::Mono, &cancel)?;

        let passed = ov_rgb.info.mosaic == Mosaic::Rgb
            && ov_ext0.hdu_names.len() == 2
            && ov_ext0.native_size == [32, 24]
            && ov_ext1.native_size == [48, 36]
            && ov_ext1.info.bitpix == -64;
        scenarios.push(ScenarioResult {
            name: "S2_FITS_All_BITPIX_and_MEF_Extensions",
            passed,
            latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
            details:
                "3D float RGB cube + MEF HDU switching (#0 32x24 BITPIX=8, #1 48x36 BITPIX=-64)"
                    .to_owned(),
        });
    }

    // Scenario 3: Monolithic XISF + Standard Formats (PNG/JPEG/TIFF) + Format-Aware Default Stretch
    {
        let t0 = Instant::now();
        let xisf_path = dir.join("Light_3.xisf");
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<xisf version="1.0">
  <Image geometry="32:32:1" sampleFormat="UInt16" colorSpace="Gray" location="attachment:2048:2048">
    <ColorFilterArray pattern="RGGB" width="2" height="2"/>
    <FITSKeyword name="OBJECT" value="'NGC 7000'" comment="North America Nebula"/>
    <FITSKeyword name="EXPTIME" value="120.0" comment="Exposure time"/>
  </Image>
</xisf>"#;
        let xml_bytes = xml.as_bytes();
        let mut xisf_bytes = Vec::new();
        xisf_bytes.extend_from_slice(b"XISF0100");
        xisf_bytes.extend_from_slice(&(xml_bytes.len() as u32).to_le_bytes());
        xisf_bytes.extend_from_slice(&0_u32.to_le_bytes());
        xisf_bytes.extend_from_slice(xml_bytes);
        xisf_bytes.resize(2048, 0);
        for i in 0..(32 * 32) {
            let v: u16 = 800 + (i as u16 % 2000);
            xisf_bytes.extend_from_slice(&v.to_le_bytes());
        }
        let mut xf = std::fs::File::create(&xisf_path)?;
        xf.write_all(&xisf_bytes)?;

        let png_path = dir.join("Standard_Photo.png");
        let img = image::RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 5) as u8, 160])
        });
        img.save(&png_path)
            .map_err(|_| Error::Invalid("Could not save test PNG."))?;

        let ov_xisf = load_frame_overview(&xisf_path, 0, LayoutChoice::Auto, &cancel)?;
        let ov_png = load_frame_overview(&png_path, 0, LayoutChoice::Auto, &cancel)?;

        let passed = ov_xisf.default_stretch
            && ov_xisf.info.mosaic == Mosaic::Bayer
            && !ov_png.default_stretch
            && ov_png.native_size == [64, 48];
        scenarios.push(ScenarioResult {
            name: "S3_XISF_and_Standard_Format_Aware_Stretch",
            passed,
            latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
            details: format!(
                "XISF default_stretch={} (expected true), PNG default_stretch={} (expected false)",
                ov_xisf.default_stretch, ov_png.default_stretch
            ),
        });
    }

    // Scenario 4: Natural Directory Sorting, Background Neighbor Prefetch & Sub-10ms Switching
    {
        let t0 = Instant::now();
        let mut playlist =
            DirectoryPlaylist::open(&dir.join("Light_1.fits"), SortOrder::NaturalName);
        let names: Vec<String> = playlist
            .files
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        let _ = t0;
        // Verify natural sort: Light_1.fits, Light_2.fits, Light_3.xisf, Light_10_mef.fits, Standard_Photo.png
        let natural_ok = names.len() == 5
            && names[0] == "Light_1.fits"
            && names[1] == "Light_2.fits"
            && names[2] == "Light_3.xisf"
            && names[3] == "Light_10_mef.fits";

        let mut cache = FrameCache::new();
        let active_key = CacheKey {
            path: playlist.current_path().unwrap().to_path_buf(),
            hdu_index: 0,
            layout: LayoutChoice::Auto,
        };
        cache.request_active_and_neighbors(active_key, playlist.neighbors_for_prefetch(2));

        // Wait until active + prefetched neighbors finish loading
        let wait_start = Instant::now();
        while wait_start.elapsed() < Duration::from_secs(3) {
            cache.poll();
            if !cache.is_busy() && cache.cached_count() >= 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // Now measure switching latency to i+1 (Light_2.fits) which was prefetched in the background!
        playlist.next();
        let switch_t0 = Instant::now();
        let next_key = CacheKey {
            path: playlist.current_path().unwrap().to_path_buf(),
            hdu_index: 0,
            layout: LayoutChoice::Auto,
        };
        let hit = cache.get(&next_key);
        let switch_ms = switch_t0.elapsed().as_secs_f64() * 1000.0;
        let render_ok = hit.is_some_and(|f| {
            let rgb8 = f.render_rgb8(f.resolve_stretch(
                StretchMode::AutoLinked,
                &StretchParams::identity(),
                false,
                ChannelView::Rgb,
            ));
            !rgb8.is_empty()
        });

        let passed = natural_ok && render_ok && switch_ms < 10.0;
        scenarios.push(ScenarioResult {
            name: "S4_Directory_NaturalSort_and_Prefetch_Switch",
            passed,
            latency_ms: switch_ms,
            details: format!(
                "Natural sort verified ({:?}); prefetched switch latency = {:.3} ms (Gate < 10 ms)",
                names, switch_ms
            ),
        });
    }

    // Scenario 5: Fast Culling (X/Delete -> _Rejected/) & Undo (Ctrl+Z)
    {
        let t0 = Instant::now();
        let mut playlist =
            DirectoryPlaylist::open(&dir.join("Light_2.fits"), SortOrder::NaturalName);
        let initial_len = playlist.len();
        let target_file = playlist.current_path().unwrap().to_path_buf();
        let mut culling = CullingHistory::default();

        let entry = culling.reject_file(&target_file)?;
        playlist.remove_current();
        let rejected_exists = entry.rejected_path.exists() && !target_file.exists();
        let len_after_reject = playlist.len();

        let restored = culling.undo_last()?.unwrap();
        playlist.rescan(Some(&restored.original_path));
        let restored_ok = target_file.exists()
            && !entry.rejected_path.exists()
            && playlist.len() == initial_len
            && playlist.current_path() == Some(target_file.as_path());

        let passed = rejected_exists && len_after_reject == initial_len - 1 && restored_ok;
        scenarios.push(ScenarioResult {
            name: "S5_Culling_Reject_and_Undo_CtrlZ",
            passed,
            latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
            details: "Moved Light_2.fits to _Rejected/ and restored via Undo without data loss"
                .to_owned(),
        });
    }

    // Scenario 6 (M2): Two-Stage DSLR RAW Pipeline (Instant Stage-1 Preview < 20ms + Stage-2 Linear CFA + VNG ROI)
    {
        let dng_path = dir.join("Light_4_EOS6D.dng");
        let dng_bytes = build_synthetic_dng_raw(256, 192);
        std::fs::write(&dng_path, &dng_bytes)?;

        let t_s1 = Instant::now();
        let stage1 = load_raw_stage1_overview(&dng_path, LayoutChoice::Auto, &cancel)?;
        let stage1_ms = t_s1.elapsed().as_secs_f64() * 1000.0;

        let t_s2 = Instant::now();
        let stage2 = load_raw_stage2_overview(&dng_path, LayoutChoice::Auto, &cancel)?;
        let stage2_ms = t_s2.elapsed().as_secs_f64() * 1000.0;

        let detail = load_detail_roi_rgb8(
            &dng_path,
            0,
            LayoutChoice::Auto,
            [256, 192],
            [64, 48, 64, 64],
            stage2.norm_min,
            stage2.norm_span,
            stage2.unlinked_stretch,
            &cancel,
        )?;

        let passed = stage1.raw_stage == RawStage::Stage1Preview
            && !stage1.default_stretch
            && stage2.raw_stage == RawStage::Stage2Linear
            && stage2.default_stretch
            && stage2.native_size == [256, 192]
            && stage2.info.mosaic == Mosaic::Bayer
            && detail.len() == 64 * 64 * 3;
        scenarios.push(ScenarioResult {
            name: "S6_DSLR_RAW_Two_Stage_Preview_and_Linear_CFA",
            passed,
            latency_ms: stage1_ms,
            details: format!(
                "Stage 1 preview = {:.2} ms (64x48 sRGB), Stage 2 linear CFA = {:.2} ms (256x192 RGGB + 1:1 VNG ROI)",
                stage1_ms, stage2_ms
            ),
        });
    }

    // Scenario 7 (M2): Interactive Signal Histogram ("Zoom to Signal" Peak Isolation & Custom [Min, Max] Sliders)
    {
        let t0 = Instant::now();
        let fits_path = dir.join("Light_1.fits");
        let ov = load_frame_overview(&fits_path, 0, LayoutChoice::Auto, &cancel)?;
        let [sig_min, sig_max] = ov.histogram.signal_range;
        let med_g = ov.histogram.stats[1].median_norm;

        // Verify that "Zoom to Signal" tightly frames the linear sky-background peak (< 15% of full [0..1] span)
        let signal_frames_peak =
            sig_min >= 0.0 && (sig_max - sig_min) < 0.15 && med_g >= sig_min && med_g <= sig_max;

        let sig_bins_total: u32 = ov.histogram.signal_bins[1].iter().sum();
        let custom_bins = ov.bins_for_range(sig_min * 0.9, (sig_max * 1.1).min(1.0));
        let custom_bins_total: u32 = custom_bins[1].iter().sum();

        let passed = signal_frames_peak && sig_bins_total > 0 && custom_bins_total > 0;
        scenarios.push(ScenarioResult {
            name: "S7_Signal_Histogram_Peak_Zoom_and_MinMax_Sliders",
            passed,
            latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
            details: format!(
                "Signal window [{:.4} .. {:.4}] isolates median {:.4} ({:.0} ADU, σ={:.1} ADU)",
                sig_min,
                sig_max,
                med_g,
                ov.histogram.stats[1].median_adu,
                ov.histogram.stats[1].sigma_adu
            ),
        });
    }

    // Scenario 8 (M3): Real-Time Star Photometry (FWHM, HFR, Eccentricity), Hot-Pixel Suppression, Cursor Star Lookup & 3x3 Tilt Map
    {
        let star_fits_path = dir.join("Light_5_StarField.fits");
        let (w, h) = (640_usize, 480_usize);
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "640"),
            ("NAXIS2", "480"),
            ("BZERO", "32768"),
            ("BSCALE", "1"),
            ("INSTRUME", "'ZWO Seestar S50'"),
            ("BAYERPAT", "'GRBG'"),
            ("XPIXSZ", "2.9"),
            ("FOCALLEN", "250.0"),
            ("EXPTIME", "10.0"),
        ]);
        let mut pixels = vec![1000.0_f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let noise = ((x * 7919 + y * 104_729) % 97) as f32 / 97.0 * 6.0 - 3.0;
                pixels[y * w + x] += noise;
            }
        }
        // 6x5 grid of Gaussian stars across all 9 zones of the 3x3 sensor tilt map
        for gy in 0..5 {
            for gx in 0..6 {
                let cx = 64.0 + gx as f64 * 100.0 + 0.3;
                let cy = 56.0 + gy as f64 * 90.0 + 0.2;
                let in_left_or_right = cx < w as f64 / 3.0 || cx > w as f64 * 2.0 / 3.0;
                let in_top_or_bottom = cy < h as f64 / 3.0 || cy > h as f64 * 2.0 / 3.0;
                let is_corner = in_left_or_right && in_top_or_bottom;
                let sigma = if is_corner { 1.85 } else { 1.55 };
                for yy in (cy as usize - 12)..=(cy as usize + 12) {
                    for xx in (cx as usize - 12)..=(cx as usize + 12) {
                        let dx = (xx as f64 - cx) / sigma;
                        let dy = (yy as f64 - cy) / sigma;
                        pixels[yy * w + xx] +=
                            (11_000.0 * (-0.5 * (dx * dx + dy * dy)).exp()) as f32;
                    }
                }
            }
        }
        // Inject 6 single-sample hot pixels (52,000 ADU)
        for k in 0..6 {
            pixels[(70 + k * 42) * w + (84 + k * 48)] = 52_000.0;
        }
        let mut data = Vec::with_capacity(w * h * 2);
        for &v in &pixels {
            let adu = v.round().clamp(0.0, 65535.0) as i32;
            let stored = (adu - 32768) as i16;
            data.extend_from_slice(&stored.to_be_bytes());
        }
        bytes.extend_from_slice(&pad_data_block(data));
        std::fs::write(&star_fits_path, &bytes)?;

        let t0 = Instant::now();
        let report = analyze_frame_stars(&star_fits_path, 0, LayoutChoice::Auto, &cancel)?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let fwhm = report.median_fwhm_px.unwrap_or(0.0);
        let hfr = report.median_hfr_px.unwrap_or(0.0);
        let ecc = report.median_eccentricity.unwrap_or(1.0);
        let all_9_zones_populated = report.tilt_map.cells.iter().all(|c| c.star_count >= 1);
        let cursor_star = report.nearest_star(264.3, 236.2, 8.0);

        let passed = report.star_count == 30
            && report.suppressed_hot_pixels >= 3
            && (3.4..4.6).contains(&fwhm)
            && (1.6..2.5).contains(&hfr)
            && ecc < 0.30
            && report.median_fwhm_arcsec.is_some()
            && all_9_zones_populated
            && report.tilt_map.corner_to_center_fwhm_pct.unwrap_or(0.0) > 5.0
            && cursor_star.is_some();

        scenarios.push(ScenarioResult {
            name: "S8_Star_Photometry_FWHM_HFR_and_3x3_Tilt_Map",
            passed,
            latency_ms: elapsed_ms,
            details: format!(
                "{} stars ({} hot px suppressed) · FWHM={:.2} px ({:.2}″) · HFR={:.2} px · Ecc={:.2} · 3x3 Curvature={:+.1}%",
                report.star_count,
                report.suppressed_hot_pixels,
                fwhm,
                report.median_fwhm_arcsec.unwrap_or(0.0),
                hfr,
                ecc,
                report.tilt_map.corner_to_center_fwhm_pct.unwrap_or(0.0)
            ),
        });
    }

    // Scenario 9 (M4): Local/Blind Gaia Plate Solving, Sky Overlays & Atomic WCS Save (FITS & XISF)
    {
        let t0 = Instant::now();
        let cfg = CatalogConfig::discover();

        // 9A. Synthetic TAN-SIP solver + Atomic WCS save to both FITS and XISF
        let ra0 = 83.822_f64;
        let dec0 = -5.391_f64;
        let scale_arcsec = 2.388_f64;
        let (w, h) = (1080_usize, 1920_usize);
        let mut cat_stars = Vec::with_capacity(42);
        let mut det_pts = Vec::with_capacity(42);
        for i in 0..42 {
            let u = ((i * 73 % 900) as f64) - 450.0;
            let v = ((i * 137 % 1600) as f64) - 800.0;
            let x = 540.0 + u;
            let y = 960.0 + v;
            let xi_arcsec = u * scale_arcsec;
            let eta_arcsec = v * scale_arcsec;
            let (ra, dec) = catalogue::deproject([xi_arcsec, eta_arcsec], ra0, dec0);
            let mag = 7.5 + (i % 6) as f64 * 0.6;
            cat_stars.push(GaiaStar {
                source_id: i as u64 + 1,
                ra,
                dec,
                pmra: 0.0,
                pmdec: 0.0,
                g: mag,
            });
            det_pts.push([x, y]);
        }
        let sky_frame = SkyFrame {
            name: "synthetic_orion",
            width: w,
            height: h,
            epoch: catalogue::GAIA_EPOCH,
            stars: &det_pts,
        };
        let blind_cache = BlindCache::new(std::sync::Arc::new(cat_stars));
        let prior = Prior {
            arcsec_per_px: scale_arcsec,
            parity: 1.0,
            centre: (ra0, dec0),
        };
        let synth_sol =
            solve_wcs_field(&sky_frame, &[(ra0, dec0)], &blind_cache, 1.5, Some(&prior))
                .map_err(|_| Error::Invalid("Synthetic TAN-SIP solver did not converge."))?;
        let wcs_cards: Vec<(String, String, String)> = wcs_to_cards(&synth_sol.wcs, [0.0, 0.0])
            .ok_or(Error::Invalid("Failed to generate WCS cards."))?
            .into_iter()
            .map(|(k, v, c)| (k, v, c.to_owned()))
            .collect();

        // Verify atomic WCS save on FITS (`Light_1.fits`) and XISF (`Light_3.xisf`)
        let fits_wcs_path = dir.join("Light_1_wcs.fits");
        std::fs::copy(dir.join("Light_1.fits"), &fits_wcs_path)?;
        let fits_before = load_frame_overview(&fits_wcs_path, 0, LayoutChoice::Auto, &cancel)?;
        save_wcs_to_file(&fits_wcs_path, 0, &wcs_cards)?;
        let fits_after = load_frame_overview(&fits_wcs_path, 0, LayoutChoice::Auto, &cancel)?;

        let xisf_wcs_path = dir.join("Light_3_wcs.xisf");
        std::fs::copy(dir.join("Light_3.xisf"), &xisf_wcs_path)?;
        let xisf_before = load_frame_overview(&xisf_wcs_path, 0, LayoutChoice::Auto, &cancel)?;
        save_wcs_to_file(&xisf_wcs_path, 0, &wcs_cards)?;
        let xisf_after = load_frame_overview(&xisf_wcs_path, 0, LayoutChoice::Auto, &cancel)?;

        let mut passed = synth_sol.fit_rms_px < 0.05
            && fits_before.pixels == fits_after.pixels
            && fits_after.header_wcs.is_some()
            && xisf_before.pixels == xisf_after.pixels
            && xisf_after.header_wcs.is_some();

        let mut details = format!(
            "Synthetic TAN-SIP RMS={:.3} px ({}★); FITS & XISF atomic .tmp WCS save bit-for-bit verified",
            synth_sol.fit_rms_px, synth_sol.matched
        );
        let mut solve_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // 9B. If a real target folder/file is supplied and local Gaia DR3 is installed, solve a real sub-frame!
        if let Some(target) = real_target
            && target.exists()
            && cfg.has_local_gaia()
        {
            let playlist = DirectoryPlaylist::open(target, SortOrder::NaturalName);
            if let Some(first_real) = playlist.files.first() {
                let t_real = Instant::now();
                let raw = load_raw_for_solving(first_real, 0, LayoutChoice::Auto, &cancel)
                    .map_err(|_| Error::Invalid("Failed to load real frame for solving."))?;
                let (real_rep, _) = solve_raw_frame(first_real, &raw, &cfg, None, &cancel)
                    .map_err(|_| Error::Invalid("Real frame plate solve failed."))?;
                solve_ms = t_real.elapsed().as_secs_f64() * 1000.0;

                // Test atomic WCS save on a temporary copy of the real FITS frame
                let real_copy = dir.join("Real_Sub_WCS_Copy.fit");
                std::fs::copy(first_real, &real_copy)?;
                let real_before = load_frame_overview(&real_copy, 0, LayoutChoice::Auto, &cancel)?;
                let real_cards = real_rep
                    .to_fits_cards()
                    .ok_or(Error::Invalid("Failed to serialize real WCS cards."))?;
                save_wcs_to_file(&real_copy, 0, &real_cards)?;
                let real_after = load_frame_overview(&real_copy, 0, LayoutChoice::Auto, &cancel)?;

                let (c_ra, c_dec) = real_rep.wcs.centre_sky();
                let real_ok = real_rep.fit_rms_px < 0.45
                    && real_rep.matched_stars >= 15
                    && real_before.pixels == real_after.pixels
                    && real_after.header_wcs.is_some();
                passed &= real_ok;
                details = format!(
                    "Real frame '{}' solved via {} in {:.0} ms: {}★ matched, RMS={:.3} px, Scale={:.3}″/px, Center=({}, {}), DSOs={}, atomic WCS save verified",
                    first_real
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    real_rep.method.label(),
                    solve_ms,
                    real_rep.matched_stars,
                    real_rep.fit_rms_px,
                    real_rep.wcs.scale(),
                    fulgur_catalog::sky::format_ra(c_ra),
                    fulgur_catalog::sky::format_dec(c_dec),
                    real_rep.field_dsos.len()
                );
            }
        }

        scenarios.push(ScenarioResult {
            name: "S9_Plate_Solving_Overlays_and_Atomic_WCS_Save",
            passed,
            latency_ms: solve_ms,
            details,
        });
    }

    // Optional Real-Data Benchmark if a path/folder was provided
    let mut real_folder_frames_tested = 0;
    let mut avg_cold_open_ms = None;
    let mut avg_prefetched_switch_ms = None;

    if let Some(target) = real_target {
        if target.exists() {
            let playlist = DirectoryPlaylist::open(target, SortOrder::NaturalName);
            let count = playlist.len().min(6);
            if count > 0 {
                let mut cold_total_ms = 0.0;
                let mut cache = FrameCache::new();
                for path in playlist.files.iter().take(count) {
                    let t0 = Instant::now();
                    let frame = load_frame_overview(path, 0, LayoutChoice::Auto, &cancel)?;
                    let _rgb8 = frame.render_rgb8(frame.unlinked_stretch);
                    cold_total_ms += t0.elapsed().as_secs_f64() * 1000.0;
                    cache.insert(
                        CacheKey {
                            path: path.clone(),
                            hdu_index: 0,
                            layout: LayoutChoice::Auto,
                        },
                        std::sync::Arc::new(frame),
                    );
                }
                real_folder_frames_tested = count;
                avg_cold_open_ms = Some(cold_total_ms / count as f64);

                // Measure prefetched switching + GPU RGB8 render across cached frames
                let mut switch_total_ms = 0.0;
                for path in playlist.files.iter().take(count) {
                    let t0 = Instant::now();
                    let key = CacheKey {
                        path: path.clone(),
                        hdu_index: 0,
                        layout: LayoutChoice::Auto,
                    };
                    if let Some(frame) = cache.get(&key) {
                        let _rgb8 = frame.render_rgb8(frame.unlinked_stretch);
                    }
                    switch_total_ms += t0.elapsed().as_secs_f64() * 1000.0;
                }
                avg_prefetched_switch_ms = Some(switch_total_ms / count as f64);
            }
        }
    }

    let all_passed = scenarios.iter().all(|s| s.passed);
    let peak_ram_mb = peak_rss_bytes().map(|b| b as f64 / (1024.0 * 1024.0));

    Ok(SmokeTestReport {
        milestone: "M4",
        status: if all_passed { "PASS" } else { "FAIL" },
        scenarios,
        real_folder_frames_tested,
        avg_cold_open_ms,
        avg_prefetched_switch_ms,
        peak_ram_mb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_m1_automated_scenarios_pass() {
        let report = run_m1_verification(None).unwrap();
        assert_eq!(report.status, "PASS", "Scenarios: {:?}", report.scenarios);
        for s in &report.scenarios {
            assert!(s.passed, "Scenario failed: {:?}", s);
        }
    }
}
