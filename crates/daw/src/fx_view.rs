//! A track's compressor, echo and room, side by side.
//!
//! Writes only those three settings of one track (decision D5). Every control
//! sends a command while it moves and seals the step when it is let go, so a
//! sweep of a slider is one undo.

use std::ops::RangeInclusive;

use eframe::egui::{self, RichText, Ui};
use project_model::{Command, CompSettings, DelaySettings, PPQ, Project, ReverbSettings, Tick, TrackId};

use crate::theme::Theme;
use crate::ui::Actions;

/// The echo times offered, as note values. Musical rather than in
/// milliseconds, so the echo stays on the beat whatever the tempo.
const DIVISIONS: [(&str, u64); 8] = [
    ("1/16", PPQ / 4),
    ("1/8 triplet", PPQ / 3),
    ("1/8", PPQ / 2),
    ("1/4 triplet", PPQ * 2 / 3),
    ("1/8 dotted", PPQ * 3 / 4),
    ("1/4", PPQ),
    ("1/4 dotted", PPQ * 3 / 2),
    ("1/2", PPQ * 2),
];

/// The name of an echo time, or its length in ticks when it is none of the
/// offered ones — as a file edited by hand might hold.
pub fn division_name(time: Tick) -> String {
    DIVISIONS.iter().find(|(_, t)| *t == time.0).map_or_else(|| format!("{} ticks", time.0), |(n, _)| (*n).to_string())
}

/// Draws the effects of `track`. Returns false once it should close.
pub fn show(ui: &mut Ui, project: &Project, track: TrackId, theme: &Theme, actions: &mut Actions) -> bool {
    let Some(t) = project.track(track) else { return false };
    let mut open = true;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Effects — {}", t.name)).color(theme.text).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close").clicked() {
                open = false;
            }
            ui.label(RichText::new("in order: compressor, then delay, then reverb").color(theme.dim).small());
        });
    });
    ui.separator();
    ui.columns(3, |columns| {
        compressor(&mut columns[0], track, t.comp, theme, actions);
        delay(&mut columns[1], track, t.delay, theme, actions);
        reverb(&mut columns[2], track, t.reverb, theme, actions);
    });
    open
}

/// A labelled slider. Returns whether it moved, and whether it was let go.
fn slider(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    log: bool,
    suffix: &str,
) -> (bool, bool) {
    let mut out = (false, false);
    ui.horizontal(|ui| {
        ui.add_sized([64.0, 18.0], egui::Label::new(label));
        let response = ui.add(egui::Slider::new(value, range).logarithmic(log).suffix(suffix));
        out = (response.changed(), response.drag_stopped() || response.lost_focus());
    });
    out
}

fn heading(ui: &mut Ui, name: &str, on: &mut bool, theme: &Theme) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        changed = ui.checkbox(on, "").changed();
        let colour = if *on { theme.accent } else { theme.dim };
        ui.label(RichText::new(name).color(colour).strong());
    });
    changed
}

fn compressor(ui: &mut Ui, track: TrackId, was: CompSettings, theme: &Theme, actions: &mut Actions) {
    let mut c = was;
    let toggled = heading(ui, "Compressor", &mut c.on, theme);
    let moves = [
        slider(ui, "threshold", &mut c.threshold, CompSettings::THRESHOLD, false, " dB"),
        slider(ui, "ratio", &mut c.ratio, CompSettings::RATIO, true, ":1"),
        slider(ui, "attack", &mut c.attack, CompSettings::ATTACK, true, " ms"),
        slider(ui, "release", &mut c.release, CompSettings::RELEASE, true, " ms"),
        slider(ui, "makeup", &mut c.makeup, CompSettings::MAKEUP, false, " dB"),
    ];
    send(actions, toggled, &moves, c != was, || Command::SetCompressor { track, value: c });
}

fn delay(ui: &mut Ui, track: TrackId, was: DelaySettings, theme: &Theme, actions: &mut Actions) {
    let mut d = was;
    let toggled = heading(ui, "Delay", &mut d.on, theme);
    let mut picked = false;
    ui.horizontal(|ui| {
        ui.add_sized([64.0, 18.0], egui::Label::new("time"));
        egui::ComboBox::from_id_salt(("delay-time", track.0)).selected_text(division_name(d.time)).show_ui(ui, |ui| {
            for (name, ticks) in DIVISIONS {
                picked |= ui.selectable_value(&mut d.time, Tick(ticks), name).clicked();
            }
        });
    });
    let moves = [
        slider(ui, "feedback", &mut d.feedback, DelaySettings::FEEDBACK, false, ""),
        slider(ui, "mix", &mut d.mix, DelaySettings::MIX, false, ""),
    ];
    send(actions, toggled || picked, &moves, d != was, || Command::SetDelay { track, value: d });
}

fn reverb(ui: &mut Ui, track: TrackId, was: ReverbSettings, theme: &Theme, actions: &mut Actions) {
    let mut r = was;
    let toggled = heading(ui, "Reverb", &mut r.on, theme);
    let moves = [
        slider(ui, "size", &mut r.size, ReverbSettings::UNIT, false, ""),
        slider(ui, "damping", &mut r.damping, ReverbSettings::UNIT, false, ""),
        slider(ui, "mix", &mut r.mix, ReverbSettings::UNIT, false, ""),
    ];
    send(actions, toggled, &moves, r != was, || Command::SetReverb { track, value: r });
}

/// Sends the change, if there is one. A switch or a pick is a decision of its
/// own, sealed at once; a slider seals when it is let go.
fn send(
    actions: &mut Actions,
    decided: bool,
    moves: &[(bool, bool)],
    changed: bool,
    command: impl FnOnce() -> Command,
) {
    if changed {
        actions.commands.push(command());
    }
    if decided || moves.iter().any(|(_, released)| *released) {
        actions.seal = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_time_is_one_the_model_accepts() {
        for (name, ticks) in DIVISIONS {
            let delay = DelaySettings { time: Tick(ticks), ..DelaySettings::default() };
            assert!(delay.is_valid(), "{name} is refused");
            assert_eq!(division_name(Tick(ticks)), name);
        }
        assert_eq!(division_name(Tick(123)), "123 ticks");
    }
}
