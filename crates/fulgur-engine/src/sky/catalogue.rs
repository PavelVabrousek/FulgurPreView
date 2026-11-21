//! Gaia DR3 stars and gnomonic tangent-plane projection.

/// Reference epoch of Gaia DR3 positions.
pub const GAIA_EPOCH: f64 = 2016.0;
const ARCSEC: f64 = 180. / std::f64::consts::PI * 3600.;

/// Parse an ISO-8601 `DATE-OBS` string (`YYYY-MM-DD...`) into a decimal year
/// for proper-motion propagation from [`GAIA_EPOCH`].
pub fn decimal_year(date_obs: &str) -> Option<f64> {
    let s = date_obs.trim().trim_matches('\'').trim();
    if s.len() < 10 {
        return None;
    }
    let year: f64 = s.get(0..4)?.parse().ok()?;
    let month: f64 = s.get(5..7)?.parse().ok()?;
    let day: f64 = s.get(8..10)?.parse().ok()?;
    if !(1800.0..=2200.0).contains(&year)
        || !(1.0..=12.0).contains(&month)
        || !(1.0..=31.0).contains(&day)
    {
        return None;
    }
    Some(year + ((month - 1.0) * 30.4375 + (day - 1.0)) / 365.25)
}

#[derive(Clone, Debug)]
pub struct GaiaStar {
    pub source_id: u64,
    pub ra: f64,
    pub dec: f64,
    /// `mu_alpha*` (mas/yr, already multiplied by `cos(dec)`) and `mu_delta` (mas/yr).
    pub pmra: f64,
    pub pmdec: f64,
    pub g: f64,
}

impl GaiaStar {
    /// ICRS position at a decimal-year epoch.
    pub fn at(&self, epoch: f64) -> (f64, f64) {
        let dt = epoch - GAIA_EPOCH;
        let dec = self.dec + self.pmdec * dt / 3.6e6;
        let ra = self.ra + self.pmra * dt / 3.6e6 / self.dec.to_radians().cos().max(1e-4);
        (ra.rem_euclid(360.0), dec.clamp(-90.0, 90.0))
    }

    pub fn from_catalog(s: &fulgur_catalog::xpsd::GaiaStar) -> Self {
        Self {
            source_id: 0,
            ra: s.ra,
            dec: s.dec,
            pmra: s.pmra,
            pmdec: s.pmdec,
            g: s.g,
        }
    }
}

/// Gnomonic projection about `(ra0, dec0)` in arcseconds; `xi` east, `eta` north.
pub fn project(ra: f64, dec: f64, ra0: f64, dec0: f64) -> Option<[f64; 2]> {
    let (a, d, a0, d0) = (
        ra.to_radians(),
        dec.to_radians(),
        ra0.to_radians(),
        dec0.to_radians(),
    );
    let cos_c = d0.sin() * d.sin() + d0.cos() * d.cos() * (a - a0).cos();
    if cos_c <= 0.2 {
        return None;
    }
    let xi = d.cos() * (a - a0).sin() / cos_c;
    let eta = (d0.cos() * d.sin() - d0.sin() * d.cos() * (a - a0).cos()) / cos_c;
    Some([xi * ARCSEC, eta * ARCSEC])
}

pub fn deproject(p: [f64; 2], ra0: f64, dec0: f64) -> (f64, f64) {
    let (xi, eta) = (p[0] / ARCSEC, p[1] / ARCSEC);
    let (a0, d0) = (ra0.to_radians(), dec0.to_radians());
    let rho = xi.hypot(eta);
    if rho == 0. {
        return (ra0, dec0);
    }
    let c = rho.atan();
    let dec = (c.cos() * d0.sin() + eta * c.sin() * d0.cos() / rho).asin();
    let ra = a0 + (xi * c.sin()).atan2(rho * d0.cos() * c.cos() - eta * d0.sin() * c.sin());
    (ra.to_degrees().rem_euclid(360.), dec.to_degrees())
}

/// Right ascension as hours, minutes and seconds: `01h 33m 50.9s`.
pub fn format_ra(ra_deg: f64) -> String {
    let tenths = ((ra_deg.rem_euclid(360.0) / 15.0 * 36_000.0).round() as u64) % 864_000;
    let (hours, rest) = (tenths / 36_000, tenths % 36_000);
    let (minutes, tenths) = (rest / 600, rest % 600);
    format!(
        "{hours:02}h {minutes:02}m {:02}.{}s",
        tenths / 10,
        tenths % 10
    )
}

/// Declination as degrees, arcminutes and arcseconds: `+30° 39′ 36″`.
pub fn format_dec(dec_deg: f64) -> String {
    let arcsec = (dec_deg.abs() * 3600.0).round() as u64;
    let sign = if dec_deg < 0.0 && arcsec > 0 {
        '-'
    } else {
        '+'
    };
    format!(
        "{sign}{:02}° {:02}′ {:02}″",
        arcsec / 3600,
        arcsec % 3600 / 60,
        arcsec % 60
    )
}

/// Formatted sky coordinate string with sexagesimal and decimal degrees.
pub fn position_text(ra_deg: f64, dec_deg: f64) -> String {
    format!(
        "RA {} · Dec {} ({ra_deg:.4}° · {dec_deg:+.4}°)",
        format_ra(ra_deg),
        format_dec(dec_deg)
    )
}

pub fn separation(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let (a1, d1, a2, d2) = (
        ra1.to_radians(),
        dec1.to_radians(),
        ra2.to_radians(),
        dec2.to_radians(),
    );
    let h = ((d2 - d1) / 2.).sin().powi(2) + d1.cos() * d2.cos() * ((a2 - a1) / 2.).sin().powi(2);
    (2. * h.sqrt().min(1.).asin()).to_degrees()
}
