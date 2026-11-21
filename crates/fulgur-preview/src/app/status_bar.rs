//! Bottom status bar, most important first:
//! 1. frame quality (stars, FWHM, HFR, eccentricity, sky background and noise),
//! 2. live readout under the cursor (X/Y, ADU, RA/Dec, hovered star),
//! 3. static facts about the file (name, format, size, BITPIX, Bayer, RAW stage).
//!
//! Load timings and cache usage are only in the tooltip of the file block.

use super::*;
use fulgur_engine::levels::ChannelLevel;

const QUALITY_COLOR: Color32 = Color32::from_rgb(135, 225, 180);
const CURSOR_COLOR: Color32 = Color32::from_rgb(240, 220, 140);
const SKY_COLOR: Color32 = Color32::from_rgb(130, 225, 255);
const STAR_COLOR: Color32 = Color32::from_rgb(255, 210, 100);

/// The channel that best represents the sky background: luminance, mono, else green.
pub(super) fn background_level(levels: &[ChannelLevel]) -> Option<&ChannelLevel> {
    ["L", "Mono", "G"]
        .iter()
        .find_map(|name| levels.iter().find(|l| l.name == *name))
}

impl FulgurPreViewApp {
    pub(super) fn show_status_bar(
        &self,
        ui: &mut egui::Ui,
        active_frame: Option<&Arc<OverviewFrame>>,
        star_report: Option<&Arc<StarQualityReport>>,
    ) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let Some(frame) = active_frame else {
                ui.label(RichText::new("No file opened").weak());
                return;
            };

