//! Shared catalogue and blind-index configuration discovery.
//!
//! Automatically inspects `%LOCALAPPDATA%\FulgurStack\settings.json` so existing
//! PixInsight Gaia DR3 `.xpsd` folders, downloaded Gaia tile caches, and the
//! `fulgurstack_gaia_g13.fsidx` blind-solver quad index work out of the box
//! without re-configuration, while allowing local overrides in
//! `%LOCALAPPDATA%\FulgurPreView\settings.json`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default filename for the pre-built blind-solver quad index.
pub const DEFAULT_INDEX_FILE_NAME: &str = "fulgurstack_gaia_g13.fsidx";

// Deliberately no `Default` impl: discovering the configuration reads files from disk, and a
// `Default` that does I/O is easy to call by accident (serde, `..Default::default()`); it
// once caused unbounded recursion. Use `CatalogConfig::discover()` explicitly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogConfig {
    /// Folder holding PixInsight Gaia DR3 `.xpsd` files.
    #[serde(default)]
    pub gaia_folder: String,
    /// Folder holding cached Gaia CSV tiles (`g{limit}_d{dec}_r{ra}.csv`).
    #[serde(default)]
    pub gaia_cache_folder: String,
    /// Folder holding the blind-solving quad index (`fulgurstack_gaia_g13.fsidx`).
    #[serde(default)]
    pub solve_index_folder: String,
    /// Provenance note describing where settings were discovered.
    #[serde(skip)]
    pub source_note: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PartialStackSettings {
    gaia_folder: String,
    solve_index_folder: String,
}

impl CatalogConfig {
    /// Discover catalogue paths from `%LOCALAPPDATA%\FulgurPreView\settings.json`,
    /// falling back to `%LOCALAPPDATA%\FulgurStack\settings.json` and standard
    /// PixInsight data directories.
    pub fn discover() -> Self {
        let preview_dir = app_data_dir("FulgurPreView");
        let stack_dir = app_data_dir("FulgurStack");

        let mut cfg = Self {
            gaia_folder: String::new(),
            gaia_cache_folder: String::new(),
            solve_index_folder: String::new(),
            source_note: "Default".to_owned(),
        };

        // 1. Check FulgurPreView's own settings.json first
        if let Some(dir) = &preview_dir {
            let file = dir.join("settings.json");
            if let Ok(text) = std::fs::read_to_string(&file)
                && let Ok(saved) = serde_json::from_str::<CatalogConfig>(&text)
            {
                cfg.gaia_folder = saved.gaia_folder;
                cfg.gaia_cache_folder = saved.gaia_cache_folder;
                cfg.solve_index_folder = saved.solve_index_folder;
                cfg.source_note = "FulgurPreView settings".to_owned();
            }
        }

        // 2. Inherit missing paths from FulgurStack's settings.json
        if let Some(dir) = &stack_dir {
            let file = dir.join("settings.json");
            if let Ok(text) = std::fs::read_to_string(&file)
                && let Ok(stack) = serde_json::from_str::<PartialStackSettings>(&text)
            {
                if cfg.gaia_folder.trim().is_empty() && !stack.gaia_folder.trim().is_empty() {
                    cfg.gaia_folder = stack.gaia_folder.trim().to_owned();
                    cfg.source_note = "Shared from FulgurStack".to_owned();
                }
                if cfg.solve_index_folder.trim().is_empty()
                    && !stack.solve_index_folder.trim().is_empty()
                {
                    cfg.solve_index_folder = stack.solve_index_folder.trim().to_owned();
                }
            }
            if cfg.gaia_cache_folder.trim().is_empty() {
                let stack_cache = dir.join("gaia_cache");
                if stack_cache.is_dir() {
                    cfg.gaia_cache_folder = stack_cache.to_string_lossy().into_owned();
                }
            }
        }

        // 3. Probe common PixInsight Gaia DR3 directories if still empty
        if cfg.gaia_folder.trim().is_empty() {
            for candidate in [
                r"C:\PixInsightData\Gaia DR3",
                r"C:\PixInsight\library\Gaia DR3",
                r"D:\PixInsightData\Gaia DR3",
            ] {
                let p = Path::new(candidate);
                if has_xpsd_files(p) {
                    cfg.gaia_folder = candidate.to_owned();
                    cfg.source_note = format!("Auto-detected ({candidate})");
                    break;
                }
            }
        }

        // 4. Fallback cache directory
        if cfg.gaia_cache_folder.trim().is_empty()
            && let Some(dir) = &preview_dir
        {
            cfg.gaia_cache_folder = dir.join("gaia_cache").to_string_lossy().into_owned();
        }

        cfg
    }

