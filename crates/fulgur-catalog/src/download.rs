//! Local Gaia DR3 tile cache reader (`g{limit}_d{dec:+03}_r{ra:03}.csv`).
//!
//! Reads cached 2×2 degree sky tiles shared with `FulgurStack` in
//! `%LOCALAPPDATA%\FulgurStack\gaia_cache`.

use std::path::Path;

use crate::sky::separation_deg;
use crate::xpsd::GaiaStar;

/// Side of one sky tile in degrees.
const TILE: f64 = 2.0;

fn tile_file(cache: &Path, dec_idx: i32, ra_idx: i32, limit: u8) -> std::path::PathBuf {
    let dec_deg = dec_idx * TILE as i32 - 90;
    let ra_deg = ra_idx * TILE as i32;
    cache.join(format!("g{limit}_d{dec_deg:+03}_r{ra_deg:03}.csv"))
}

fn covering_tiles(ra: f64, dec: f64, radius: f64) -> Vec<(i32, i32)> {
    let d0 = ((dec - radius + 90.0) / TILE).floor() as i32;
    let d1 = ((dec + radius + 90.0) / TILE).floor() as i32;
    let n_dec = (180.0 / TILE) as i32;
    let n_ra = (360.0 / TILE) as i32;
    let mut out = vec![];
    for di in d0.max(0)..=d1.min(n_dec - 1) {
        let dec_lo = di as f64 * TILE - 90.0;
        let dec_hi = dec_lo + TILE;
        let edge = dec.clamp(dec_lo, dec_hi);
        let cos = edge.to_radians().cos().abs();
        let half = if cos < 1e-6 || radius >= 90.0 {
            180.0
        } else {
            let s = (radius.to_radians().sin() / cos).min(1.0);
            s.asin().to_degrees()
        };
        if half >= 180.0 - 1e-6 {
            for ri in 0..n_ra {
                out.push((di, ri));
            }
        } else {
            let r0 = ((ra - half) / TILE).floor() as i32;
            let r1 = ((ra + half) / TILE).floor() as i32;
            for r in r0..=r1 {
                let ri = r.rem_euclid(n_ra);
                if !out.contains(&(di, ri)) {
                    out.push((di, ri));
                }
            }
        }
    }
    out
}

fn parse_csv(text: &str) -> Result<Vec<GaiaStar>, String> {
    let mut out = vec![];
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("ra") || line.starts_with('#') {
            continue;
        }
        let mut f = line.split(',');
        let mut num = || -> Option<f64> {
            let s = f.next()?.trim().trim_matches('"');
            if s.is_empty() || s.eq_ignore_ascii_case("null") || s.eq_ignore_ascii_case("nan") {
                Some(0.0)
            } else {
                s.parse().ok()
            }
        };
        let (Some(ra), Some(dec), Some(pmra), Some(pmdec), Some(g)) =
            (num(), num(), num(), num(), num())
        else {
            return Err(format!("Cannot parse cached Gaia CSV line {}", line_no + 1));
        };
        out.push(GaiaStar {
            source_id: 0,
            ra,
            dec,
            pmra,
            pmdec,
            g,
        });
    }
    Ok(out)
}

/// Read stars brighter than `g_max` within `radius` degrees of `(ra, dec)` from
/// the local tile cache `cache`.
pub fn cone(
    cache: &Path,
    ra: f64,
    dec: f64,
    radius: f64,
    g_max: f64,
) -> Result<Vec<GaiaStar>, String> {
    let req_limit = (g_max.ceil() as i32).clamp(10, 21) as u8;
    let mut stars = vec![];
    let mut any_tile_found = false;
    for (di, ri) in covering_tiles(ra, dec, radius) {
        // Check exact limit or any deeper cached tile (e.g. g16, g17, g18)
        let mut found_file = None;
        for lim in req_limit..=21 {
            let p = tile_file(cache, di, ri, lim);
            if p.is_file() {
                found_file = Some(p);
                break;
            }
        }
        let Some(file) = found_file else {
            continue;
        };
        any_tile_found = true;
        let csv = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        for s in parse_csv(&csv)? {
            if s.g <= g_max && separation_deg(ra, dec, s.ra, s.dec) <= radius {
                stars.push(s);
            }
        }
    }
    if !any_tile_found {
        return Err(format!(
            "No cached Gaia tiles covering RA={ra:.2}°, Dec={dec:.2}° in {}",
            cache.display()
        ));
    }
    stars.sort_by(|a, b| a.g.total_cmp(&b.g));
    Ok(stars)
}
