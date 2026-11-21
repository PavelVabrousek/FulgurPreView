//! Built-in deep-sky table (Messier, NGC, IC, Sh2, LBN, Barnard and aliases from OpenNGC,
//! CC BY-SA 4.0).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::sky::separation_deg;

const TABLE: &str = include_str!("../data/deep_sky.csv");

#[derive(Clone, Debug, PartialEq)]
pub struct DeepSky {
    /// Primary catalogue name, e.g. `"NGC 598"`.
    pub name: String,
    pub ra: f64,
    pub dec: f64,
    /// Optional Messier or common alias (e.g. `"M 33"`).
    pub alias: Option<String>,
}

impl DeepSky {
    /// Human-friendly label combining Messier/common alias and primary name (e.g. `"M 33 (NGC 598)"`).
    pub fn display_name(&self) -> String {
        match &self.alias {
            Some(a) if !a.eq_ignore_ascii_case(&self.name) => format!("{a} ({})", self.name),
            _ => self.name.clone(),
        }
    }
}

/// Comparable form of a name: letters and digits only, upper case, leading
/// zeros of numbers dropped, catalogue words shortened ("Messier 33",
/// "M033" and "M 33" all become "M33").
pub fn normalize(name: &str) -> String {
    let upper = name.to_uppercase();
    let words = [
        ("MESSIER", "M"),
        ("CALDWELL", "C"),
        ("SHARPLESS", "SH2"),
        ("BARNARD", "B"),
    ];
    let mut text = upper.clone();
    for (long, short) in words {
        if let Some(rest) = text.strip_prefix(long) {
            text = format!("{short}{rest}");
        }
    }
    let compact: String = text.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let (prefix, body) = match compact.strip_prefix("SH2") {
        Some(rest) => ("SH2", rest.to_string()),
        None => ("", compact),
    };
    let mut out = String::from(prefix);
    let mut previous_letter = true;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '0' && previous_letter && chars.peek().is_some_and(|n| n.is_ascii_digit()) {
            continue;
        }
        previous_letter = c.is_ascii_alphabetic();
        out.push(c);
    }
    out
}

struct ParsedDsoCatalog {
    by_name: HashMap<String, DeepSky>,
    all: Vec<DeepSky>,
}

fn catalog() -> &'static ParsedDsoCatalog {
    static CAT: OnceLock<ParsedDsoCatalog> = OnceLock::new();
    CAT.get_or_init(|| {
        let mut by_name = HashMap::new();
        let mut all = Vec::with_capacity(16000);
        for line in TABLE.lines().filter(|l| !l.starts_with('#')) {
            let f: Vec<&str> = line.split(';').collect();
            if f.len() < 4 {
                continue;
            }
            let (Ok(ra), Ok(dec)) = (f[1].parse::<f64>(), f[2].parse::<f64>()) else {
                continue;
            };
            let aliases: Vec<&str> = f[3].split('|').filter(|a| !a.is_empty()).collect();
            let preferred_alias = aliases
                .iter()
                .find(|a| a.starts_with("M ") || a.starts_with('M'))
                .or_else(|| aliases.first())
                .map(|s| (*s).to_owned());
            let object = DeepSky {
                name: f[0].to_string(),
                ra,
                dec,
                alias: preferred_alias,
            };
            by_name.insert(normalize(f[0]), object.clone());
            for alias in aliases {
                by_name
                    .entry(normalize(alias))
                    .or_insert_with(|| object.clone());
            }
            all.push(object);
        }
        ParsedDsoCatalog { by_name, all }
    })
}

/// The object a name denotes, if the table knows it.
pub fn find(name: &str) -> Option<DeepSky> {
    catalog().by_name.get(&normalize(name)).cloned()
}

/// Number of names (catalogue names and aliases) the table answers.
pub fn names() -> usize {
    catalog().by_name.len()
}

/// Find all deep-sky objects within `radius_deg` of `(ra_deg, dec_deg)`, sorted by angular distance.
pub fn in_cone(ra_deg: f64, dec_deg: f64, radius_deg: f64) -> Vec<DeepSky> {
    let dec_min = (dec_deg - radius_deg).max(-90.0);
    let dec_max = (dec_deg + radius_deg).min(90.0);
    let mut hits: Vec<(f64, DeepSky)> = catalog()
        .all
        .iter()
        .filter(|o| o.dec >= dec_min && o.dec <= dec_max)
        .filter_map(|o| {
            let dist = separation_deg(ra_deg, dec_deg, o.ra, o.dec);
            (dist <= radius_deg).then(|| (dist, o.clone()))
        })
        .collect();
    hits.sort_by(|a, b| a.0.total_cmp(&b.0));
    hits.into_iter().map(|(_, o)| o).collect()
}
