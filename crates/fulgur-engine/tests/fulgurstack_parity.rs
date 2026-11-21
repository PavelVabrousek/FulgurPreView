//! Star-metric parity between FulgurPreView and FulgurStack.
//!
//! The two applications live in separate repositories (ADR-013) and each keeps its own copy
//! of the star photometry. This test guards against the copies drifting apart: FulgurPreView
//! must report the same detected star count, median HFR, moment FWHM and eccentricity as
//! FulgurStack's `inspect` command on the same frames.
//!
//! Reference values: `tests/fixtures/fulgurstack_parity_seestar.csv`.
//!
//! Test data: the 32 Seestar S50 subs in `FulgurStack/SeestarS50 Subs` (not committed to this
//! repository). The test looks in `FULGUR_PARITY_DATA` if set, otherwise in the sibling
//! checkout `../FulgurStack/SeestarS50 Subs`. Without the data the comparison is skipped
//! with a note (the fixture is still validated).
//!
//! Regenerating the reference after an intentional change in FulgurStack:
//! 1. In the FulgurStack checkout: `cargo build --release --bin fulgurstack`
//! 2. `fulgurstack inspect --input "<FulgurStack>/SeestarS50 Subs" --report inspect.json --csv inspect.csv`
//! 3. Copy the columns `file_name;detected_stars;hfr_px;moment_fwhm_px;eccentricity` into the
//!    fixture (sorted by file name) and update the commit noted in its header.

use fulgur_core::LayoutChoice;
use fulgur_engine::stars::analyze_frame_stars;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

/// The reference CSV has 6 decimals; allow rounding plus float noise.
const TOLERANCE: f64 = 1e-5;

struct Reference {
    file_name: String,
    detected_stars: usize,
    hfr_px: f64,
    moment_fwhm_px: f64,
    eccentricity: f64,
}

fn load_reference() -> Vec<Reference> {
    let text = include_str!("fixtures/fulgurstack_parity_seestar.csv");
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .skip(1) // column header
        .map(|line| {
            let f: Vec<&str> = line.split(';').collect();
            assert_eq!(f.len(), 5, "bad fixture line: {line}");
            Reference {
                file_name: f[0].to_owned(),
                detected_stars: f[1].parse().unwrap(),
                hfr_px: f[2].parse().unwrap(),
                moment_fwhm_px: f[3].parse().unwrap(),
                eccentricity: f[4].parse().unwrap(),
            }
        })
        .collect()
}

fn data_dir() -> PathBuf {
    std::env::var_os("FULGUR_PARITY_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../FulgurStack/SeestarS50 Subs")
        })
}

#[test]
fn reference_fixture_is_complete() {
    let reference = load_reference();
    assert_eq!(reference.len(), 32);
    assert!(
        reference
            .iter()
            .all(|r| r.detected_stars > 0 && r.hfr_px > 0.0)
    );
}

#[test]
fn star_metrics_match_fulgurstack_on_seestar_subs() {
    let dir = data_dir();
    if !dir.is_dir() {
        eprintln!(
            "SKIPPED FulgurStack parity: test data not found at {} (set FULGUR_PARITY_DATA).",
            dir.display()
        );
        return;
    }
    let cancel = AtomicBool::new(false);
    let mut failures = Vec::new();
    for r in load_reference() {
        let path = dir.join(&r.file_name);
        assert!(path.is_file(), "missing test frame {}", path.display());
        let report = analyze_frame_stars(&path, 0, LayoutChoice::Auto, &cancel).unwrap();
        let close = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() <= TOLERANCE);
        if report.star_count != r.detected_stars
            || !close(report.median_hfr_px, r.hfr_px)
            || !close(report.median_fwhm_px, r.moment_fwhm_px)
            || !close(report.median_eccentricity, r.eccentricity)
        {
            failures.push(format!(
                "{}: stars {} vs {}, HFR {:?} vs {}, FWHM {:?} vs {}, ecc {:?} vs {}",
                r.file_name,
                report.star_count,
                r.detected_stars,
                report.median_hfr_px,
                r.hfr_px,
                report.median_fwhm_px,
                r.moment_fwhm_px,
                report.median_eccentricity,
                r.eccentricity
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "FulgurPreView star metrics differ from FulgurStack:\n{}",
        failures.join("\n")
    );
}
