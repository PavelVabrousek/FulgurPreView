//! Identification of asteroids and comets near a sky position, using the NASA/JPL
//! Small-Body Identification API (`https://ssd-api.jpl.nasa.gov/sb_ident.api`), which works
//! from the Minor Planet Center orbit catalogue.
//!
//! This is the only part of the viewer that talks to the internet, and only when the user
//! asks for it. The request carries the field centre, the observation time and the observer
//! location; header GPS coordinates are rounded to 0.1° (~10 km) before they are sent.

use fulgur_core::{Error, HeaderCard, Result};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// JPL SB Ident endpoint.
pub const JPL_SB_IDENT_URL: &str = "https://ssd-api.jpl.nasa.gov/sb_ident.api";
/// Search radius around the cursor (owner decision: 25′).
pub const SEARCH_RADIUS_ARCMIN: f64 = 25.0;
/// Faintest objects reported (owner decision: V ≤ 20).
pub const VMAG_LIMIT: f64 = 20.0;
/// Fallback observer: Prague, Czech Republic.
pub const PRAGUE: ObserverLocation = ObserverLocation {
    lat_deg: 50.0755,
    lon_deg: 14.4378,
    alt_km: 0.2,
    source: LocationSource::DefaultPrague,
};
/// Header coordinates are rounded to this step before they leave the computer (privacy).
const LOCATION_STEP_DEG: f64 = 0.1;
/// Network timeout for one query.
const TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LocationSource {
    /// From `SITELAT` / `SITELONG` (rounded to 0.1°).
    Header,
    /// No usable header location: Prague.
    DefaultPrague,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObserverLocation {
    pub lat_deg: f64,
    /// East positive.
    pub lon_deg: f64,
    pub alt_km: f64,
    pub source: LocationSource,
}

/// One identified asteroid or comet.
#[derive(Clone, Debug, PartialEq)]
pub struct SmallBody {
    /// Designation as given by JPL, e.g. `93987 (2000 XB16)` or `C/2023 A3 (Tsuchinshan-ATLAS)`.
    pub name: String,
    pub is_comet: bool,
    pub ra_deg: f64,
    pub dec_deg: f64,
    /// Distance from the search centre, arcseconds.
    pub distance_arcsec: f64,
    pub vmag: Option<f64>,
    /// Apparent motion, arcseconds per hour (RA rate includes cos(Dec) as JPL reports it).
    pub ra_rate_arcsec_h: f64,
    pub dec_rate_arcsec_h: f64,
}

/// Everything needed for one search.
#[derive(Clone, Debug, PartialEq)]
pub struct SmallBodyQuery {
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub radius_arcmin: f64,
    pub vmag_limit: f64,
    /// UTC, `YYYY-MM-DD_hh:mm:ss` (mid-exposure).
    pub obs_time_utc: String,
    pub observer: ObserverLocation,
}

impl SmallBodyQuery {
    /// Build the query for a sky position from the frame's header cards. The time is the
    /// middle of the exposure (`DATE-OBS` + `EXPTIME` / 2); the location comes from
    /// `SITELAT` / `SITELONG` (`SITEELEV`), else Prague.
    pub fn from_header(ra_deg: f64, dec_deg: f64, cards: &[HeaderCard]) -> Result<Self> {
        let date_obs = card(cards, "DATE-OBS").ok_or(Error::Invalid(
            "The header has no DATE-OBS, so the observation time is unknown.",
        ))?;
        let exposure = card(cards, "EXPTIME")
            .or_else(|| card(cards, "EXPOSURE"))
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(0.0);
        let obs_time_utc = mid_exposure_utc(&date_obs, exposure).ok_or(Error::Invalid(
            "DATE-OBS is not a valid date and time (expected YYYY-MM-DDThh:mm:ss).",
        ))?;
        Ok(Self {
            ra_deg,
            dec_deg,
            radius_arcmin: SEARCH_RADIUS_ARCMIN,
            vmag_limit: VMAG_LIMIT,
            obs_time_utc,
            observer: observer_from_header(cards),
        })
    }

    /// Request URL. The API searches a box; the box half-width equals the search radius and
    /// results outside the circle are dropped when the response is read.
    pub fn url(&self) -> String {
        let half_deg = self.radius_arcmin / 60.0;
        format!(
            "{JPL_SB_IDENT_URL}?lat={:.4}&lon={:.4}&alt={:.3}&obs-time={}&mag-required=true\
             &two-pass=true&suppress-first-pass=true&vmag-lim={}&fov-ra-center={}\
             &fov-dec-center={}&fov-ra-hwidth={half_deg:.5}&fov-dec-hwidth={half_deg:.5}",
            self.observer.lat_deg,
            self.observer.lon_deg,
            self.observer.alt_km,
            self.obs_time_utc,
            self.vmag_limit,
            sexagesimal(self.ra_deg.rem_euclid(360.0) / 15.0, false),
            sexagesimal(self.dec_deg, true),
        )
    }
}

/// Run the query (blocking; call from a background job).
pub fn query_small_bodies(query: &SmallBodyQuery, cancel: &AtomicBool) -> Result<Vec<SmallBody>> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(TIMEOUT)
        .user_agent(concat!("FulgurPreView/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent.get(&query.url()).call().map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            Error::Message(format!(
                "JPL small-body service answered HTTP {code}: {}",
                body.chars().take(200).collect::<String>()
            ))
        }
        ureq::Error::Transport(t) => Error::Message(format!(
            "Could not reach the JPL small-body service (internet connection?): {t}"
        )),
    })?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let body = response
        .into_string()
        .map_err(|e| Error::Message(format!("Reading the JPL response failed: {e}")))?;
    parse_jpl_response(&body, query.radius_arcmin * 60.0)
}

