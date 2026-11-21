//! Asteroid / comet identification overlay: a spinner while the online query runs, then the
//! 25′ search circle and the identified objects with name, magnitude and motion arrow.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use fulgur_engine::sky::small_bodies::SmallBody;
use std::time::Instant;

/// What the user asked for: the clicked sky position (and its pixel position).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SmallBodyRequest {
    pub x_px: f64,
    pub y_px: f64,
    pub ra_deg: f64,
    pub dec_deg: f64,
}

/// State of a search, as drawn on the image.
pub enum SmallBodyState {
    Searching {
        started: Instant,
    },
    /// Objects with their pixel position on this frame (`None` when off the frame).
    Found(Vec<(SmallBody, Option<[f64; 2]>)>),
    Failed(String),
}

/// Everything the viewer needs to draw one search.
pub struct SmallBodyOverlay {
    pub center_px: [f64; 2],
    pub radius_px: f64,
    pub state: SmallBodyState,
}

const ASTEROID_COLOR: Color32 = Color32::from_rgb(120, 230, 255);
const COMET_COLOR: Color32 = Color32::from_rgb(140, 255, 150);
const CIRCLE_COLOR: Color32 = Color32::from_rgb(255, 210, 120);

/// Motion arrow on screen: the projected motion over one hour, lengthened to at least 14
/// points so the direction of slow objects stays readable.
pub fn readable_arrow(one_hour: Vec2) -> Vec2 {
    let len = one_hour.length();
    if len < 1e-6 {
        Vec2::ZERO
    } else if len < 14.0 {
        one_hour * (14.0 / len)
    } else {
        one_hour
    }
}

/// Draw the search. `to_screen` maps native pixels to screen points; `motion_dir` returns the
/// screen vector of one hour of the body's motion (projected through the WCS).
pub fn draw_small_bodies(
    painter: &egui::Painter,
    viewport: Rect,
    overlay: &SmallBodyOverlay,
    points_per_pixel: f32,
    to_screen: impl Fn([f64; 2]) -> Pos2,
    motion_dir: impl Fn(&SmallBody, [f64; 2]) -> Option<Vec2>,
) {
    let centre = to_screen(overlay.center_px);
    let radius = (overlay.radius_px as f32 * points_per_pixel).max(6.0);

    match &overlay.state {
        SmallBodyState::Searching { started } => {
            // Rotating arc at the clicked position until the query finishes.
            let t = started.elapsed().as_secs_f32();
            let r = 18.0;
            let start = t * 5.0;
            let points: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let a = start + i as f32 / 24.0 * std::f32::consts::PI * 1.5;
                    centre + Vec2::new(a.cos(), a.sin()) * r
                })
                .collect();
            painter.add(egui::Shape::line(
                points,
                Stroke::new(3.0_f32, CIRCLE_COLOR),
            ));
            painter.text(
                centre + Vec2::new(0.0, r + 8.0),
                Align2::CENTER_TOP,
                "Searching asteroids & comets…",
                FontId::proportional(12.0),
                CIRCLE_COLOR,
            );
        }
        SmallBodyState::Failed(message) => {
            dashed_circle(painter, centre, radius, Color32::from_rgb(255, 120, 110));
            painter.text(
                centre + Vec2::new(0.0, 22.0),
                Align2::CENTER_TOP,
                message,
                FontId::proportional(12.0),
                Color32::from_rgb(255, 150, 140),
            );
        }
        SmallBodyState::Found(bodies) => {
            dashed_circle(painter, centre, radius, CIRCLE_COLOR);
            painter.circle_stroke(centre, 4.0, Stroke::new(1.5_f32, CIRCLE_COLOR));
            painter.text(
                centre + Vec2::new(radius * 0.71 + 4.0, -radius * 0.71 - 4.0),
                Align2::LEFT_BOTTOM,
                format!(
                    "25′ · {} object{}",
                    bodies.len(),
                    if bodies.len() == 1 { "" } else { "s" }
                ),
                FontId::proportional(12.0),
                CIRCLE_COLOR,
            );
            for (body, px) in bodies {
                let Some(px) = px else { continue };
                let pos = to_screen(*px);
                if !viewport.expand(40.0).contains(pos) {
                    continue;
                }
                let color = if body.is_comet {
                    COMET_COLOR
                } else {
                    ASTEROID_COLOR
                };
                painter.circle_stroke(pos, 9.0, Stroke::new(1.6_f32, color));
                if let Some(dir) = motion_dir(body, *px).map(readable_arrow)
                    && dir != Vec2::ZERO
                {
                    painter.arrow(pos, dir, Stroke::new(1.4_f32, color));
                }
                let label = match body.vmag {
                    Some(v) => format!("{}  V{v:.1}", body.name),
                    None => body.name.clone(),
                };
                let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), color);
                let at = pos + Vec2::new(12.0, -galley.size().y - 4.0);
                let bg = Rect::from_min_size(
                    at - Vec2::new(3.0, 1.0),
                    galley.size() + Vec2::new(6.0, 2.0),
                );
                painter.rect_filled(bg, 3.0, Color32::from_black_alpha(150));
                painter.galley(at, galley, color);
            }
        }
    }
}

fn dashed_circle(painter: &egui::Painter, centre: Pos2, radius: f32, color: Color32) {
    let segments = 72;
    for i in (0..segments).step_by(2) {
        let a0 = i as f32 / segments as f32 * std::f32::consts::TAU;
        let a1 = (i + 1) as f32 / segments as f32 * std::f32::consts::TAU;
        painter.line_segment(
            [
                centre + Vec2::new(a0.cos(), a0.sin()) * radius,
                centre + Vec2::new(a1.cos(), a1.sin()) * radius,
            ],
            Stroke::new(1.5_f32, color),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_keep_direction_and_a_readable_minimum_length() {
        let fast = Vec2::new(30.0, -40.0);
        assert_eq!(readable_arrow(fast), fast);
        let slow = readable_arrow(Vec2::new(0.0, 2.0));
        assert!((slow.length() - 14.0).abs() < 1e-4 && slow.x == 0.0 && slow.y > 0.0);
        assert_eq!(readable_arrow(Vec2::ZERO), Vec2::ZERO);
    }
}
