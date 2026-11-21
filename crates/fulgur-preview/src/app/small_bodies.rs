//! Asteroid / comet identification: runs the online query in the background, keeps one
//! search per frame, builds the image overlay and the result list in the Astrometry panel.

use super::*;
use crate::jobs::{Job, JobPoll};
use crate::small_body_view::{SmallBodyOverlay, SmallBodyRequest, SmallBodyState};
use fulgur_engine::sky::small_bodies::{
    LocationSource, SmallBody, SmallBodyQuery, query_small_bodies,
};

/// One search around a clicked position of one frame.
pub(super) struct SmallBodySearch {
    request: SmallBodyRequest,
    query: Option<SmallBodyQuery>,
    started: Instant,
    job: Option<Job<CacheKey, fulgur_core::Result<Vec<SmallBody>>>>,
    outcome: Option<Result<Vec<SmallBody>, String>>,
}

impl FulgurPreViewApp {
    /// Start a search for `request` on frame `key` (replaces an earlier search on that frame).
    pub(super) fn start_small_body_search(
        &mut self,
        key: CacheKey,
        request: SmallBodyRequest,
        frame: &OverviewFrame,
        plate: Option<&PlateSolveReport>,
    ) {
        let started = Instant::now();
        let mut search = SmallBodySearch {
            request,
            query: None,
            started,
            job: None,
            outcome: None,
        };
        if plate.is_none() {
            search.outcome = Some(Err("Plate-solve the frame first (S).".to_owned()));
        } else {
            match SmallBodyQuery::from_header(request.ra_deg, request.dec_deg, &frame.cards) {
                Ok(query) => {
                    let worker_query = query.clone();
                    search.job = Some(Job::spawn(
                        key.clone(),
                        "fulgur-small-bodies",
                        1,
                        move |cancel, tx| {
                            let _ = tx.send(query_small_bodies(&worker_query, cancel));
                        },
                    ));
                    search.query = Some(query);
                }
                Err(err) => search.outcome = Some(Err(err.to_string())),
            }
        }
        self.show_astrometry = true;
        self.small_body_searches.insert(key, search);
    }

    /// Collect finished searches. Returns `true` when something changed.
    pub(super) fn poll_small_body_searches(&mut self) -> bool {
        let mut changed = false;
        for search in self.small_body_searches.values_mut() {
            let Some(job) = &search.job else { continue };
            let outcome = match job.poll() {
                JobPoll::Pending => continue,
                JobPoll::Done(result) => result.map_err(|e| e.to_string()),
                JobPoll::Stopped => {
                    Err("The search stopped unexpectedly (see crash.log).".to_owned())
                }
            };
            search.outcome = Some(outcome);
            search.job = None;
            changed = true;
        }
        changed
    }

    /// Overlay for the active frame's search, with object positions projected through `plate`.
    pub(super) fn small_body_overlay(
        &self,
        key: &CacheKey,
        plate: Option<&PlateSolveReport>,
    ) -> Option<SmallBodyOverlay> {
        let search = self.small_body_searches.get(key)?;
        let scale = plate.map(|p| p.wcs.scale()).filter(|s| *s > 0.0)?;
        let radius_arcsec = search
            .query
            .as_ref()
            .map_or(25.0 * 60.0, |q| q.radius_arcmin * 60.0);
        let state = match &search.outcome {
            None => SmallBodyState::Searching {
                started: search.started,
            },
            Some(Err(message)) => SmallBodyState::Failed(message.clone()),
            Some(Ok(bodies)) => SmallBodyState::Found(
                bodies
                    .iter()
                    .map(|b| {
                        let px = plate.and_then(|p| p.wcs.sky_to_pixel(b.ra_deg, b.dec_deg));
                        (b.clone(), px)
                    })
                    .collect(),
            ),
        };
        Some(SmallBodyOverlay {
            center_px: [search.request.x_px, search.request.y_px],
            radius_px: radius_arcsec / scale,
            state,
        })
    }

    /// Result list in the Astrometry panel for the active frame.
    pub(super) fn show_small_body_results(
        &mut self,
        ui: &mut egui::Ui,
        panel_w: f32,
        font_size: f32,
    ) {
        let Some(key) = self.current_cache_key() else {
            return;
        };
        let Some(search) = self.small_body_searches.get(&key) else {
            return;
        };
        ui.add_space(6.0);
        let mut clear = false;
        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(panel_w);
            ui.label(
                RichText::new("Asteroids & Comets")
                    .size(font_size + 0.5)
                    .strong(),
            );
            if ui
                .small_button(RichText::new("Clear").size(font_size - 0.5))
                .on_hover_text("Remove the search circle and results from this frame")
                .clicked()
            {
                clear = true;
            }
        });
        if let Some(q) = &search.query {
            let place = match q.observer.source {
                LocationSource::Header => format!(
                    "header {:.1}°, {:.1}° (rounded)",
                    q.observer.lat_deg, q.observer.lon_deg
                ),
                LocationSource::DefaultPrague => {
                    "Prague (no SITELAT/SITELONG in header)".to_owned()
                }
            };
            ui.label(
                RichText::new(format!(
                    "{} UTC · {:.0}′ · V ≤ {:.0} · {place}",
                    q.obs_time_utc.replace('_', " "),
                    q.radius_arcmin,
                    q.vmag_limit
                ))
                .size(font_size - 0.5)
                .weak(),
            );
        }
        match &search.outcome {
            None => {
                ui.label(
                    RichText::new(format!(
                        "Searching… {:.0} s",
                        search.started.elapsed().as_secs_f32()
                    ))
                    .size(font_size)
                    .color(Color32::from_rgb(255, 210, 120)),
                );
            }
            Some(Err(message)) => {
                ui.label(
                    RichText::new(message)
                        .size(font_size)
                        .color(Color32::from_rgb(255, 140, 130)),
                );
            }
            Some(Ok(bodies)) if bodies.is_empty() => {
                ui.label(
                    RichText::new("No known asteroids or comets in this area.")
                        .size(font_size)
                        .weak(),
                );
            }
            Some(Ok(bodies)) => {
                egui::Grid::new("small_body_grid")
                    .num_columns(4)
                    .spacing([8.0, 1.0])
                    .show(ui, |ui| {
                        for head in ["Object", "V", "Dist", "Motion"] {
                            ui.label(RichText::new(head).size(font_size - 0.5).weak());
                        }
                        ui.end_row();
                        for b in bodies {
                            let color = if b.is_comet {
                                Color32::from_rgb(140, 255, 150)
                            } else {
                                Color32::from_rgb(120, 230, 255)
                            };
                            ui.label(RichText::new(&b.name).size(font_size).color(color));
                            ui.label(
                                RichText::new(
                                    b.vmag.map_or_else(|| "—".to_owned(), |v| format!("{v:.1}")),
                                )
                                .size(font_size)
                                .monospace(),
                            );
                            ui.label(
                                RichText::new(format!("{:.1}′", b.distance_arcsec / 60.0))
                                    .size(font_size)
                                    .monospace(),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{:.0}″/h",
                                    b.ra_rate_arcsec_h.hypot(b.dec_rate_arcsec_h)
                                ))
                                .size(font_size)
                                .monospace(),
                            );
                            ui.end_row();
                        }
                    });
                ui.label(
                    RichText::new("Source: NASA/JPL Small-Body Identification (MPC orbits)")
                        .size(font_size - 1.0)
                        .weak(),
                );
            }
        }
        if clear {
            self.small_body_searches.remove(&key);
        }
    }
}
