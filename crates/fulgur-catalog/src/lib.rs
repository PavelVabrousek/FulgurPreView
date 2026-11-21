//! Star and deep-sky catalogues for FulgurPreView:
//! - `xpsd`: Local PixInsight Gaia DR3 `.xpsd` reader.
//! - `download`: Cached local Gaia DR3 tiles.
//! - `dso`: Built-in `deep_sky.csv` catalog for target lookup and field annotations.
//! - `guess`: Target recognition from file names and folder paths.
//! - `config`: Automatic discovery of `%LOCALAPPDATA%\FulgurStack` catalogue and index paths.

pub mod config;
pub mod download;
pub mod dso;
pub mod guess;
pub mod sky;
pub mod xpsd;
