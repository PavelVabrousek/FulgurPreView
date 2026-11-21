//! Top toolbar: navigation, sorting, zoom, stretch and action buttons.

use super::*;
use crate::slider_scale::{ChannelPoints, MedianSliderScale};

impl FulgurPreViewApp {
    pub(super) fn show_top_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        active_frame: Option<&Arc<OverviewFrame>>,
    ) {
        let has_files = !self.playlist.is_empty();
        let is_maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));

        // Row 1: Integrated Window Title Bar + File & Folder Navigation + Window Control Triplet
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            // Application icon + title (also acts as draggable title bar area)
            let (icon_rect, icon_resp) =
                ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::drag());
            draw_app_star_icon(ui.painter(), icon_rect.center(), 7.5);
            let title_resp = ui.add(
                egui::Label::new(
                    RichText::new(concat!("FulgurPreView ", env!("CARGO_PKG_VERSION")))
                        .strong()
                        .color(Color32::from_rgb(185, 215, 255)),
                )
                .sense(egui::Sense::click_and_drag()),
            );
            if icon_resp.drag_started() || title_resp.drag_started() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if title_resp.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
            }

            ui.separator();

            if ui
                .button("Open File")
                .on_hover_text("Open an astronomical or standard image file (Ctrl+O)")
                .clicked()
            {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter(
                        "Astro & Standard Images",
                        &[
                            "fit", "fits", "fts", "xisf", "tif", "tiff", "jpg", "jpeg", "png",
                            "webp", "bmp", "cr2", "cr3", "nef", "arw", "dng", "orf", "pef", "raf",
                            "rw2",
                        ],
                    )
                    .pick_file()
                {
                    self.open_path(&file);
                }
            }
            if ui
                .button("Folder")
                .on_hover_text("Open a folder of images")
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.open_path(&folder);
                }
            }

            ui.separator();

            // Directory navigation moved into the title bar row
            if ui
                .add_enabled(
                    has_files && self.playlist.current_index > 0,
                    egui::Button::new("◀ Prev"),
                )
                .on_hover_text("Previous image in folder (Left Arrow)")
                .clicked()
            {
                let moved = self.playlist.prev();
                self.navigate_to(moved);
            }

            let counter_text = if has_files {
                format!(
                    "{} / {}",
                    self.playlist.current_index + 1,
                    self.playlist.len()
                )
            } else {
                "0 / 0".to_owned()
            };
            ui.label(RichText::new(counter_text).strong().monospace());

            if ui
                .add_enabled(
                    has_files && self.playlist.current_index + 1 < self.playlist.len(),
                    egui::Button::new("Next ▶"),
                )
                .on_hover_text("Next image in folder (Right Arrow / Space)")
                .clicked()
            {
                let moved = self.playlist.next();
                self.navigate_to(moved);
            }

            egui::ComboBox::from_id_salt("sort_order_combo")
                .selected_text(self.playlist.sort_order.label())
                .width(105.0)
                .show_ui(ui, |ui| {
                    for order in [SortOrder::NaturalName, SortOrder::ModifiedTime] {
                        if ui
                            .selectable_label(self.playlist.sort_order == order, order.label())
                            .clicked()
                        {
                            self.playlist.set_sort_order(order);
                            self.sync_active_request();
                        }
                    }
                });

            ui.checkbox(&mut self.follow_new_frames, "Follow New")
                .on_hover_text(
                    "When viewing the last frame, jump automatically to new frames as they arrive in the folder",
                );

            ui.checkbox(&mut self.lock_view, "Lock View (Blink)")
                .on_hover_text(
                    "Preserve zoom, pan center, and stretch when switching between frames of matching size (B)",
                );

            ui.separator();

            // Mouse-wheel mode
            let wheel_label = if self.viewport.wheel_browse {
                "Wheel: Browse"
            } else {
                "Wheel: Zoom"
            };
            if ui
                .selectable_label(self.viewport.wheel_browse, wheel_label)
                .on_hover_text(
                    "Mouse wheel over the image: zoom (default) or browse to the previous / next \
                     frame. In browse mode Ctrl+wheel still zooms. Shortcut: W",
                )
                .clicked()
            {
                self.viewport.wheel_browse = !self.viewport.wheel_browse;
            }

            // Zoom controls
            if ui
                .selectable_label(self.viewport.fit, "Fit")
                .on_hover_text("Fit image to window (F / Double-click)")
                .clicked()
            {
                self.viewport.set_fit();
            }
            if ui
                .button("1:1")
                .on_hover_text("100% native pixel resolution (1)")
                .clicked()
            {
                self.viewport.set_zoom(1.0);
            }
            if ui
                .button("2:1")
                .on_hover_text("200% zoom (2)")
                .clicked()
            {
                self.viewport.set_zoom(2.0);
            }
            ui.label(
                RichText::new(format!("{:.0}%", self.viewport.scale * 100.0))
                    .monospace()
                    .weak(),
            );

            // Right-aligned window control triplet ([ — ][ □ ][ ✕ ]) + draggable empty title bar space
            let remaining = ui.available_width();
            let controls_w = 108.0_f32;
            let drag_w = (remaining - controls_w).max(8.0);
            let (_, drag_resp) =
                ui.allocate_exact_size(egui::vec2(drag_w, 22.0), egui::Sense::click_and_drag());
            if drag_resp.drag_started() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if drag_resp.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                draw_window_control_buttons(ui, is_maximized);
            });
        });

        ui.separator();

        // Row 2: Stretch, Channels, Bayer Pattern Override, Culling & Metadata Toggle
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            ui.label("Stretch:");
            for (mode, label, tip) in [
                (
                    StretchMode::Linear,
                    "Off",
                    "Linear un-stretched display (default for JPEG/PNG/sRGB TIFF)",
                ),
                (
                    StretchMode::AutoLinked,
                    "Linked",
                    "Auto MTF Stretch with shared RGB channels (preserves natural star colors)",
                ),
                (
                    StretchMode::AutoUnlinked,
                    "Unlinked",
                    "Auto MTF Stretch with independent RGB channels (neutralizes green/sky cast)",
                ),
                (
                    StretchMode::Manual,
                    "Manual",
                    "Manual Black / Midtone / White sliders",
                ),
            ] {
                if ui
                    .selectable_label(self.stretch_mode == mode, label)
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.user_overrode_stretch = true;
                    if mode == StretchMode::Manual && self.stretch_mode != StretchMode::Manual {
                        if let Some(frame) = active_frame {
                            self.manual_stretch = match self.stretch_mode {
                                StretchMode::AutoLinked => frame.linked_stretch,
                                StretchMode::AutoUnlinked => frame.unlinked_stretch,
                                _ => frame.unlinked_stretch,
                            };
                            // Keep the channels unlinked unless we start from a linked stretch.
                            self.manual_link_channels =
                                self.stretch_mode == StretchMode::AutoLinked;
                        }
                        self.show_manual_sliders = true;
                    }
                    self.stretch_mode = mode;
                }
            }

            if ui
                .selectable_label(self.show_manual_sliders, "Sliders")
                .on_hover_text("Toggle manual Black / Midtone / White stretch sliders")
                .clicked()
            {
                self.show_manual_sliders = !self.show_manual_sliders;
            }

            egui::ComboBox::from_id_salt("channel_combo")
                .selected_text(self.channel_view.label())
                .width(95.0)
                .show_ui(ui, |ui| {
                    for ch in [
                        ChannelView::Rgb,
                        ChannelView::Red,
                        ChannelView::Green,
                        ChannelView::Blue,
                        ChannelView::Luminance,
                    ] {
                        ui.selectable_value(&mut self.channel_view, ch, ch.label());
                    }
                });

            ui.checkbox(&mut self.invert, "Invert").on_hover_text(
                "Invert display luminance for inspecting faint galaxies/nebulae (I)",
            );

            // Bayer override selector
            let layout_label = match self.layout_choice {
                LayoutChoice::Auto => "Bayer: Auto",
                LayoutChoice::Mono => "Bayer: Mono",
                LayoutChoice::Bayer(BayerPattern::RGGB) => "Bayer: RGGB",
                LayoutChoice::Bayer(BayerPattern::BGGR) => "Bayer: BGGR",
                LayoutChoice::Bayer(BayerPattern::GRBG) => "Bayer: GRBG",
                LayoutChoice::Bayer(BayerPattern::GBRG) => "Bayer: GBRG",
            };
            egui::ComboBox::from_id_salt("bayer_override_combo")
                .selected_text(layout_label)
                .width(105.0)
                .show_ui(ui, |ui| {
                    for (choice, text) in [
                        (LayoutChoice::Auto, "Auto (Header / Camera DB)"),
                        (LayoutChoice::Mono, "Force Monochrome"),
                        (LayoutChoice::Bayer(BayerPattern::RGGB), "Force RGGB"),
                        (LayoutChoice::Bayer(BayerPattern::BGGR), "Force BGGR"),
                        (LayoutChoice::Bayer(BayerPattern::GRBG), "Force GRBG"),
                        (LayoutChoice::Bayer(BayerPattern::GBRG), "Force GBRG"),
                    ] {
                        if ui
                            .selectable_label(self.layout_choice == choice, text)
                            .clicked()
                        {
                            self.layout_choice = choice;
                            self.sync_active_request();
                        }
                    }
                });

            ui.separator();

            // Fast Culling & Undo
            if ui
                .add_enabled(
                    has_files,
                    egui::Button::new(
                        RichText::new("Reject [X]").color(Color32::from_rgb(255, 130, 130)),
                    ),
                )
                .on_hover_text("Move current image to _Rejected/ subfolder (X or Delete)")
                .clicked()
            {
                self.reject_current_file();
            }

            let can_undo = self.culling.can_undo();
            let undo_label = match self.culling.last_rejected_name() {
                Some(name) => format!("↩ Undo ({name})"),
                None => "↩ Undo".to_owned(),
            };
            if ui
                .add_enabled(can_undo, egui::Button::new(undo_label))
                .on_hover_text("Restore the last rejected image from _Rejected/ (Ctrl+Z)")
                .clicked()
            {
                self.undo_last_reject();
            }

            ui.separator();

            let is_solving = self
                .current_cache_key()
                .as_ref()
                .is_some_and(|k| self.cache.is_solving(k));
            let solve_btn_text = if is_solving {
                RichText::new("⏳ Solving...").color(Color32::from_rgb(255, 220, 100))
            } else {
                RichText::new("⚡ Solve [S]").color(Color32::from_rgb(255, 225, 95))
            };
            let solve_hint = self.solve_button_hint(is_solving);
            if ui
                .add_enabled(has_files, egui::Button::new(solve_btn_text))
                .on_hover_text(solve_hint)
                .clicked()
            {
                self.trigger_plate_solve();
            }

            if ui
                .selectable_label(self.show_astrometry, "Astrometry [O]")
                .on_hover_text("Show/hide Astrometry, WCS & Sky Overlays panel (O)")
                .clicked()
            {
                self.show_astrometry = !self.show_astrometry;
            }

            if ui
                .selectable_label(self.show_histogram, "Histogram [H]")
                .on_hover_text("Show/hide interactive linear Signal Histogram panel (H)")
                .clicked()
            {
                self.show_histogram = !self.show_histogram;
            }

            if ui
                .selectable_label(self.show_star_quality, "FWHM & Tilt [Q]")
                .on_hover_text(
                    "Show/hide real-time Star Quality (FWHM / HFR / Eccentricity) & 3×3 Sensor Tilt Map (Q)",
                )
                .clicked()
            {
                self.show_star_quality = !self.show_star_quality;
            }

            if ui
                .selectable_label(self.show_metadata, "Metadata [M]")
                .on_hover_text("Show/hide FITS, XISF & EXIF metadata inspector panel (M)")
                .clicked()
            {
                self.show_metadata = !self.show_metadata;
            }
        });

        // Optional manual stretch slider bar
        if self.show_manual_sliders || self.stretch_mode == StretchMode::Manual {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Manual Stretch:").strong());
                let points = ChannelPoints {
                    black: self.manual_stretch.black,
                    mid: self.manual_stretch.mid,
                    white: self.manual_stretch.white,
                };
                // Sliders show the mean of the channels; unlinked channels move together.
                let (mut black, mut mid, mut white) = points.master();
                let mut changed = false;

                // Black / White sliders are most sensitive around the frame median (sky
                // background) and logarithmic towards the ends; see `slider_scale`.
                let scale = active_frame.map_or(MedianSliderScale::new(0.1, 0.01), |f| {
                    let lum = &f.histogram.stats[3];
                    MedianSliderScale::new(lum.median_norm, lum.sigma_norm)
                });
                let median_hint = active_frame.map(|f| f.histogram.stats[3].median_norm);

                ui.label("Black:");
                changed |= median_slider(ui, &mut black, scale, median_hint, "black");
                ui.label("Midtone:");
                // Midtone: most sensitive around the automatic (Linked) midtone of this frame.
                let auto_mid = active_frame.map(|f| f.linked_stretch.mid[0]);
                let mid_scale = auto_mid.map_or(MedianSliderScale::new(0.25, 0.05), |m| {
                    MedianSliderScale::new(m, (m * 0.3).max(1e-4))
                });
                if median_slider(ui, &mut mid, mid_scale, None, "midtone") {
                    mid = mid.clamp(0.0005, 0.9995);
                    changed = true;
                }
                ui.label("White:");
                changed |= median_slider(ui, &mut white, scale, median_hint, "white");

                let was_linked = self.manual_link_channels;
                ui.checkbox(&mut self.manual_link_channels, "Link RGB")
                    .on_hover_text(
                        "On: the sliders set the same values for R, G and B.\n\
                         Off: each channel keeps its own offset (e.g. from Unlinked auto stretch), \
                         and the sliders move all three together, preserving the colour balance.",
                    );
                if self.manual_link_channels && !was_linked {
                    changed = true; // linking: collapse the channels onto the shown values
                }

                if changed {
                    let updated = points.with_master(black, mid, white, self.manual_link_channels);
                    self.manual_stretch.black = updated.black;
                    self.manual_stretch.mid = updated.mid;
                    self.manual_stretch.white = updated.white;
                    self.stretch_mode = StretchMode::Manual;
                    self.user_overrode_stretch = true;
                }

                if let Some(frame) = active_frame {
                    if ui
                        .button("Copy Auto Linked")
                        .on_hover_text("Start from the automatic stretch with linked channels")
                        .clicked()
                    {
                        self.manual_stretch = frame.linked_stretch;
                        self.manual_link_channels = true;
                        self.stretch_mode = StretchMode::Manual;
                        self.user_overrode_stretch = true;
                    }
                    if ui
                        .button("Copy Auto Unlinked")
                        .on_hover_text(
                            "Start from the automatic stretch with independent R, G, B channels",
                        )
                        .clicked()
                    {
                        self.manual_stretch = frame.unlinked_stretch;
                        self.manual_link_channels = false;
                        self.stretch_mode = StretchMode::Manual;
                        self.user_overrode_stretch = true;
                    }
                }
            });
        }
    }
}

/// A Black / White point slider on the median-centred scale. The slider moves in position
/// space; the number shown (and typed) is the real normalized value. Returns `true` when
/// the value changed.
fn median_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    scale: MedianSliderScale,
    median: Option<f32>,
    id: &str,
) -> bool {
    let mut position = scale.position(f64::from(*value));
    let response = ui
        .push_id(id, |ui| {
            ui.add(
                egui::Slider::new(&mut position, 0.0..=1.0)
                    .custom_formatter(move |s, _| format!("{:.4}", scale.value(s)))
                    .custom_parser(move |text| {
                        text.trim()
                            .replace(',', ".")
                            .parse::<f64>()
                            .ok()
                            .map(|v| scale.position(v))
                    }),
            )
        })
        .inner;
    let response = match median {
        Some(m) => response.on_hover_text(format!(
            "Fine control around the sky background (median {m:.4}); coarser towards 0 and 1.
You can also type an exact value."
        )),
        None => response,
    };
    if response.changed() {
        *value = scale.value(position) as f32;
        true
    } else {
        false
    }
}
