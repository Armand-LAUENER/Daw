//! A track's equaliser: its response drawn across the spectrum, with a handle
//! per band to drag.
//!
//! Writes only the track's bands (decision D5). The curve is computed from the
//! same coefficients the engine builds, so what is drawn is what is heard; the
//! frequency axis is logarithmic and the gain axis in decibels, because that is
//! how both are heard.

use eframe::egui::{self, Align2, FontId, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2};
use project_engine::band_gain_db;
use project_model::{BANDS, BandShape, Command, EqBand, Project, TrackId};

use crate::theme::Theme;
use crate::ui::Actions;

const LOWEST: f32 = 20.0;
const HIGHEST: f32 = 20_000.0;
/// The gain axis runs from minus this to plus this, in dB.
const RANGE_DB: f32 = 24.0;
const HANDLE: f32 = 7.0;

/// Where a frequency is across a width, on a logarithmic axis.
pub fn x_of_freq(freq: f32, width: f32) -> f32 {
    (freq.clamp(LOWEST, HIGHEST) / LOWEST).log10() / (HIGHEST / LOWEST).log10() * width
}

/// The frequency at a point across a width.
pub fn freq_at_x(x: f32, width: f32) -> f32 {
    LOWEST * (HIGHEST / LOWEST).powf((x / width.max(1.0)).clamp(0.0, 1.0))
}

/// Where a gain is down a height: +24 dB at the top, -24 at the bottom.
pub fn y_of_db(db: f32, height: f32) -> f32 {
    (0.5 - db.clamp(-RANGE_DB, RANGE_DB) / (2.0 * RANGE_DB)) * height
}

/// The gain at a point down a height.
pub fn db_at_y(y: f32, height: f32) -> f32 {
    ((0.5 - y / height.max(1.0)) * 2.0 * RANGE_DB).clamp(-RANGE_DB, RANGE_DB)
}

/// Every band's effect at a frequency, added up: bands in series multiply,
/// which in decibels is a sum.
pub fn response_db(bands: &[EqBand], freq: f32, sample_rate: f32) -> f32 {
    bands.iter().map(|b| band_gain_db(b, freq, sample_rate)).sum()
}

/// Draws the equaliser of `track`. Returns false once it should close — the
/// track is gone, or the close button was pressed.
pub fn show(
    ui: &mut Ui,
    project: &Project,
    track: TrackId,
    theme: &Theme,
    sample_rate: f32,
    actions: &mut Actions,
) -> bool {
    let Some(t) = project.track(track) else { return false };
    let mut open = true;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("EQ — {}", t.name)).color(theme.text).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close").clicked() {
                open = false;
            }
            ui.label(
                egui::RichText::new("drag a handle: frequency and gain · wheel on it: width").color(theme.dim).small(),
            );
        });
    });

    // The controls take their rows first; the curve gets what is left.
    let controls_height = BANDS as f32 * 24.0 + 8.0;
    // Never so short that the 6 dB lines crowd each other.
    let curve_height = (ui.available_height() - controls_height).max(180.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), curve_height), Sense::hover());
    draw_curve(ui, rect, &t.eq, theme, sample_rate);
    handles(ui, rect, track, &t.eq, theme, actions);

    ui.add_space(6.0);
    for (index, band) in t.eq.iter().enumerate() {
        band_controls(ui, track, index, band, theme, actions);
    }
    open
}

fn draw_curve(ui: &Ui, rect: Rect, bands: &[EqBand], theme: &Theme, sample_rate: f32) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.background);

    // Octave-ish lines, labelled, and gain lines every 6 dB.
    for freq in [50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0] {
        let x = rect.left() + x_of_freq(freq, rect.width());
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.0, theme.panel));
        let label = if freq >= 1_000.0 { format!("{}k", freq / 1_000.0) } else { format!("{freq}") };
        painter.text(
            Pos2::new(x + 2.0, rect.bottom() - 2.0),
            Align2::LEFT_BOTTOM,
            label,
            FontId::proportional(10.0),
            theme.dim,
        );
    }
    for db in [-18.0, -12.0, -6.0, 0.0, 6.0, 12.0, 18.0] {
        let y = rect.top() + y_of_db(db, rect.height());
        let colour = if db == 0.0 { theme.control } else { theme.panel };
        painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], Stroke::new(1.0, colour));
        painter.text(
            Pos2::new(rect.left() + 2.0, y - 1.0),
            Align2::LEFT_BOTTOM,
            format!("{db:+}"),
            FontId::proportional(10.0),
            theme.dim,
        );
    }

    // The response, one point every two pixels.
    let points: Vec<Pos2> = (0..=(rect.width() as usize / 2))
        .map(|i| {
            let x = (i * 2) as f32;
            let db = response_db(bands, freq_at_x(x, rect.width()), sample_rate);
            Pos2::new(rect.left() + x, rect.top() + y_of_db(db, rect.height()))
        })
        .collect();
    painter.add(Shape::line(points, Stroke::new(2.0, theme.accent)));
}

