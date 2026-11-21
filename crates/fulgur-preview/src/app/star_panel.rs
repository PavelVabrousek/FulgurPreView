//! Star Quality panel: frame metrics, 3x3 tilt map, units and field maps.

use super::*;

impl FulgurPreViewApp {
    pub(super) fn show_star_quality_panel(
        &mut self,
        ui: &mut egui::Ui,
        star_report: Option<&Arc<StarQualityReport>>,
        is_computing: bool,
    ) {
        let panel_w = ui.available_width().max(120.0);
        ui.set_max_width(panel_w);
        let font_size = (panel_w * 0.048).clamp(9.5, 12.0);
        let heading_size = (panel_w * 0.060).clamp(11.5, 15.0);

        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.label(
                RichText::new("Star Quality & 3×3 Tilt")
                    .size(heading_size)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(
                    &mut self.show_star_markers,
                    RichText::new("Mark Stars").size(font_size),
                )
                .on_hover_text("Overlay detected star apertures on the image canvas");
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.label(RichText::new("Units:").size(font_size).weak());
            if ui
                .selectable_label(!self.lengths_in_arcsec, RichText::new("px").size(font_size))
                .on_hover_text("Show FWHM, HFR and tilt in pixels")
                .clicked()
            {
                self.lengths_in_arcsec = false;
            }
            let scale_known = self.active_arcsec_per_px.is_some();
            let arcsec = ui
                .add_enabled(
                    scale_known,
                    egui::SelectableLabel::new(
                        self.lengths_in_arcsec,
                        RichText::new("arcsec \u{2033}").size(font_size),
                    ),
                )
                .on_hover_text(match self.active_arcsec_per_px {
                    Some(s) => format!("Show FWHM, HFR and tilt in arcseconds ({s:.3}\u{2033}/px)"),
                    None => "Pixel scale unknown: plate solve the frame (S) or add FOCALLEN / XPIXSZ to the header".to_owned(),
                })
                .on_disabled_hover_text(
                    "Pixel scale unknown: plate solve the frame (S) or add FOCALLEN / XPIXSZ to the header",
                );
            if arcsec.clicked() {
                self.lengths_in_arcsec = true;
            }
            if self.lengths_in_arcsec && !scale_known {
                ui.label(
                    RichText::new("(scale unknown, showing px)")
                        .size(font_size - 0.5)
                        .weak(),
                );
            }
        });

        let Some(report) = star_report else {
            if is_computing {
                ui.label(
                    RichText::new("Analyzing stars & 3×3 tilt in background...")
                        .size(font_size)
                        .color(Color32::from_rgb(150, 200, 255)),
                );
            } else if let Some(err) = &self.active_star_error {
                ui.label(
                    RichText::new(err.as_str())
                        .size(font_size)
                        .color(Color32::from_rgb(255, 200, 95)),
                );
            } else {
                ui.label(
                    RichText::new("No star photometry available for this frame.")
                        .size(font_size)
                        .weak(),
                );
            }
            return;
        };
        if let Some(reason) = report.metrics_unavailable {
            // ADR-022: no FWHM / HFR / tilt for 8-bit processed exports.
            ui.label(
                RichText::new(reason)
                    .size(font_size)
                    .color(Color32::from_rgb(255, 200, 95)),
            );
            return;
        }