#[derive(Deserialize)]
struct JplResponse {
    signature: Option<JplSignature>,
    #[serde(default)]
    data_second_pass: Vec<Vec<String>>,
    #[serde(default)]
    data_first_pass: Vec<Vec<String>>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct JplSignature {
    version: String,
}

/// Parse an SB Ident JSON response and keep objects within `radius_arcsec` of the centre,
/// nearest first.
pub fn parse_jpl_response(body: &str, radius_arcsec: f64) -> Result<Vec<SmallBody>> {
    let parsed: JplResponse = serde_json::from_str(body)
        .map_err(|e| Error::Message(format!("Unexpected JPL response: {e}")))?;
    if let Some(sig) = &parsed.signature
        && !sig.version.starts_with('1')
    {
        return Err(Error::Message(format!(
            "Unsupported JPL API version {}.",
            sig.version
        )));
    }
    if let Some(message) = parsed.message {
        return Err(Error::Message(format!("JPL: {message}")));
    }
    let rows = if parsed.data_second_pass.is_empty() {
        parsed.data_first_pass
    } else {
        parsed.data_second_pass
    };
    let mut bodies: Vec<SmallBody> = rows.iter().filter_map(|row| parse_row(row)).collect();
    bodies.retain(|b| b.distance_arcsec <= radius_arcsec);
    bodies.sort_by(|a, b| a.distance_arcsec.total_cmp(&b.distance_arcsec));
    Ok(bodies)
}

/// Row: name, RA hh:mm:ss, Dec dd mm'ss", dRA", dDec", dist", V, RA rate "/h, Dec rate "/h.
fn parse_row(row: &[String]) -> Option<SmallBody> {
    if row.len() < 9 {
        return None;
    }
    let num = |s: &str| s.trim().parse::<f64>().ok();
    let name = row[0].trim().to_owned();
    Some(SmallBody {
        is_comet: is_comet_designation(&name),
        ra_deg: parse_sexagesimal(&row[1])? * 15.0,
        dec_deg: parse_sexagesimal(&row[2])?,
        distance_arcsec: num(&row[5])?,
        vmag: num(&row[6]),
        ra_rate_arcsec_h: num(&row[7]).unwrap_or(0.0),
        dec_rate_arcsec_h: num(&row[8]).unwrap_or(0.0),
        name,
    })
}

/// Comet designations: `C/`, `P/`, `D/`, `X/`, `I/` or numbered periodic `123P/`.
pub fn is_comet_designation(name: &str) -> bool {
    let n = name.trim();
    let digits = n.trim_start_matches(|c: char| c.is_ascii_digit());
    ["C/", "P/", "D/", "X/", "I/"]
        .iter()
        .any(|p| n.starts_with(p) || digits.starts_with(p))
}

/// Parse `hh:mm:ss.s`, `+dd mm'ss.s"` or decimal into a number (hours or degrees).
pub fn parse_sexagesimal(text: &str) -> Option<f64> {
    let t = text.trim();
    let negative = t.starts_with('-');
    let parts: Vec<f64> = t
        .trim_start_matches(['+', '-'])
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f64>())
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let value =
        parts[0] + parts.get(1).unwrap_or(&0.0) / 60.0 + parts.get(2).unwrap_or(&0.0) / 3600.0;
    Some(if negative { -value } else { value })
}

