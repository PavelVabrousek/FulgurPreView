//! Common spherical geometry and sexagesimal formatting helpers.

/// Great-circle angular distance in degrees between two `(ra_deg, dec_deg)` points.
pub fn separation_deg(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let (r1, d1) = (ra1.to_radians(), dec1.to_radians());
    let (r2, d2) = (ra2.to_radians(), dec2.to_radians());
    let sin_d = ((d2 - d1) * 0.5).sin();
    let sin_r = ((r2 - r1) * 0.5).sin();
    let a = sin_d * sin_d + d1.cos() * d2.cos() * sin_r * sin_r;
    2.0 * a.clamp(0.0, 1.0).sqrt().asin().to_degrees()
}

/// Format right ascension `ra_deg` (`0..360`) as `HHh MMm SS.ss`.
pub fn format_ra(ra_deg: f64) -> String {
    let total_sec = (ra_deg.rem_euclid(360.0) / 15.0) * 3600.0;
    let total_hundredths = (total_sec * 100.0).round() as i64;
    let h = (total_hundredths / 360_000) % 24;
    let m = (total_hundredths / 6_000) % 60;
    let s = (total_hundredths % 6_000) as f64 / 100.0;
    format!("{h:02}h {m:02}m {s:05.2}s")
}

/// Format declination `dec_deg` (`-90..+90`) as `±DD° MM′ SS.s″`.
pub fn format_dec(dec_deg: f64) -> String {
    let sign = if dec_deg < 0.0 { '-' } else { '+' };
    let total_tenths = (dec_deg.abs().min(90.0) * 36_000.0).round() as i64;
    let d = total_tenths / 36_000;
    let m = (total_tenths / 600) % 60;
    let s = (total_tenths % 600) as f64 / 10.0;
    format!("{sign}{d:02}° {m:02}′ {s:04.1}″")
}