        // Global Frame Star Metrics Card
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(13, 40, 40))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(38, 84, 84)))
            .show(ui, |ui| {
                let inner_w = (panel_w - 20.0).max(100.0);
                ui.set_max_width(inner_w);

                let fwhm_str = report
                    .median_fwhm_px
                    .map_or_else(|| "—".to_owned(), |v| self.fmt_len(v));
                let hfr_str = report
                    .median_hfr_px
                    .map_or_else(|| "—".to_owned(), |v| self.fmt_len(v));
                let ecc_str = report
                    .median_eccentricity
                    .map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"));

                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(
                        RichText::new(format!("FWHM: {fwhm_str}"))
                            .size(font_size)
                            .monospace()
                            .strong()
                            .color(Color32::from_rgb(130, 230, 175)),
                    );
                    ui.label(
                        RichText::new(format!("HFR: {hfr_str}"))
                            .size(font_size)
                            .monospace()
                            .color(Color32::from_rgb(175, 215, 255)),
                    );
                    ui.label(
                        RichText::new(format!("Ecc: {ecc_str}"))
                            .size(font_size)
                            .monospace()
                            .color(Color32::from_rgb(240, 215, 130)),
                    );
                });

                let stars_line = format!(
                    "Stars: {} ({} measured · {} hot px suppressed)",
                    report.star_count, report.measured_star_count, report.suppressed_hot_pixels
                );
                add_truncated_label(
                    ui,
                    RichText::new(&stars_line).size(font_size - 0.5).monospace(),
                    inner_w,
                    &stars_line,
                );

                let sky_line = if let Some([r, g, b]) = report.channel_background_adu {
                    format!(
                        "Sky R/G/B: {:.0}/{:.0}/{:.0} ADU · Noise: {:.1} ADU",
                        r, g, b, report.noise_adu
                    )
                } else {
                    format!(
                        "Sky BG: {:.0} ADU · Noise: {:.1} ADU ({:.0} ms)",
                        report.background_adu, report.noise_adu, report.elapsed_ms
                    )
                };
                add_truncated_label(
                    ui,
                    RichText::new(&sky_line)
                        .size(font_size - 0.5)
                        .monospace()
                        .weak(),
                    inner_w,
                    &sky_line,
                );
            });

        ui.add_space(3.0);

        // 3x3 Sensor Tilt & Corner Map Controls
        ui.horizontal(|ui| {
            ui.set_max_width(panel_w);
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.label(RichText::new("3×3 Map:").size(font_size).weak());
            for (metric, label) in [
                (TiltMetric::Fwhm, "FWHM"),
                (TiltMetric::Hfr, "HFR"),
                (TiltMetric::Eccentricity, "Ecc"),
            ] {
                if ui
                    .selectable_label(
                        self.tilt_metric == metric,
                        RichText::new(label).size(font_size),
                    )
                    .clicked()
                {
                    self.tilt_metric = metric;
                }
            }
        });

        // Visual 3x3 Grid
        let grid_w = (panel_w - 6.0).max(114.0);
        let gap = 3.0_f32;
        let cell_w = ((grid_w - gap * 2.0) / 3.0).floor().max(34.0);
        let cell_h = (font_size * 3.35).round().clamp(34.0, 46.0);
        let total_h = cell_h * 3.0 + gap * 2.0;

        let (grid_rect, _) = ui.allocate_exact_size(
            egui::vec2(cell_w * 3.0 + gap * 2.0, total_h),
            egui::Sense::hover(),
        );
        let painter = ui.painter();

        for (idx, cell) in report.tilt_map.cells.iter().enumerate() {
            let gx = (idx % 3) as f32;
            let gy = (idx / 3) as f32;
            let x0 = grid_rect.left() + gx * (cell_w + gap);
            let y0 = grid_rect.top() + gy * (cell_h + gap);
            let rect = egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(cell_w, cell_h));

            let (val_opt, delta_opt) = match self.tilt_metric {
                TiltMetric::Fwhm => (cell.median_fwhm_px, cell.delta_fwhm_pct),
                TiltMetric::Hfr => (cell.median_hfr_px, cell.delta_hfr_pct),
                TiltMetric::Eccentricity => (cell.median_eccentricity, None),
            };

            // Color-code cell background & border by deviation from Center (or Eccentricity level)
            let (bg_col, border_col, val_col) = if cell.star_count == 0 {
                (
                    Color32::from_rgb(13, 38, 38),
                    Color32::from_rgb(32, 74, 74),
                    Color32::from_rgb(120, 145, 145),
                )
            } else if self.tilt_metric == TiltMetric::Eccentricity {
                let e = val_opt.unwrap_or(0.0);
                if e <= 0.42 {
                    (
                        Color32::from_rgb(16, 34, 28),
                        Color32::from_rgb(48, 125, 88),
                        Color32::from_rgb(140, 235, 180),
                    )
                } else if e <= 0.58 {
                    (
                        Color32::from_rgb(38, 32, 18),
                        Color32::from_rgb(145, 115, 45),
                        Color32::from_rgb(255, 215, 115),
                    )
                } else {
                    (
                        Color32::from_rgb(44, 20, 22),
                        Color32::from_rgb(155, 62, 62),
                        Color32::from_rgb(255, 140, 135),
                    )
                }
            } else {
                let abs_d = delta_opt.unwrap_or(0.0).abs();
                if idx == 4 {
                    (
                        Color32::from_rgb(18, 30, 44),
                        Color32::from_rgb(70, 130, 195),
                        Color32::from_rgb(165, 220, 255),
                    )
                } else if abs_d <= 7.0 {
                    (
                        Color32::from_rgb(16, 34, 28),
                        Color32::from_rgb(48, 125, 88),
                        Color32::from_rgb(140, 235, 180),
                    )
                } else if abs_d <= 16.0 {
                    (
                        Color32::from_rgb(38, 32, 18),
                        Color32::from_rgb(145, 115, 45),
                        Color32::from_rgb(255, 215, 115),
                    )
                } else {
                    (
                        Color32::from_rgb(44, 20, 22),
                        Color32::from_rgb(155, 62, 62),
                        Color32::from_rgb(255, 140, 135),
                    )
                }
            };

            painter.rect_filled(rect, 3.0, bg_col);
            painter.rect_stroke(
                rect,
                3.0,
                Stroke::new(1.0_f32, border_col),
                egui::StrokeKind::Middle,
            );

            let f_small = egui::FontId::monospace((font_size - 1.5).max(8.5));
            let f_val = egui::FontId::monospace((font_size + 0.5).clamp(10.0, 12.5));

            // Top row: Code + star count
            painter.text(
                egui::pos2(rect.left() + 4.0, rect.top() + 2.0),
                egui::Align2::LEFT_TOP,
                format!("{} {}★", cell.code, cell.star_count),
                f_small.clone(),
                Color32::from_rgb(155, 170, 190),
            );

            // Middle row: Metric value
            let v_txt = match (val_opt, self.tilt_metric) {
                (None, _) => "—".to_owned(),
                (Some(v), TiltMetric::Eccentricity) => format!("{v:.2}"),
                (Some(v), _) => match self.display_arcsec_per_px() {
                    Some(s) => format!("{:.2}\u{2033}", v * s),
                    None => format!("{v:.2}"),
                },
            };
            painter.text(
                egui::pos2(rect.center().x, rect.center().y + 1.0),
                egui::Align2::CENTER_CENTER,
                v_txt,
                f_val,
                val_col,
            );

            // Bottom row: Delta % from Center
            let d_txt = if idx == 4 {
                "Center".to_owned()
            } else if let Some(d) = delta_opt {
                format!("{d:+.0}%")
            } else {
                String::new()
            };
            if !d_txt.is_empty() {
                painter.text(
                    egui::pos2(rect.center().x, rect.bottom() - 2.0),
                    egui::Align2::CENTER_BOTTOM,
                    d_txt,
                    f_small,
                    Color32::from_rgb(175, 185, 200),
                );
            }

            // Cell hover tooltip
            let cell_resp =
                ui.interact(rect, ui.id().with(("tilt_cell", idx)), egui::Sense::hover());
            if cell_resp.hovered() {
                let tip = format!(
                    "{} ({}) — {} stars\nFWHM: {}\nHFR: {}\nEccentricity: {}",
                    cell.name,
                    cell.code,
                    cell.star_count,
                    cell.median_fwhm_px
                        .map_or_else(|| "—".to_owned(), |v| self.fmt_len(v)),
                    cell.median_hfr_px
                        .map_or_else(|| "—".to_owned(), |v| self.fmt_len(v)),
                    cell.median_eccentricity
                        .map_or_else(|| "—".to_owned(), |v| format!("{v:.2}")),
                );
                cell_resp.on_hover_text(tip);
            }
        }

        // Curvature & Corner Tilt summary — wraps Corner Tilt onto the next line when the column is narrowed
        let curv_str = report
            .tilt_map
            .corner_to_center_fwhm_pct
            .map_or_else(|| "—".to_owned(), |v| format!("{v:+.1}%"));
        let tilt_str = match (
            report.tilt_map.max_corner_tilt_px,
            report.tilt_map.max_corner_tilt_pct,
        ) {
            (Some(px), Some(pct)) => format!("{} ({pct:.0}%)", self.fmt_len(px)),
            _ => "—".to_owned(),
        };
        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w - 6.0);
            ui.spacing_mut().item_spacing = egui::vec2(10.0, 2.0);
            ui.label(
                RichText::new(format!("Curvature: {curv_str}"))
                    .size(font_size - 0.5)
                    .monospace()
                    .weak(),
            )
            .on_hover_text("Mean corner FWHM relative to the Center sensor zone");
            ui.label(
                RichText::new(format!("Corner Tilt Δ: {tilt_str}"))
                    .size(font_size - 0.5)
                    .monospace()
                    .weak(),
            )
            .on_hover_text("Difference between highest and lowest corner FWHM");
        });

        self.show_field_analysis(ui, report, panel_w, font_size);
    }

    /// Arcseconds per pixel to display lengths with, or `None` to display pixels (toggle off,
    /// or the frame's scale is unknown).
    pub(super) fn display_arcsec_per_px(&self) -> Option<f64> {
        self.active_arcsec_per_px.filter(|_| self.lengths_in_arcsec)
    }

    /// Format a length (px) in the unit chosen in the Star Quality panel.
    pub(super) fn fmt_len(&self, px: f64) -> String {
        format_length(px, self.display_arcsec_per_px())
    }

    /// Field curvature / tilt / collimation section of the star panel.
    pub(super) fn show_field_analysis(
        &mut self,
        ui: &mut egui::Ui,
        report: &StarQualityReport,
        panel_w: f32,
        font_size: f32,
    ) {
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w - 6.0);
            ui.label(
                RichText::new("Field Curvature & Collimation")
                    .size(font_size + 0.5)
                    .strong(),
            );
        });
        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w - 6.0);
            ui.label(RichText::new("Map:").size(font_size).weak());
            for (choice, label, tip) in [
                (None, "Off", "No field map"),
                (
                    Some(MapKind::Fwhm),
                    "FWHM [C]",
                    "FWHM map: field curvature, tilt and collimation (blue = sharp, red = soft)",
                ),
                (
                    Some(MapKind::Eccentricity),
                    "Ecc [E]",
                    "Eccentricity map: star elongation across the field (blue = round, red = elongated)",
                ),
            ] {
                if ui
                    .selectable_label(self.field_map == choice, RichText::new(label).size(font_size))
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.field_map = choice;
                }
            }
        });
        let Some(field) = &report.field else {
            ui.label(
                RichText::new(format!(
                    "Needs at least {} measured stars.",
                    fulgur_engine::field::MIN_FIELD_STARS
                ))
                .size(font_size)
                .weak(),
            );
            return;
        };
        let mut text = field.summary_text_in(self.display_arcsec_per_px());
        if let Some(ecc) = &report.eccentricity_map {
            text.push('\n');
            text.push_str(&ecc.summary_text());
        }
        ui.label(RichText::new(&text).size(font_size - 0.5).monospace())
            .on_hover_text(
                "FWHM of all measured stars fitted with a smooth surface.\n\
                 Curvature: corners vs sharpest point.\n\
                 Tilt X: right minus left edge; Tilt Y: bottom minus top edge.\n\
                 Total Tilt: steepest edge-to-edge difference; the angle points to the softer side (0 = right, 90 = up).\n\
                 Collimation: distance of the sharpest point from the frame centre.",
            );
        if ui
            .small_button("Copy")
            .on_hover_text("Copy the field analysis to the clipboard")
            .clicked()
        {
            ui.ctx().copy_text(text);
            self.status_banner = Some((
                "Copied field analysis to clipboard.".to_owned(),
                Instant::now(),
            ));
        }
    }
}