fn handles(ui: &Ui, rect: Rect, track: TrackId, bands: &[EqBand], theme: &Theme, actions: &mut Actions) {
    let painter = ui.painter_at(rect);
    for (index, band) in bands.iter().enumerate() {
        let gain = if band.shape.has_gain() { band.gain } else { 0.0 };
        let centre = rect.left_top() + Vec2::new(x_of_freq(band.freq, rect.width()), y_of_db(gain, rect.height()));
        let colour = if band.on { theme.accent } else { theme.muted };
        painter.circle_filled(centre, HANDLE, theme.panel);
        painter.circle_stroke(centre, HANDLE, Stroke::new(2.0, colour));
        painter.text(centre, Align2::CENTER_CENTER, (index + 1).to_string(), FontId::proportional(10.0), theme.text);

        let area = Rect::from_center_size(centre, Vec2::splat(HANDLE * 2.5));
        let response = ui.interact(area, ui.id().with(("eq-band", index)), Sense::drag());
        let mut value = *band;
        if response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
        {
            value.freq = freq_at_x(pos.x - rect.left(), rect.width()).clamp(*EqBand::FREQ.start(), *EqBand::FREQ.end());
            if band.shape.has_gain() {
                value.gain = db_at_y(pos.y - rect.top(), rect.height());
            }
        }
        // The wheel over a handle narrows or widens it.
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                value.q = (band.q * (scroll * 0.005).exp()).clamp(*EqBand::Q.start(), *EqBand::Q.end());
            }
        }
        if value != *band {
            // Folded into one step per band until the handle is let go.
            actions.commands.push(Command::SetEqBand { track, band: index as u8, value });
        }
        if response.drag_stopped() {
            actions.seal = true;
        }
    }
}

fn band_controls(ui: &mut Ui, track: TrackId, index: usize, band: &EqBand, theme: &Theme, actions: &mut Actions) {
    let mut value = *band;
    let mut sealed = false;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{}", index + 1)).color(theme.dim).monospace());
        ui.checkbox(&mut value.on, "");
        egui::ComboBox::from_id_salt(("eq-shape", track.0, index))
            .width(86.0)
            .selected_text(band.shape.name())
            .show_ui(ui, |ui| {
                for shape in BandShape::ALL {
                    ui.selectable_value(&mut value.shape, shape, shape.name());
                }
            });
        let freq = ui.add(
            egui::DragValue::new(&mut value.freq)
                .range(EqBand::FREQ)
                .speed(band.freq * 0.01)
                .max_decimals(0)
                .suffix(" Hz"),
        );
        let gain = ui.add_enabled(
            band.shape.has_gain(),
            egui::DragValue::new(&mut value.gain).range(EqBand::GAIN).speed(0.1).max_decimals(1).suffix(" dB"),
        );
        let q = ui.add(egui::DragValue::new(&mut value.q).range(EqBand::Q).speed(0.01).max_decimals(2).prefix("Q "));
        sealed = [freq, gain, q].iter().any(|r| r.drag_stopped() || r.lost_focus());
    });
    if value != *band {
        actions.commands.push(Command::SetEqBand { track, band: index as u8, value });
    }
    if sealed {
        actions.seal = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frequency_and_its_position_agree_both_ways() {
        for freq in [20.0f32, 100.0, 1_000.0, 20_000.0] {
            let x = x_of_freq(freq, 600.0);
            assert!((freq_at_x(x, 600.0) - freq).abs() / freq < 1e-4, "{freq} Hz");
        }
        // Log: each decade takes the same width.
        let decade = x_of_freq(200.0, 600.0) - x_of_freq(20.0, 600.0);
        assert!((x_of_freq(2_000.0, 600.0) - x_of_freq(200.0, 600.0) - decade).abs() < 1e-3);
    }

    #[test]
    fn a_gain_and_its_height_agree_both_ways() {
        assert_eq!(y_of_db(0.0, 200.0), 100.0);
        assert_eq!(y_of_db(24.0, 200.0), 0.0);
        for db in [-24.0f32, -6.0, 0.0, 12.5] {
            assert!((db_at_y(y_of_db(db, 200.0), 200.0) - db).abs() < 1e-4);
        }
    }

    #[test]
    fn the_drawn_response_adds_the_bands() {
        let mut bands = project_model::default_eq();
        assert!(response_db(&bands, 1_000.0, 48_000.0).abs() < 1e-4, "the default is not flat");
        bands[1].gain = 6.0;
        bands[2].gain = 6.0;
        bands[2].freq = 500.0;
        assert!((response_db(&bands, 500.0, 48_000.0) - 12.0).abs() < 0.05);
        bands[2].on = false;
        assert!((response_db(&bands, 500.0, 48_000.0) - 6.0).abs() < 0.05, "a band that is off was drawn");
    }
}
