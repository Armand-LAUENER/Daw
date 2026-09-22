//! The effects of a track or a bus: compressor, echo and room side by side,
//! and for a track, how much it sends to each bus.
//!
//! Writes only those settings (decision D5). Every control sends a command
//! while it moves and seals the step when it is let go, so a sweep of a slider
//! is one undo.

use std::ops::RangeInclusive;

use eframe::egui::{self, RichText, Ui};
use project_model::{BusId, Command, CompSettings, DelaySettings, PPQ, Project, ReverbSettings, SEND, Tick, TrackId};

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

/// What the panel is showing the effects of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strip {
    Track(TrackId),
    Bus(BusId),
}

/// Draws the effects of a track or a bus. Returns false once it should close.
pub fn show(ui: &mut Ui, project: &Project, strip: Strip, theme: &Theme, actions: &mut Actions) -> bool {
    let (name, comp, delay, reverb) = match strip {
        Strip::Track(id) => match project.track(id) {
            Some(t) => (t.name.clone(), t.comp, t.delay, t.reverb),
            None => return false,
        },
        Strip::Bus(id) => match project.bus(id) {
            Some(b) => (b.name.clone(), b.comp, b.delay, b.reverb),
            None => return false,
        },
    };
    let mut open = true;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Effects — {name}")).color(theme.text).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close").clicked() {
                open = false;
            }
            let order = match strip {
                Strip::Track(_) => "in order: compressor, then delay, then reverb",
                Strip::Bus(_) => "a return: its delay and reverb are wet only",
            };
            ui.label(RichText::new(order).color(theme.dim).small());
        });
    });
    ui.separator();

    let columns = if matches!(strip, Strip::Track(_)) { 4 } else { 3 };
    let (mut c, mut d, mut r) = (comp, delay, reverb);
    let mut sealed = false;
    ui.columns(columns, |cols| {
        sealed |= compressor(&mut cols[0], &mut c, theme);
        sealed |= delay_column(&mut cols[1], &mut d, theme, strip);
        sealed |= reverb_column(&mut cols[2], &mut r, theme);
        if let Strip::Track(id) = strip {
            sends(&mut cols[3], project, id, theme, actions);
        }
    });

    match strip {
        Strip::Track(track) => {
            if c != comp {
                actions.commands.push(Command::SetCompressor { track, value: c });
            }
            if d != delay {
                actions.commands.push(Command::SetDelay { track, value: d });
            }
            if r != reverb {
                actions.commands.push(Command::SetReverb { track, value: r });
            }
        }
        Strip::Bus(id) => {
            if (c, d, r) != (comp, delay, reverb) {
                actions.commands.push(Command::SetBusEffects { id, comp: c, delay: d, reverb: r });
            }
        }
    }
    if sealed {
        actions.seal = true;
    }
    open
}

/// One slider per bus: how much of this track it hears. A checkbox makes or
/// removes the send, so a send at zero and no send at all are different
/// things — the second costs nothing.
fn sends(ui: &mut Ui, project: &Project, track: TrackId, theme: &Theme, actions: &mut Actions) {
    ui.label(RichText::new("Sends").color(theme.text).strong());
    let Some(t) = project.track(track) else { return };
    if project.buses().len() == 0 {
        ui.label(RichText::new("no bus yet — add one in the mixer").color(theme.dim).small());
    }
    for bus in project.buses() {
        let was = t.sends.get(&bus.id).copied();
        let mut on = was.is_some();
        let mut level = was.unwrap_or(0.25);
        let mut released = false;
        ui.horizontal(|ui| {
            ui.checkbox(&mut on, "");
            ui.add_sized([64.0, 18.0], egui::Label::new(&bus.name));
            let response = ui.add_enabled(on, egui::Slider::new(&mut level, SEND));
            released = response.drag_stopped() || response.lost_focus();
        });
        let now = on.then_some(level);
        if now != was {
            actions.commands.push(Command::SetSend { track, bus: bus.id, level: now });
            if now.is_none() || was.is_none() {
                // Making or removing a send is a decision, not a drag.
                actions.seal = true;
            }
        }
        if released {
            actions.seal = true;
        }
    }
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

/// Each column edits its settings in place and says whether a gesture ended:
/// a switch or a pick is a decision of its own, sealed at once; a slider
/// seals when it is let go.
fn compressor(ui: &mut Ui, c: &mut CompSettings, theme: &Theme) -> bool {
    let toggled = heading(ui, "Compressor", &mut c.on, theme);
    let moves = [
        slider(ui, "threshold", &mut c.threshold, CompSettings::THRESHOLD, false, " dB"),
        slider(ui, "ratio", &mut c.ratio, CompSettings::RATIO, true, ":1"),
        slider(ui, "attack", &mut c.attack, CompSettings::ATTACK, true, " ms"),
        slider(ui, "release", &mut c.release, CompSettings::RELEASE, true, " ms"),
        slider(ui, "makeup", &mut c.makeup, CompSettings::MAKEUP, false, " dB"),
    ];
    toggled || moves.iter().any(|(_, released)| *released)
}

fn delay_column(ui: &mut Ui, d: &mut DelaySettings, theme: &Theme, strip: Strip) -> bool {
    let toggled = heading(ui, "Delay", &mut d.on, theme);
    let mut picked = false;
    let salt = match strip {
        Strip::Track(t) => (0u8, t.0),
        Strip::Bus(b) => (1u8, b.0),
    };
    ui.horizontal(|ui| {
        ui.add_sized([64.0, 18.0], egui::Label::new("time"));
        egui::ComboBox::from_id_salt(("delay-time", salt)).selected_text(division_name(d.time)).show_ui(ui, |ui| {
            for (name, ticks) in DIVISIONS {
                picked |= ui.selectable_value(&mut d.time, Tick(ticks), name).clicked();
            }
        });
    });
    let moves = [
        slider(ui, "feedback", &mut d.feedback, DelaySettings::FEEDBACK, false, ""),
        slider(ui, "mix", &mut d.mix, DelaySettings::MIX, false, ""),
    ];
    toggled || picked || moves.iter().any(|(_, released)| *released)
}

fn reverb_column(ui: &mut Ui, r: &mut ReverbSettings, theme: &Theme) -> bool {
    let toggled = heading(ui, "Reverb", &mut r.on, theme);
    let moves = [
        slider(ui, "size", &mut r.size, ReverbSettings::UNIT, false, ""),
        slider(ui, "damping", &mut r.damping, ReverbSettings::UNIT, false, ""),
        slider(ui, "mix", &mut r.mix, ReverbSettings::UNIT, false, ""),
    ];
    toggled || moves.iter().any(|(_, released)| *released)
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