    /// Save user-customized catalogue paths to `%LOCALAPPDATA%\FulgurPreView\settings.json`.
    pub fn save(&self) -> Result<(), String> {
        let Some(dir) = app_data_dir("FulgurPreView") else {
            return Err("Could not determine local application data directory.".to_owned());
        };
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join("settings.json");
        let tmp = dir.join("settings.json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &file).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Returns `true` if a local Gaia DR3 `.xpsd` folder or tile cache directory is available.
    pub fn has_local_gaia(&self) -> bool {
        self.xpsd_dir().is_some() || self.cache_dir().is_some()
    }

    /// Resolve the active Gaia `.xpsd` folder if it exists and contains `.xpsd` files.
    pub fn xpsd_dir(&self) -> Option<PathBuf> {
        let trimmed = self.gaia_folder.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = PathBuf::from(trimmed);
        has_xpsd_files(&path).then_some(path)
    }

    /// Resolve the active Gaia tile cache directory if it exists.
    pub fn cache_dir(&self) -> Option<PathBuf> {
        let trimmed = self.gaia_cache_folder.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = PathBuf::from(trimmed);
        path.is_dir().then_some(path)
    }

    /// Resolve the full path to the `fulgurstack_gaia_g13.fsidx` blind-solver index file.
    pub fn index_file_path(&self) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        let explicit = self.solve_index_folder.trim();
        if !explicit.is_empty() {
            let p = PathBuf::from(explicit);
            if p.is_file() {
                return Some(p);
            }
            candidates.push(p.join(DEFAULT_INDEX_FILE_NAME));
        }
        let gaia = self.gaia_folder.trim();
        if !gaia.is_empty() {
            candidates.push(PathBuf::from(gaia).join(DEFAULT_INDEX_FILE_NAME));
        }
        if let Some(stack_dir) = app_data_dir("FulgurStack") {
            candidates.push(stack_dir.join("solve_index").join(DEFAULT_INDEX_FILE_NAME));
        }
        if let Some(preview_dir) = app_data_dir("FulgurPreView") {
            candidates.push(
                preview_dir
                    .join("solve_index")
                    .join(DEFAULT_INDEX_FILE_NAME),
            );
        }
        candidates.into_iter().find(|p| p.is_file())
    }
}

fn has_xpsd_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("xpsd"))
        })
}

fn app_data_dir(app_name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(|base| PathBuf::from(base).join(app_name))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(app_name)
        })
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .map(|base| base.join(app_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_with_missing_fields_parse_without_running_discovery() {
        // Regression: a container-level `#[serde(default)]` made this parse call
        // `discover()`, which re-parsed `settings.json` and overflowed the stack.
        let cfg: CatalogConfig = serde_json::from_str(r#"{"gaia_folder":"G"}"#).unwrap();
        assert_eq!(cfg.gaia_folder, "G");
        assert!(cfg.gaia_cache_folder.is_empty());
        assert!(cfg.solve_index_folder.is_empty());
        assert!(cfg.source_note.is_empty());

        let empty: CatalogConfig = serde_json::from_str("{}").unwrap();
        assert!(empty.gaia_folder.is_empty());
    }
}
