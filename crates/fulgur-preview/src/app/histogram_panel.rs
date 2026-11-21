//! Histogram panel with signal zoom and Min / Max sliders.

use super::*;

impl FulgurPreViewApp {
    pub(super) fn show_histogram_panel(
        &mut self,
        ui: &mut egui::Ui,
        frame: &OverviewFrame,
        active_params: StretchParams,
    ) {
        let panel_w = ui.available_width().max(120.0);
        ui.set_max_width(panel_w);
        let font_size = (panel_w * 0.048).clamp(9.5, 12.0);
        let heading_size = (panel_w * 0.060).clamp(11.5, 15.0);

        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.label(
                RichText::new("Signal Histogram")
                    .size(heading_size)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button(RichText::new("Full").size(font_size - 0.5))
                    .on_hover_text("Show full [0.0 .. 1.0] sensor range")
                    .clicked()
                {
                    self.hist_zoom_to_signal = false;
                    self.hist_range_min = 0.0;
                    self.hist_range_max = 1.0;
                }
                let mut zoom_sig = self.hist_zoom_to_signal;
                if ui
                    .checkbox(
                        &mut zoom_sig,
                        RichText::new("Zoom to Signal").size(font_size),
                    )
                    .on_hover_text(
                        "Automatically zoom X-axis to the linear sky-background and nebulosity peak",
                    )
                    .changed()
                {
                    self.hist_zoom_to_signal = zoom_sig;
                    if zoom_sig {
                        self.hist_range_min = frame.histogram.signal_range[0];
                        self.hist_range_max = frame.histogram.signal_range[1];
                    } else {
                        self.hist_range_min = 0.0;
                        self.hist_range_max = 1.0;
                    }
                }
            });
        });

        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.checkbox(&mut self.hist_log_y, RichText::new("Log Y").size(font_size))
                .on_hover_text("Logarithmic vertical scale to reveal faint nebula/star tails");
            ui.checkbox(
                &mut self.hist_show_mtf,
                RichText::new("MTF").size(font_size),
            )
            .on_hover_text("Overlay active MTF stretch curve and Black/Mid/White markers");

            for (ch_idx, label, col) in [
                (0, "R", Color32::from_rgb(245, 95, 95)),
                (1, "G", Color32::from_rgb(95, 225, 120)),
                (2, "B", Color32::from_rgb(100, 165, 255)),
                (3, "L", Color32::from_rgb(215, 220, 230)),
            ] {
                if ui
                    .selectable_label(
                        self.hist_show_channels[ch_idx],
                        RichText::new(label).size(font_size).color(col).strong(),
                    )
                    .clicked()
                {
                    self.hist_show_channels[ch_idx] = !self.hist_show_channels[ch_idx];
                }
            }
        });

        let r_min = self.hist_range_min.clamp(0.0, 0.9995);
        let r_max = self.hist_range_max.clamp(r_min + 0.0005, 1.0);
        let bins = frame.bins_for_range(r_min, r_max);

        // Plot canvas (strictly hover-only so mouse wheel always zooms the main image viewport)
        let plot_w = (panel_w - 6.0).max(100.0);
        let plot_h = 118.0_f32;
        let (plot_rect, plot_resp) =
            ui.allocate_exact_size(egui::vec2(plot_w, plot_h), egui::Sense::hover());

        if ui.is_rect_visible(plot_rect) {
            let painter = ui.painter_at(plot_rect);
            painter.rect_filled(plot_rect, 4.0, Color32::from_rgb(11, 34, 34));
            painter.rect_stroke(
                plot_rect,
                4.0,
                Stroke::new(1.0_f32, Color32::from_rgb(38, 84, 84)),
                egui::StrokeKind::Middle,
            );

            // Subtle grid lines (25%, 50%, 75%)
            let grid_stroke = Stroke::new(1.0_f32, Color32::from_rgb(20, 56, 56));
            for frac in [0.25_f32, 0.50, 0.75] {
                let gx = plot_rect.left() + plot_rect.width() * frac;
                painter.line_segment(
                    [
                        egui::pos2(gx, plot_rect.top()),
                        egui::pos2(gx, plot_rect.bottom()),
                    ],
                    grid_stroke,
                );
                let gy = plot_rect.top() + plot_rect.height() * frac;
                painter.line_segment(
                    [
                        egui::pos2(plot_rect.left(), gy),
                        egui::pos2(plot_rect.right(), gy),
                    ],
                    grid_stroke,
                );
            }

            let mut max_bin = 1_u32;
            for (ch, ch_bins) in bins.iter().enumerate() {
                if self.hist_show_channels[ch] {
                    for &c in ch_bins {
                        max_bin = max_bin.max(c);
                    }
                }
            }
            let max_y = if self.hist_log_y {
                (max_bin as f32 + 1.0).ln().max(1.0)
            } else {
                max_bin as f32
            };

            let ch_colors = [
                (
                    Color32::from_rgb(240, 85, 85),
                    Color32::from_rgba_unmultiplied(240, 85, 85, 32),
                ),
                (
                    Color32::from_rgb(85, 220, 115),
                    Color32::from_rgba_unmultiplied(85, 220, 115, 32),
                ),
                (
                    Color32::from_rgb(90, 160, 250),
                    Color32::from_rgba_unmultiplied(90, 160, 250, 32),
                ),
                (
                    Color32::from_rgb(215, 220, 230),
                    Color32::from_rgba_unmultiplied(215, 220, 230, 28),
                ),
            ];

            let inner = plot_rect.shrink(2.0);
            for (ch, ch_bins) in bins.iter().enumerate() {
                if !self.hist_show_channels[ch] {
                    continue;
                }
                let (stroke_col, fill_col) = ch_colors[ch];
                let mut pts = Vec::with_capacity(HISTOGRAM_BINS);
                for (i, &cnt) in ch_bins.iter().enumerate() {
                    let t = i as f32 / (HISTOGRAM_BINS - 1) as f32;
                    let x = inner.left() + t * inner.width();
                    let val_y = if self.hist_log_y {
                        if cnt == 0 {
                            0.0
                        } else {
                            ((cnt as f32 + 1.0).ln() / max_y).clamp(0.0, 1.0)
                        }
                    } else {
                        (cnt as f32 / max_y).clamp(0.0, 1.0)
                    };
                    let y = inner.bottom() - val_y * inner.height();
                    pts.push(egui::pos2(x, y));

                    if cnt > 0 {
                        let col_w = (inner.width() / HISTOGRAM_BINS as f32).max(1.0);
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(x - col_w * 0.5, y),
                                egui::pos2(x + col_w * 0.5, inner.bottom()),
                            ),
                            0.0,
                            fill_col,
                        );
                    }
                }
                painter.add(egui::Shape::line(pts, Stroke::new(1.3_f32, stroke_col)));
            }

            // Optional MTF transfer curve & Black/White markers overlay
            if self.hist_show_mtf {
                let r_span = (r_max - r_min).max(1e-6);
                let black_avg =
                    (active_params.black[0] + active_params.black[1] + active_params.black[2])
                        / 3.0;
                let mid_avg =
                    (active_params.mid[0] + active_params.mid[1] + active_params.mid[2]) / 3.0;
                let white_avg =
                    (active_params.white[0] + active_params.white[1] + active_params.white[2])
                        / 3.0;

                // Vertical Black point line
                if black_avg >= r_min && black_avg <= r_max {
                    let bx = inner.left() + ((black_avg - r_min) / r_span) * inner.width();
                    painter.line_segment(
                        [egui::pos2(bx, inner.top()), egui::pos2(bx, inner.bottom())],
                        Stroke::new(1.0_f32, Color32::from_rgb(255, 190, 90)),
                    );
                }
                // Vertical White point line
                if white_avg >= r_min && white_avg <= r_max {
                    let wx = inner.left() + ((white_avg - r_min) / r_span) * inner.width();
                    painter.line_segment(
                        [egui::pos2(wx, inner.top()), egui::pos2(wx, inner.bottom())],
                        Stroke::new(1.0_f32, Color32::from_rgb(255, 190, 90)),
                    );
                }

                // MTF curve
                let steps = 96_usize;
                let mut mtf_pts = Vec::with_capacity(steps + 1);
                for s in 0..=steps {
                    let t = s as f32 / steps as f32;
                    let x_norm = r_min + t * r_span;
                    let y_mtf = evaluate_mtf_curve(
                        x_norm,
                        black_avg,
                        mid_avg,
                        white_avg,
                        active_params.invert,
                    );
                    let px = inner.left() + t * inner.width();
                    let py = inner.bottom() - y_mtf * inner.height();
                    mtf_pts.push(egui::pos2(px, py));
                }
                painter.add(egui::Shape::line(
                    mtf_pts,
                    Stroke::new(1.4_f32, Color32::from_rgba_unmultiplied(255, 210, 110, 210)),
                ));
            }

            // Hover crosshair inside histogram plot
            if let Some(hover_pos) = plot_resp.hover_pos() {
                if plot_rect.contains(hover_pos) {
                    painter.line_segment(
                        [
                            egui::pos2(hover_pos.x, plot_rect.top()),
                            egui::pos2(hover_pos.x, plot_rect.bottom()),
                        ],
                        Stroke::new(1.0_f32, Color32::from_rgb(170, 200, 240)),
                    );
                }
            }
        }

        // Hover readout or range summary line
        let min_adu = frame.norm_min + r_min * frame.norm_span;
        let max_adu = frame.norm_min + r_max * frame.norm_span;
        if let Some(hover_pos) = plot_resp.hover_pos() {
            let t = ((hover_pos.x - plot_rect.left()) / plot_rect.width().max(1.0)).clamp(0.0, 1.0);
            let h_norm = r_min + t * (r_max - r_min);
            let h_adu = frame.norm_min + h_norm * frame.norm_span;
            ui.label(
                RichText::new(format!("Cursor: {:.1} ADU (norm {:.4})", h_adu, h_norm))
                    .size(font_size - 0.5)
                    .monospace()
                    .color(Color32::from_rgb(240, 220, 140)),
            );
        } else {
            ui.label(
                RichText::new(format!(
                    "Window: [{:.0} .. {:.0} ADU] ([{:.4} .. {:.4}])",
                    min_adu, max_adu, r_min, r_max
                ))
                .size(font_size - 0.5)
                .monospace()
                .weak(),
            );
        }

        // Explicit Min & Max X-axis sliders inside the histogram panel (Mouse Wheel is reserved for image zoom)
        let mut new_min = self.hist_range_min;
        let mut new_max = self.hist_range_max;
        let mut range_changed = false;

        ui.horizontal(|ui| {
            ui.set_max_width(panel_w);
            ui.label(RichText::new("Min:").size(font_size).weak());
            ui.spacing_mut().slider_width = (panel_w - 105.0).max(50.0);
            range_changed |= ui
                .add(
                    egui::Slider::new(&mut new_min, 0.0..=0.999)
                        .logarithmic(true)
                        .fixed_decimals(4),
                )
                .on_hover_text("Histogram X-axis minimum bound (logarithmic precision)")
                .changed();
        });
        ui.horizontal(|ui| {
            ui.set_max_width(panel_w);
            ui.label(RichText::new("Max:").size(font_size).weak());
            ui.spacing_mut().slider_width = (panel_w - 105.0).max(50.0);
            range_changed |= ui
                .add(
                    egui::Slider::new(&mut new_max, 0.0005..=1.0)
                        .logarithmic(true)
                        .fixed_decimals(4),
                )
                .on_hover_text("Histogram X-axis maximum bound (logarithmic precision)")
                .changed();
        });

        if range_changed {
            self.hist_zoom_to_signal = false;
            self.hist_range_min = new_min.min(new_max - 0.0005).max(0.0);
            self.hist_range_max = new_max.max(self.hist_range_min + 0.0005).min(1.0);
        }

        // Compact quantitative linear statistics (Median & Noise Sigma in ADU)
        let st_r = &frame.histogram.stats[0];
        let st_g = &frame.histogram.stats[1];
        let st_b = &frame.histogram.stats[2];
        let st_l = &frame.histogram.stats[3];
        let stats_line = if frame.info.mosaic == Mosaic::Mono {
            format!(
                "Med: {:.0} ADU  ·  σ: {:.1} ADU  ·  Clip: {:.2}%/{:.2}%",
                st_l.median_adu, st_l.sigma_adu, st_l.clipped_low_pct, st_l.clipped_high_pct
            )
        } else {
            format!(
                "Med R/G/B: {:.0}/{:.0}/{:.0} ADU · σ: {:.1}/{:.1}/{:.1}",
                st_r.median_adu,
                st_g.median_adu,
                st_b.median_adu,
                st_r.sigma_adu,
                st_g.sigma_adu,
                st_b.sigma_adu
            )
        };
        add_truncated_label(
            ui,
            RichText::new(&stats_line)
                .size(font_size - 0.5)
                .monospace()
                .weak(),
            panel_w - 8.0,
            &stats_line,
        );
    }

    /// Median and noise per channel in ADU, below the histogram. Full-resolution values from
    /// the star analysis when available, otherwise the 2048 px overview (noise is then lower
    /// than on the real frame because the overview averages pixels).
    pub(super) fn show_signal_levels(
        &self,
        ui: &mut egui::Ui,
        frame: &OverviewFrame,
        star_report: Option<&StarQualityReport>,
    ) {
        let font_size = (ui.available_width() * 0.048).clamp(9.5, 12.0);
        let full_res = star_report
            .map(|r| r.levels.as_slice())
            .filter(|l| !l.is_empty());
        let rows: Vec<(String, f64, f64)> = match full_res {
            Some(levels) => levels
                .iter()
                .map(|l| (l.name.to_owned(), l.median_adu, l.noise_adu))
                .collect(),
            None => {
                let names = if frame.info.planes >= 3 || frame.info.bayer_pattern.is_some() {
                    vec![(0, "R"), (1, "G"), (2, "B"), (3, "L")]
                } else {
                    vec![(3, "Mono")]
                };
                names
                    .into_iter()
                    .map(|(i, n)| {
                        let st = &frame.histogram.stats[i];
                        (
                            n.to_owned(),
                            f64::from(st.median_adu),
                            f64::from(st.sigma_adu),
                        )
                    })
                    .collect()
            }
        };
        if rows.is_empty() {
            return;
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(if full_res.is_some() {
                "Signal levels (full resolution, ADU)"
            } else {
                "Signal levels (overview, ADU)"
            })
            .size(font_size)
            .strong(),
        )
        .on_hover_text(
            "Median: typical pixel value (sky background on deep-sky frames).
             Noise: pixel-to-pixel noise from differences of neighbouring same-colour pixels              (robust to gradients, nebulosity and stars). Overview values are shown until the              star analysis finishes; their noise is lower because the overview averages pixels.",
        );
        egui::Grid::new("signal_levels_grid")
            .num_columns(4)
            .spacing([12.0, 1.0])
            .show(ui, |ui| {
                for head in ["Ch", "Median", "Noise", "SNR"] {
                    ui.label(RichText::new(head).size(font_size - 0.5).weak());
                }
                ui.end_row();
                for (name, median, noise) in rows {
                    let color = match name.as_str() {
                        "R" => Color32::from_rgb(255, 120, 110),
                        "G" => Color32::from_rgb(120, 220, 130),
                        "B" => Color32::from_rgb(120, 170, 255),
                        _ => Color32::from_rgb(220, 220, 220),
                    };
                    ui.label(RichText::new(name).size(font_size).monospace().color(color));
                    ui.label(
                        RichText::new(format!("{median:.1}"))
                            .size(font_size)
                            .monospace(),
                    );
                    ui.label(
                        RichText::new(format!("{noise:.2}"))
                            .size(font_size)
                            .monospace(),
                    );
                    let snr = if noise > 1e-9 { median / noise } else { 0.0 };
                    ui.label(
                        RichText::new(format!("{snr:.0}"))
                            .size(font_size)
                            .monospace(),
                    )
                    .on_hover_text("Median / noise of the background (not a star SNR)");
                    ui.end_row();
                }
            });
    }
}