/// `hh-mm-ss.ss` / `dd-mm-ss.s` with the API's `M` prefix for negative values.
fn sexagesimal(value: f64, signed: bool) -> String {
    let negative = value < 0.0;
    let total = (value.abs() * 3600.0 * 100.0).round() / 100.0;
    let d = (total / 3600.0).floor();
    let m = ((total - d * 3600.0) / 60.0).floor();
    let s = total - d * 3600.0 - m * 60.0;
    let sign = if signed && negative { "M" } else { "" };
    format!("{sign}{:02}-{:02}-{:05.2}", d as i64, m as i64, s)
}

fn card(cards: &[HeaderCard], key: &str) -> Option<String> {
    cards
        .iter()
        .find(|c| c.keyword.trim().eq_ignore_ascii_case(key))
        .map(|c| c.value.trim().trim_matches('\'').trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Observer location from `SITELAT` / `SITELONG` / `SITEELEV` (decimal or sexagesimal),
/// rounded to 0.1°; Prague when missing or invalid.
pub fn observer_from_header(cards: &[HeaderCard]) -> ObserverLocation {
    let lat = card(cards, "SITELAT").and_then(|v| parse_sexagesimal(&v));
    let lon = card(cards, "SITELONG").and_then(|v| parse_sexagesimal(&v));
    match (lat, lon) {
        (Some(lat), Some(lon))
            if (-90.0..=90.0).contains(&lat)
                && (-180.0..=360.0).contains(&lon)
                && !(lat == 0.0 && lon == 0.0) =>
        {
            let round = |v: f64| (v / LOCATION_STEP_DEG).round() * LOCATION_STEP_DEG;
            let lon = if lon > 180.0 { lon - 360.0 } else { lon };
            let alt_km = card(cards, "SITEELEV")
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| (-500.0..=9000.0).contains(v))
                .map_or(0.0, |m| (m / 100.0).round() / 10.0);
            ObserverLocation {
                lat_deg: round(lat),
                lon_deg: round(lon),
                alt_km,
                source: LocationSource::Header,
            }
        }
        _ => PRAGUE,
    }
}

/// `DATE-OBS` (UTC, `YYYY-MM-DD[Thh:mm:ss[.sss]]`) plus half the exposure, formatted as
/// `YYYY-MM-DD_hh:mm:ss`.
pub fn mid_exposure_utc(date_obs: &str, exposure_s: f64) -> Option<String> {
    let t = date_obs.trim().trim_end_matches('Z');
    let (date, time) = t.split_once(['T', ' ']).unwrap_or((t, "00:00:00"));
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let mut tp = time.split(':');
    let h = tp.next()?.parse::<f64>().ok()?;
    let mi = tp.next().map_or(Some(0.0), |v| v.parse::<f64>().ok())?;
    let s = tp.next().map_or(Some(0.0), |v| v.parse::<f64>().ok())?;
    let seconds = (days_from_civil(y, mo, da) as f64) * 86_400.0
        + h * 3600.0
        + mi * 60.0
        + s
        + exposure_s / 2.0;
    let total = seconds.round() as i64;
    let (days, rem) = (total.div_euclid(86_400), total.rem_euclid(86_400));
    let (y, mo, da) = civil_from_days(days);
    Some(format!(
        "{y:04}-{mo:02}-{da:02}_{:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    ))
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hc(k: &str, v: &str) -> HeaderCard {
        HeaderCard {
            keyword: k.to_owned(),
            value: v.to_owned(),
            comment: String::new(),
        }
    }

    #[test]
    fn parses_recorded_jpl_response_for_m33_seestar_sub() {
        let body = include_str!("../../tests/fixtures/jpl_sb_ident_m33.json");
        let bodies = parse_jpl_response(body, 25.0 * 60.0).unwrap();
        assert_eq!(bodies.len(), 3);
        let first = &bodies[0];
        assert_eq!(first.name, "93987 (2000 XB16)");
        assert!(!first.is_comet);
        assert!((first.ra_deg - (1.0 + 34.0 / 60.0 + 31.18 / 3600.0) * 15.0).abs() < 1e-9);
        assert!((first.dec_deg - (30.0 + 44.0 / 60.0 + 42.2 / 3600.0)).abs() < 1e-9);
        assert_eq!(first.vmag, Some(18.8));
        assert!((first.ra_rate_arcsec_h + 30.62).abs() < 1e-9);
        assert!((first.distance_arcsec - 414.4).abs() < 1e-9);
        // Sorted by distance; a tighter radius drops the far one.
        assert!(
            bodies
                .windows(2)
                .all(|w| w[0].distance_arcsec <= w[1].distance_arcsec)
        );
        assert_eq!(parse_jpl_response(body, 600.0).unwrap().len(), 2);
    }

    #[test]
    fn query_uses_mid_exposure_time_and_rounded_header_location() {
        let cards = [
            hc("DATE-OBS", "'2026-10-06T18:45:12.145756'"),
            hc("EXPTIME", "10."),
            hc("SITELAT", "49.1951"),
            hc("SITELONG", "16.6068"),
        ];
        let q = SmallBodyQuery::from_header(23.5214, 30.6784, &cards).unwrap();
        assert_eq!(q.obs_time_utc, "2026-10-06_18:45:17");
        assert_eq!(q.observer.source, LocationSource::Header);
        assert!((q.observer.lat_deg - 49.2).abs() < 1e-9);
        assert!((q.observer.lon_deg - 16.6).abs() < 1e-9);
        let url = q.url();
        assert!(url.contains("lat=49.2000&lon=16.6000"), "{url}");
        assert!(url.contains("obs-time=2026-10-06_18:45:17"));
        assert!(url.contains("vmag-lim=20"));
        assert!(url.contains("fov-ra-center=01-34-05.14"), "{url}");
        assert!(url.contains("fov-dec-center=30-40-42.24"), "{url}");
        assert!(url.contains("fov-ra-hwidth=0.41667"));
        assert!(!url.contains("49.1951"), "exact GPS must not be sent");
    }

    #[test]
    fn missing_location_falls_back_to_prague_and_negative_dec_uses_m() {
        let cards = [hc("DATE-OBS", "2026-01-31T23:59:58"), hc("EXPTIME", "4")];
        let q = SmallBodyQuery::from_header(10.0, -5.5, &cards).unwrap();
        assert_eq!(q.observer, PRAGUE);
        assert_eq!(q.obs_time_utc, "2026-02-01_00:00:00");
        assert!(
            q.url().contains("fov-dec-center=M05-30-00.00"),
            "{}",
            q.url()
        );
        assert!(SmallBodyQuery::from_header(10.0, 5.0, &[]).is_err());
    }

    /// Live query against the JPL service (needs internet): `cargo test -- --ignored live_jpl`.
    #[test]
    #[ignore]
    fn live_jpl_query_finds_known_asteroids_near_m33() {
        let cards = [
            hc("DATE-OBS", "2026-10-06T18:45:12"),
            hc("EXPTIME", "10"),
            hc("SITELAT", "49.1951"),
            hc("SITELONG", "16.6068"),
        ];
        let q = SmallBodyQuery::from_header(23.5214, 30.6784, &cards).unwrap();
        let bodies = query_small_bodies(&q, &AtomicBool::new(false)).unwrap();
        assert!(
            bodies.iter().any(|b| b.name.starts_with("93987")),
            "{bodies:?}"
        );
    }

    #[test]
    fn helpers() {
        assert!(is_comet_designation("C/2023 A3 (Tsuchinshan-ATLAS)"));
        assert!(is_comet_designation("12P/Pons-Brooks"));
        assert!(!is_comet_designation("93987 (2000 XB16)"));
        assert_eq!(parse_sexagesimal("-05 30'00.0\""), Some(-5.5));
        assert_eq!(parse_sexagesimal("+50 12 00"), Some(50.2));
        assert_eq!(parse_sexagesimal("16.6068"), Some(16.6068));
        assert_eq!(
            mid_exposure_utc("2024-02-28T23:59:59", 2.0).unwrap(),
            "2024-02-29_00:00:00"
        );
        let sexa = observer_from_header(&[hc("SITELAT", "+50 02 00"), hc("SITELONG", "14 26 00")]);
        assert!((sexa.lat_deg - 50.0).abs() < 1e-9 && (sexa.lon_deg - 14.4).abs() < 1e-9);
    }
}