            self.status_quality_block(ui, star_report);
            ui.separator();
            self.status_cursor_block(ui, frame);
            ui.separator();
            self.status_file_block(ui, frame);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((msg, when)) = &self.status_banner
                    && when.elapsed() < Duration::from_secs(6)
                {
                    ui.label(RichText::new(msg).color(Color32::from_rgb(140, 230, 160)));
                }
                if self.viewport.is_loading_detail() {
                    ui.label(
                        RichText::new("Streaming 1:1 VNG ROI...")
                            .small()
                            .color(Color32::from_rgb(120, 200, 255)),
                    );
                }
            });
        });
    }

    /// Stars, FWHM, HFR, eccentricity and sky background ± noise.
    fn status_quality_block(
        &self,
        ui: &mut egui::Ui,
        star_report: Option<&Arc<StarQualityReport>>,
    ) {
        let Some(rep) = star_report else {
            let text = if self.active_star_error.is_some() {
                "★ —"
            } else {
                "★ analysing…"
            };
            let response = ui.label(RichText::new(text).monospace().weak());
            if let Some(err) = &self.active_star_error {
                response.on_hover_text(err.as_str());
            }
            return;
        };
        if let Some(reason) = rep.metrics_unavailable {
            // ADR-022: star metrics are withheld for 8-bit processed exports.
            let mut text = "★ — (needs linear data)".to_owned();
            if let Some(bg) = background_level(&rep.levels) {
                text.push_str(&format!(
                    " · Bkg {:.0} ±{:.1} ADU",
                    bg.median_adu, bg.noise_adu
                ));
            }
            ui.label(RichText::new(text).monospace().weak())
                .on_hover_text(reason);
            return;
        }
        let opt_len = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |v| self.fmt_len(v));
        let mut text = format!(
            "★ {} · FWHM {} · HFR {} · Ecc {}",
            rep.star_count,
            opt_len(rep.median_fwhm_px),
            opt_len(rep.median_hfr_px),
            rep.median_eccentricity
                .map_or_else(|| "—".to_owned(), |e| format!("{e:.2}")),
        );
        if let Some(bg) = background_level(&rep.levels) {
            text.push_str(&format!(
                " · Bkg {:.0} ±{:.1} ADU",
                bg.median_adu, bg.noise_adu
            ));
        }
        ui.label(RichText::new(text).monospace().color(QUALITY_COLOR))
            .on_hover_text(
                "Frame quality: detected stars, median FWHM / HFR / eccentricity, and the sky \
                 background (median) ± pixel noise of the luminance / green channel. \
                 Per-channel values are in the Histogram panel.",
            );
    }

    /// Live readout under the cursor; keeps its place (placeholder) when the cursor is away.
    fn status_cursor_block(&self, ui: &mut egui::Ui, frame: &OverviewFrame) {
        let Some(cursor) = &self.viewport.cursor_readout else {
            ui.label(RichText::new("X —  Y —").monospace().weak());
            return;
        };
        let [r, g, b] = cursor.adu;
        let adu_str = if frame.norm_span <= 1.05 {
            format!(
                "X {:4} Y {:4} · RGB [{:.4}, {:.4}, {:.4}]",
                cursor.x, cursor.y, r, g, b
            )
        } else if frame.info.planes >= 3 || frame.info.bayer_pattern.is_some() {
            format!(
                "X {:4} Y {:4} · ADU R={:5.0} G={:5.0} B={:5.0}",
                cursor.x, cursor.y, r, g, b
            )
        } else {
            format!("X {:4} Y {:4} · ADU {:5.0}", cursor.x, cursor.y, g)
        };
        ui.label(RichText::new(adu_str).monospace().color(CURSOR_COLOR));
        if let Some((ra, dec)) = cursor.sky_ra_dec {
            ui.label(
                RichText::new(format!("RA {} · Dec {}", format_ra(ra), format_dec(dec)))
                    .monospace()
                    .color(SKY_COLOR),
            );
        }
        if let Some(star) = &cursor.hovered_star {
            ui.label(
                RichText::new(format!(
                    "★ FWHM {} · HFR {} · Ecc {:.2}",
                    self.fmt_len(star.moment_fwhm_px),
                    self.fmt_len(star.hfr_px),
                    star.eccentricity
                ))
                .monospace()
                .color(STAR_COLOR),
            );
        }
    }

    /// File name, format, size, BITPIX, Bayer pattern and RAW decoding stage.
    fn status_file_block(&self, ui: &mut egui::Ui, frame: &OverviewFrame) {
        let info = &frame.info;
        let name = self
            .playlist
            .current_path()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut text = format!(
            "{name} · {} {}×{} · BITPIX {}",
            frame.format_label, info.width, info.height, info.bitpix
        );
        if let Some(pat) = info.bayer_pattern {
            text.push_str(&format!(" · {}", pat.name()));
        }
        let timing = match self.last_switch_latency_ms {
            Some(ms) => format!("Display {ms:.1} ms, decode {:.1} ms", frame.decode_ms),
            None => format!("Decode {:.1} ms", frame.decode_ms),
        };
        ui.label(RichText::new(text).monospace().weak())
            .on_hover_text(format!(
                "{timing}\nCache: {} frames ({:.0} MB)",
                self.cache.cached_count(),
                self.cache.used_megabytes()
            ));

        match frame.raw_stage {
            RawStage::NotRaw => {}
            RawStage::Stage1Preview => {
                if let Some(err) = &self.active_stage2_error {
                    ui.label(
                        RichText::new("RAW preview only (linear decode failed)")
                            .monospace()
                            .color(Color32::from_rgb(255, 120, 110)),
                    )
                    .on_hover_text(err.as_str());
                } else {
                    ui.label(
                        RichText::new("RAW preview (decoding linear…)")
                            .monospace()
                            .color(Color32::from_rgb(255, 200, 95)),
                    );
                }
            }
            RawStage::Stage2PreviewOnly => {
                ui.label(
                    RichText::new("RAW: embedded JPEG only (not linear)")
                        .monospace()
                        .color(Color32::from_rgb(255, 200, 95)),
                )
                .on_hover_text(
                    "The sensor data of this RAW format is not decoded yet. The image is the \
                     camera's embedded JPEG converted back to linear; star metrics are disabled \
                     because they would be misleading.",
                );
            }
            RawStage::Stage2Linear => {
                ui.label(
                    RichText::new("RAW linear")
                        .monospace()
                        .color(Color32::from_rgb(110, 225, 255)),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(name: &'static str, median: f64) -> ChannelLevel {
        ChannelLevel {
            name,
            median_adu: median,
            noise_adu: 1.0,
        }
    }

    #[test]
    fn background_prefers_luminance_then_mono_then_green() {
        let rgb = [
            level("R", 1.0),
            level("G", 2.0),
            level("B", 3.0),
            level("L", 4.0),
        ];
        assert_eq!(background_level(&rgb).unwrap().name, "L");
        let cfa = [level("R", 1.0), level("G", 2.0), level("B", 3.0)];
        assert_eq!(background_level(&cfa).unwrap().name, "G");
        assert_eq!(
            background_level(&[level("Mono", 5.0)]).unwrap().name,
            "Mono"
        );
        assert!(background_level(&[]).is_none());
    }
}
