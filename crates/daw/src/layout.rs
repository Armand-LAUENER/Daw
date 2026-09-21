//! How big things are.
//!
//! Kept apart from the project on purpose. A fader's width is a preference of
//! whoever is looking at the screen, not something the piece of music holds, so
//! it never becomes a [`Command`](project_model::Command) and never enters the
//! history. Undoing a change of mind about a slider would be nonsense.
//!
//! Adjustable at runtime because the right size is not something to be decided
//! once by argument: it is found by trying, with the mixer in front of you.

use eframe::egui::{self, RichText, Ui};

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// The control reached for most, so the one that most needs travel.
    pub gain_width: f32,
    /// Panning has a centre to find rather than a curve to ride, so it needs
    /// less.
    pub pan_width: f32,
    pub name_width: f32,
    /// Square-ish, so M and S read as switches rather than as words.
    pub switch_width: f32,
    /// Air between strips. Too little and a tall mixer becomes a wall.
    pub strip_spacing: f32,
}

impl Default for Layout {
    fn default() -> Self {
        Self { gain_width: 240.0, pan_width: 150.0, name_width: 110.0, switch_width: 26.0, strip_spacing: 4.0 }
    }
}

impl Layout {
    /// Roomier, for a wide screen.
    pub fn wide() -> Self {
        Self { gain_width: 380.0, pan_width: 220.0, name_width: 140.0, ..Self::default() }
    }

    /// Tighter, for fitting more strips on screen than comfort would suggest.
    pub fn compact() -> Self {
        Self { gain_width: 150.0, pan_width: 100.0, name_width: 90.0, strip_spacing: 2.0, ..Self::default() }
    }
}

/// The controls for changing it, shown when asked for.
pub fn controls(ui: &mut Ui, theme: &Theme, layout: &mut Layout) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("layout").color(theme.dim).small());

        if ui.button("Compact").clicked() {
            *layout = Layout::compact();
        }
        if ui.button("Default").clicked() {
            *layout = Layout::default();
        }
        if ui.button("Wide").clicked() {
            *layout = Layout::wide();
        }

        ui.separator();
        ui.spacing_mut().slider_width = 120.0;
        ui.label(RichText::new("gain").color(theme.dim).small());
        ui.add(egui::Slider::new(&mut layout.gain_width, 80.0..=500.0).show_value(false));
        ui.label(RichText::new("pan").color(theme.dim).small());
        ui.add(egui::Slider::new(&mut layout.pan_width, 60.0..=300.0).show_value(false));
        ui.label(RichText::new("name").color(theme.dim).small());
        ui.add(egui::Slider::new(&mut layout.name_width, 60.0..=260.0).show_value(false));

        ui.label(
            RichText::new(format!("{:.0} / {:.0} / {:.0}", layout.gain_width, layout.pan_width, layout.name_width))
                .color(theme.dim)
                .monospace()
                .small(),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_leaves_the_gain_fader_the_longest_control() {
        // Gain is the one reached for most; a preset that made it the shortest
        // would be a preset nobody wants.
        for layout in [Layout::default(), Layout::wide(), Layout::compact()] {
            assert!(layout.gain_width > layout.pan_width, "{layout:?}");
            assert!(layout.gain_width > layout.name_width, "{layout:?}");
        }
    }

    #[test]
    fn the_presets_are_actually_different() {
        assert!(Layout::compact().gain_width < Layout::default().gain_width);
        assert!(Layout::wide().gain_width > Layout::default().gain_width);
    }
}
