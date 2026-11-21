//! Metadata panel: header search, summary and clipboard export.

use super::*;

impl FulgurPreViewApp {
    pub(super) fn show_metadata_panel(&mut self, ui: &mut egui::Ui, frame: &OverviewFrame) {
        let info = &frame.info;
        let panel_w = ui.available_width().max(120.0);
        ui.set_max_width(panel_w);

        // Scale font sizes smoothly when the user narrows the panel
        let font_size = (panel_w * 0.048).clamp(9.5, 12.5);
        let heading_size = (panel_w * 0.062).clamp(12.0, 16.0);
        let row_h = (font_size + 5.0).round();

        ui.label(
            RichText::new("Metadata Inspector")
                .size(heading_size)
                .strong(),
        );
        ui.add_space(3.0);

        // Multi-Extension FITS (MEF) HDU selector if multiple image HDUs exist
        if frame.hdu_names.len() > 1 {
            ui.group(|ui| {
                ui.set_max_width((panel_w - 16.0).max(100.0));
                ui.label(
                    RichText::new("MEF Extension (HDU):")
                        .size(font_size)
                        .strong(),
                );
                egui::ComboBox::from_id_salt("mef_hdu_selector")
                    .selected_text(
                        frame
                            .hdu_names
                            .get(self.hdu_index)
                            .cloned()
                            .unwrap_or_else(|| format!("HDU #{}", self.hdu_index)),
                    )
                    .width((panel_w - 28.0).max(90.0))
                    .show_ui(ui, |ui| {
                        for (idx, hdu_label) in frame.hdu_names.iter().enumerate() {
                            if ui
                                .selectable_label(self.hdu_index == idx, hdu_label)
                                .clicked()
                            {
                                self.hdu_index = idx;
                                self.sync_active_request();
                            }
                        }
                    });
            });
            ui.add_space(4.0);
        }

        // Summary Card with proportional label & value columns that shrink with the panel
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(13, 40, 40))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(38, 84, 84)))
            .show(ui, |ui| {
                let inner_w = (panel_w - 20.0).max(100.0);
                ui.set_max_width(inner_w);
                ui.label(
                    RichText::new("Observation Summary")
                        .size((font_size + 0.5).min(13.0))
                        .strong()
                        .color(Color32::from_rgb(130, 190, 255)),
                );
                ui.add_space(2.0);

                let label_w = (inner_w * 0.40).max(45.0);
                let val_w = (inner_w - label_w - 8.0).max(45.0);

                let mut summary_rows: Vec<(&str, String)> = Vec::with_capacity(14);
                summary_rows.push(("Format", frame.format_label.to_owned()));
                summary_rows.push(("Dimensions", format!("{} × {} px", info.width, info.height)));
                summary_rows.push(("BITPIX", format!("{}", info.bitpix)));
                let mosaic_str = match (info.mosaic, info.bayer_pattern) {
                    (Mosaic::Bayer, Some(pat)) => {
                        format!("Bayer {} ({})", pat.name(), info.mosaic_source)
                    }
                    (Mosaic::Rgb, _) => "RGB (3-channel)".to_owned(),
                    (Mosaic::Mono, _) => "Monochrome".to_owned(),
                    _ => "Unknown".to_owned(),
                };
                summary_rows.push(("Sensor Layout", mosaic_str));
                if let Some(obj) = &info.object {
                    summary_rows.push(("Object / Target", obj.clone()));
                }
                if let Some(inst) = &info.instrument {
                    summary_rows.push(("Camera / Sensor", inst.clone()));
                }
                if let Some(tel) = &info.telescope {
                    summary_rows.push(("Telescope / Lens", tel.clone()));
                }
                if let Some(filt) = &info.filter {
                    summary_rows.push(("Filter", filt.clone()));
                }
                if info.exposure_seconds > 0.0 {
                    summary_rows.push(("Exposure", format!("{:.3} s", info.exposure_seconds)));
                }
                if let Some(gain) = info.gain {
                    summary_rows.push(("Gain / ISO", format!("{gain:.1}")));
                }
                if let Some(temp) = info.ccd_temperature {
                    summary_rows.push(("Sensor Temp", format!("{temp:.1} °C")));
                }
                if let Some(creator) = &info.creator {
                    summary_rows.push(("Software", creator.clone()));
                }
                if let Some(date) = &info.date_obs {
                    summary_rows.push(("Date-Obs (UTC)", date.clone()));
                }

                for (idx, (label, val)) in summary_rows.into_iter().enumerate() {
                    let bg = if idx % 2 == 1 {
                        Color32::from_rgb(18, 52, 52)
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::NONE.fill(bg).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.allocate_ui_with_layout(
                                egui::vec2(label_w, row_h),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    add_truncated_label(
                                        ui,
                                        RichText::new(label).size(font_size).weak(),
                                        label_w,
                                        label,
                                    );
                                },
                            );
                            ui.allocate_ui_with_layout(
                                egui::vec2(val_w, row_h),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    add_truncated_label(
                                        ui,
                                        RichText::new(&val).size(font_size).monospace(),
                                        val_w,
                                        &val,
                                    );
                                },
                            );
                        });
                    });
                }
            });

        ui.add_space(5.0);

        // Search filter and privacy controls
        ui.horizontal(|ui| {
            ui.set_max_width(panel_w);
            let edit_w = if self.metadata_filter.is_empty() {
                (panel_w - 14.0).max(60.0)
            } else {
                (panel_w - 38.0).max(60.0)
            };
            ui.add(
                egui::TextEdit::singleline(&mut self.metadata_filter)
                    .hint_text("Filter keys or values...")
                    .desired_width(edit_w),
            );
            if !self.metadata_filter.is_empty() && ui.small_button("X").clicked() {
                self.metadata_filter.clear();
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.checkbox(
                &mut self.show_gps_metadata,
                RichText::new("Show GPS").size(font_size),
            )
            .on_hover_text("Reveal SITELAT / SITELONG observatory coordinates");
            if ui
                .button(RichText::new("Copy Header").size(font_size))
                .on_hover_text("Copy filtered metadata cards to clipboard")
                .clicked()
            {
                let text = self.format_filtered_cards_for_clipboard(frame);
                ui.ctx().copy_text(text);
                self.status_banner = Some((
                    "Copied metadata cards to clipboard.".to_owned(),
                    Instant::now(),
                ));
            }
        });

        ui.separator();

        // Scrollable 2-column table (Key + Value) with full description in the Key tooltip
        let query = self.metadata_filter.trim().to_ascii_lowercase();
        let cards: Vec<_> = frame
            .cards
            .iter()
            .filter(|c| {
                if !self.show_gps_metadata && is_gps_keyword(&c.keyword) {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                c.keyword.to_ascii_lowercase().contains(&query)
                    || c.value.to_ascii_lowercase().contains(&query)
                    || c.comment.to_ascii_lowercase().contains(&query)
            })
            .collect();

        ui.label(
            RichText::new(format!(
                "Showing {} / {} header cards",
                cards.len(),
                frame.cards.len()
            ))
            .size((font_size - 1.0).max(9.0))
            .weak(),
        );

        let table_w = (panel_w - 22.0).max(95.0);
        let key_w = (table_w * 0.36).clamp(44.0, 125.0);
        let val_w = (table_w - key_w - 6.0).max(40.0);

        // Table header row
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.allocate_ui_with_layout(
                egui::vec2(key_w, row_h),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_max_width(key_w);
                    ui.add(
                        egui::Label::new(RichText::new("Key").size(font_size).strong()).truncate(),
                    );
                },
            );
            ui.allocate_ui_with_layout(
                egui::vec2(val_w, row_h),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_max_width(val_w);
                    ui.add(
                        egui::Label::new(RichText::new("Value").size(font_size).strong())
                            .truncate(),
                    );
                },
            );
        });

        let cards_scroll_h = ui.available_height().max(260.0);
        egui::ScrollArea::vertical()
            .id_salt("metadata_cards_scroll")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .max_height(cards_scroll_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(table_w);
                for (idx, card) in cards.into_iter().enumerate() {
                    let bg = if idx % 2 == 0 {
                        Color32::from_rgb(18, 52, 52)
                    } else {
                        Color32::TRANSPARENT
                    };
                    egui::Frame::NONE.fill(bg).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;

                            // 1. Key column (shrinks proportionally with panel; shows description tooltip on hover)
                            ui.allocate_ui_with_layout(
                                egui::vec2(key_w, row_h),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    let tip = if card.comment.is_empty() {
                                        card.keyword.clone()
                                    } else {
                                        format!("{}: {}", card.keyword, card.comment)
                                    };
                                    add_truncated_label(
                                        ui,
                                        RichText::new(&card.keyword)
                                            .size(font_size)
                                            .monospace()
                                            .color(Color32::from_rgb(160, 210, 255)),
                                        key_w,
                                        &tip,
                                    );
                                },
                            );

                            // 2. Value column (shrinks proportionally with panel; single non-duplicated tooltip)
                            ui.allocate_ui_with_layout(
                                egui::vec2(val_w, row_h),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    add_truncated_label(
                                        ui,
                                        RichText::new(&card.value).size(font_size).monospace(),
                                        val_w,
                                        &card.value,
                                    );
                                },
                            );
                        });
                    });
                }
            });
    }

    pub(super) fn format_filtered_cards_for_clipboard(&self, frame: &OverviewFrame) -> String {
        let query = self.metadata_filter.trim().to_ascii_lowercase();
        let mut out = String::new();
        for card in &frame.cards {
            if !self.show_gps_metadata && is_gps_keyword(&card.keyword) {
                continue;
            }
            if !query.is_empty()
                && !card.keyword.to_ascii_lowercase().contains(&query)
                && !card.value.to_ascii_lowercase().contains(&query)
                && !card.comment.to_ascii_lowercase().contains(&query)
            {
                continue;
            }
            if card.comment.is_empty() {
                out.push_str(&format!("{:<10} = {}\n", card.keyword, card.value));
            } else {
                out.push_str(&format!(
                    "{:<10} = {:<24} / {}\n",
                    card.keyword, card.value, card.comment
                ));
            }
        }
        out
    }
}
