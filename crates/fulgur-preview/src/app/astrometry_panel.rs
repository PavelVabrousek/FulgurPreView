//! Astrometry & WCS panel: solve, solution summary, overlays, WCS save.

use super::*;

impl FulgurPreViewApp {
    pub(super) fn show_astrometry_panel(
        &mut self,
        ui: &mut egui::Ui,
        plate_report: Option<&Arc<PlateSolveReport>>,
        is_solving: bool,
        solve_error: Option<&str>,
    ) {
        let panel_w = ui.available_width().max(120.0);
        ui.set_max_width(panel_w);
        let font_size = (panel_w * 0.048).clamp(9.5, 12.0);
        let heading_size = (panel_w * 0.060).clamp(11.5, 15.0);

        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.label(
                RichText::new("Astrometry & WCS")
                    .size(heading_size)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui
                    .selectable_label(
                        self.show_catalog_settings,
                        RichText::new("⚙ Paths").size(font_size - 0.5),
                    )
                    .on_hover_text(
                        "Configure local Gaia DR3 (.xpsd) and Blind Quad Index (.fsidx) paths",
                    )
                    .clicked()
                {
                    self.show_catalog_settings = !self.show_catalog_settings;
                }
                let btn_txt = if is_solving {
                    RichText::new("✕ Cancel")
                        .size(font_size - 0.5)
                        .color(Color32::from_rgb(255, 160, 140))
                } else {
                    RichText::new("⚡ Solve [S]")
                        .size(font_size - 0.5)
                        .color(Color32::from_rgb(255, 225, 95))
                };
                if ui
                    .button(btn_txt)
                    .on_hover_text(self.solve_button_hint(is_solving))
                    .clicked()
                {
                    self.trigger_plate_solve();
                }
            });
        });

        // Optional Catalogue & Index Paths Configuration Card
        if self.show_catalog_settings {
            egui::Frame::group(ui.style())
                .fill(Color32::from_rgb(12, 36, 36))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 98, 98)))
                .show(ui, |ui| {
                    let inner_w = (panel_w - 20.0).max(100.0);
                    ui.set_max_width(inner_w);
                    add_truncated_label(
                        ui,
                        RichText::new(&self.catalog_config.source_note)
                            .size(font_size - 1.0)
                            .weak(),
                        inner_w,
                        &self.catalog_config.source_note,
                    );

                    let has_gaia = self.catalog_config.has_local_gaia();
                    let gaia_badge = if has_gaia {
                        ("Gaia DR3 (.xpsd): Ready", Color32::from_rgb(130, 235, 175))
                    } else {
                        (
                            "Gaia DR3 (.xpsd): Not Found",
                            Color32::from_rgb(255, 185, 110),
                        )
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(gaia_badge.0)
                                .size(font_size - 0.5)
                                .color(gaia_badge.1)
                                .strong(),
                        );
                        if ui
                            .small_button(RichText::new("Browse...").size(font_size - 1.0))
                            .clicked()
                        {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.catalog_config.gaia_folder =
                                    dir.to_string_lossy().into_owned();
                            }
                        }
                    });
                    if !self.catalog_config.gaia_folder.is_empty() {
                        add_truncated_label(
                            ui,
                            RichText::new(&self.catalog_config.gaia_folder)
                                .size(font_size - 1.0)
                                .monospace()
                                .weak(),
                            inner_w,
                            &self.catalog_config.gaia_folder,
                        );
                    }

                    let idx_opt = self.catalog_config.index_file_path();
                    let idx_badge = if idx_opt.is_some() {
                        (
                            "Blind Index (.fsidx): Ready",
                            Color32::from_rgb(130, 235, 175),
                        )
                    } else {
                        (
                            "Blind Index (.fsidx): Not Found",
                            Color32::from_rgb(255, 185, 110),
                        )
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(idx_badge.0)
                                .size(font_size - 0.5)
                                .color(idx_badge.1)
                                .strong(),
                        );
                        if ui
                            .small_button(RichText::new("Browse...").size(font_size - 1.0))
                            .clicked()
                        {
                            if let Some(file) = rfd::FileDialog::new()
                                .add_filter("FulgurStack Quad Index", &["fsidx"])
                                .pick_file()
                            {
                                self.catalog_config.solve_index_folder =
                                    file.to_string_lossy().into_owned();
                            }
                        }
                    });
                    if let Some(p) = &idx_opt {
                        let s = p.to_string_lossy();
                        add_truncated_label(
                            ui,
                            RichText::new(s.as_ref())
                                .size(font_size - 1.0)
                                .monospace()
                                .weak(),
                            inner_w,
                            s.as_ref(),
                        );
                    }

                    ui.horizontal(|ui| {
                        if ui
                            .small_button(RichText::new("Auto-Detect").size(font_size - 0.5))
                            .on_hover_text(
                                "Re-discover paths from %LOCALAPPDATA%\\FulgurStack\\settings.json",
                            )
                            .clicked()
                        {
                            self.catalog_config = CatalogConfig::discover();
                        }
                        if ui
                            .small_button(RichText::new("Save Paths").size(font_size - 0.5))
                            .on_hover_text("Save catalogue paths to FulgurPreView settings.json")
                            .clicked()
                        {
                            match self.catalog_config.save() {
                                Ok(()) => {
                                    self.status_banner = Some((
                                        "Saved Gaia & Blind Index paths to FulgurPreView settings."
                                            .to_owned(),
                                        Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    self.status_banner =
                                        Some((format!("Save paths failed: {e}"), Instant::now()));
                                }
                            }
                        }
                    });
                });
            ui.add_space(3.0);
        }

        if is_solving {
            ui.label(
                RichText::new("⏳ Solving astrometric WCS against local Gaia DR3...")
                    .size(font_size)
                    .color(Color32::from_rgb(255, 220, 110)),
            );
        }
        if let Some(err) = solve_error {
            ui.label(
                RichText::new(format!("Solve failed: {err}"))
                    .size(font_size - 0.5)
                    .color(Color32::from_rgb(255, 135, 130)),
            );
        }

        let Some(report) = plate_report else {
            if !is_solving && solve_error.is_none() {
                // ADR-024: header WCS stays hidden until the user presses Solve.
                let has_header_wcs = self
                    .current_cache_key()
                    .is_some_and(|k| self.cache.has_hidden_header_wcs(&k));
                let text = if has_header_wcs {
                    "This file already contains a WCS in its header. Press Solve [S] to show it (no solve needed)."
                } else {
                    "No WCS solution yet. Press Solve [S] to plate-solve this frame."
                };
                ui.label(RichText::new(text).size(font_size).weak());
            }
            return;
        };

        let wcs = &report.wcs;
        let (c_ra, c_dec) = wcs.centre_sky();
        let (fov_w, fov_h) = wcs.fov_deg();
        let scale = wcs.scale();
        let north_deg = wcs.north_deg();
        let parity_str = if wcs.mirrored() { "Mirrored" } else { "Normal" };

        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(13, 40, 40))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(38, 84, 84)))
            .show(ui, |ui| {
                let inner_w = (panel_w - 20.0).max(100.0);
                ui.set_max_width(inner_w);

                let status_col = match report.method {
                    SolveMethod::HeaderWcs => Color32::from_rgb(130, 210, 255),
                    SolveMethod::LocalGaiaCone | SolveMethod::BlindQuadIndex => {
                        Color32::from_rgb(135, 240, 175)
                    }
                };
                let status_line = if report.elapsed_ms > 0.0 {
                    format!("✓ {} ({:.0} ms)", report.method.label(), report.elapsed_ms)
                } else {
                    format!("✓ {}", report.method.label())
                };
                add_truncated_label(
                    ui,
                    RichText::new(&status_line)
                        .size(font_size)
                        .strong()
                        .color(status_col),
                    inner_w,
                    &status_line,
                );

                let ra_line = format!("Center RA:  {}  ({:.4}°)", format_ra(c_ra), c_ra);
                let dec_line = format!("Center Dec: {}  ({:+.4}°)", format_dec(c_dec), c_dec);
                add_truncated_label(
                    ui,
                    RichText::new(&ra_line)
                        .size(font_size - 0.5)
                        .monospace()
                        .color(Color32::from_rgb(205, 235, 255)),
                    inner_w,
                    &ra_line,
                );
                add_truncated_label(
                    ui,
                    RichText::new(&dec_line)
                        .size(font_size - 0.5)
                        .monospace()
                        .color(Color32::from_rgb(205, 235, 255)),
                    inner_w,
                    &dec_line,
                );

                let fov_line = format!(
                    "FoV: {:.2}° × {:.2}° ({:.1}′ × {:.1}′)",
                    fov_w,
                    fov_h,
                    fov_w * 60.0,
                    fov_h * 60.0
                );
                add_truncated_label(
                    ui,
                    RichText::new(&fov_line).size(font_size - 0.5).monospace(),
                    inner_w,
                    &fov_line,
                );

                let scale_line = format!(
                    "Scale: {:.3} ″/px · PA {:+.1}° · {}",
                    scale, north_deg, parity_str
                );
                add_truncated_label(
                    ui,
                    RichText::new(&scale_line)
                        .size(font_size - 0.5)
                        .monospace()
                        .color(Color32::from_rgb(240, 220, 140)),
                    inner_w,
                    &scale_line,
                );

                let model_line = if report.matched_stars > 0 {
                    format!(
                        "Fit: {} · {}★ · RMS {:.2} px",
                        wcs.model_label(),
                        report.matched_stars,
                        report.fit_rms_px
                    )
                } else {
                    format!("Model: {}", wcs.model_label())
                };
                add_truncated_label(
                    ui,
                    RichText::new(&model_line)
                        .size(font_size - 0.5)
                        .monospace()
                        .weak(),
                    inner_w,
                    &model_line,
                );

                if !report.field_dsos.is_empty() {
                    let names: Vec<String> = report
                        .field_dsos
                        .iter()
                        .take(4)
                        .map(|d| d.display_label())
                        .collect();
                    let dso_line = format!("Objects: {}", names.join(", "));
                    add_truncated_label(
                        ui,
                        RichText::new(&dso_line)
                            .size(font_size - 0.5)
                            .color(Color32::from_rgb(255, 215, 115)),
                        inner_w,
                        &dso_line,
                    );
                }
            });

        ui.add_space(3.0);

        // Sky Overlay Toggles
        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 2.0);
            ui.checkbox(
                &mut self.sky_overlays.show_radec_grid,
                RichText::new("RA/Dec Grid").size(font_size),
            )
            .on_hover_text("Draw curved celestial RA/Dec coordinate grid over the image");
            ui.checkbox(
                &mut self.sky_overlays.show_compass,
                RichText::new("N/E Compass").size(font_size),
            )
            .on_hover_text("Draw North/East orientation rose in the top-right corner");
            ui.checkbox(
                &mut self.sky_overlays.show_dso_annotations,
                RichText::new("DSO Labels").size(font_size),
            )
            .on_hover_text("Annotate Messier, NGC, and IC deep-sky objects inside the field");
            ui.checkbox(
                &mut self.sky_overlays.show_star_annotations,
                RichText::new("Gaia Stars").size(font_size),
            )
            .on_hover_text("Annotate bright Gaia DR3 stars with G-band magnitudes");
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.sky_overlays.all_visible(),
                    egui::Button::new(RichText::new("Show all").size(font_size)).small(),
                )
                .on_hover_text("Turn on the grid, compass, DSO labels and Gaia stars")
                .clicked()
            {
                self.sky_overlays.set_all(true);
            }
            if ui
                .add_enabled(
                    !self.sky_overlays.none_visible(),
                    egui::Button::new(RichText::new("Hide all").size(font_size)).small(),
                )
                .on_hover_text("Turn off all celestial overlays for an unobstructed view")
                .clicked()
            {
                self.sky_overlays.set_all(false);
            }
        });

        self.show_small_body_results(ui, panel_w, font_size);

        // Header WCS is already in the file: saving it back would change nothing (ADR-024).
        let from_header = report.method == SolveMethod::HeaderWcs;
        let can_save_wcs = !from_header && self.playlist.current_path().is_some_and(is_astro_input);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    can_save_wcs,
                    egui::Button::new(
                        RichText::new("💾 Save WCS to Header")
                            .size(font_size)
                            .color(Color32::from_rgb(145, 235, 185)),
                    ),
                )
                .on_hover_text(if from_header {
                    "This WCS already comes from the file header. Press Solve [S] to re-solve; a new solution can then be saved."
                } else {
                    "Atomically write solved WCS cards (CRVAL, CRPIX, CD, SIP) to the FITS/XISF header via a .tmp file and rename"
                })
                .on_disabled_hover_text(if from_header {
                    "This WCS already comes from the file header. Press Solve [S] to re-solve; a new solution can then be saved."
                } else {
                    "WCS header save is supported for FITS and XISF files."
                })
                .clicked()
            {
                let rep_clone = (**report).clone();
                self.save_wcs_to_current_file(&rep_clone);
            }
        });
    }
}
