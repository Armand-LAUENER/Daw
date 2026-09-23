//! The effects of a track or a bus: compressor, echo and room side by side,
//! and for a track, how much it sends to each bus.
//!
//! Writes only those settings (decision D5). Every control sends a command
//! while it moves and seals the step when it is let go, so a sweep of a slider
//! is one undo.

use std::ops::RangeInclusive;

use eframe::egui::{self, RichText, Ui};
use project_model::{
    BusId, Command, CompSettings, DelaySettings, Instrument, PPQ, Project, ReverbSettings, SEND, Tick, TrackId, Wave,
};

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
/// `reduction` is how much the compressor is turning down right now, in dB,
/// when it is on and playing.
pub fn show(
    ui: &mut Ui,
    project: &Project,
    strip: Strip,
    reduction: Option<f32>,
    theme: &Theme,
    actions: &mut Actions,
) -> bool {
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

    // A track also has its instrument, first, and its sends, last.
    let track = match strip {
        Strip::Track(id) => project.track(id),
        Strip::Bus(_) => None,
    };
    let (mut c, mut d, mut r) = (comp, delay, reverb);
    let mut sealed = false;
    // Columns of a fixed width, side by side, scrolled sideways when the
    // window is too narrow for them all — rather than squeezed until their
    // controls run into each other.
    egui::ScrollArea::horizontal().show(ui, |ui| {
        ui.horizontal_top(|ui| {
            if let Some(t) = track {
                let mut inst = t.instrument;
                sealed |= column(ui, |ui| instrument(ui, &mut inst, theme));
                if inst != t.instrument {
                    actions.commands.push(Command::SetInstrument { track: t.id, value: inst });
                }
            }
            sealed |= column(ui, |ui| compressor(ui, &mut c, reduction, theme));
            sealed |= column(ui, |ui| delay_column(ui, &mut d, theme, strip));
            sealed |= column(ui, |ui| reverb_column(ui, &mut r, theme));
            if let Some(t) = track {
                column(ui, |ui| {
                    sends(ui, project, t.id, theme, actions);
                    false
                });
            }
        });
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

/// Width of one column of controls: a label, a slider and its value.
const COLUMN_WIDTH: f32 = 250.0;
const SLIDER_WIDTH: f32 = 110.0;

/// A column of controls, `COLUMN_WIDTH` wide. Returns what its contents say.
fn column(ui: &mut Ui, contents: impl FnOnce(&mut Ui) -> bool) -> bool {
    ui.allocate_ui(egui::vec2(COLUMN_WIDTH, ui.available_height()), |ui| {
        ui.set_width(COLUMN_WIDTH);
        ui.spacing_mut().slider_width = SLIDER_WIDTH;
        ui.vertical(contents).inner
    })
    .inner
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

/// The track's instrument: its wave, its envelope and its level. Played by
/// the track's MIDI clips; the tone and audio clips are not affected.
fn instrument(ui: &mut Ui, i: &mut Instrument, theme: &Theme) -> bool {
    ui.label(RichText::new("Instrument").color(theme.accent).strong());
    let mut picked = false;
    ui.horizontal(|ui| {
        ui.add_sized([64.0, 18.0], egui::Label::new("wave"));
        for wave in Wave::ALL {
            picked |= ui.selectable_value(&mut i.wave, wave, wave.name()).clicked();
        }
    });
    let moves = [
        slider(ui, "attack", &mut i.attack, Instrument::ATTACK, true, " ms"),
        slider(ui, "decay", &mut i.decay, Instrument::DECAY, true, " ms"),
        slider(ui, "sustain", &mut i.sustain, Instrument::SUSTAIN, false, ""),
        slider(ui, "release", &mut i.release, Instrument::RELEASE, true, " ms"),
        slider(ui, "level", &mut i.level, Instrument::LEVEL, false, ""),
    ];
    picked || moves.iter().any(|(_, released)| *released)
}

/// How much the compressor is turning down: a bar growing from the right,
/// as gain reduction is drawn on hardware, over 24 dB.
fn reduction_meter(ui: &mut Ui, db: f32, theme: &Theme) {
    const RANGE_DB: f32 = 24.0;
    ui.horizontal(|ui| {
        ui.add_sized([64.0, 14.0], egui::Label::new("reduction"));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(SLIDER_WIDTH, 10.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2.0, theme.control);
        let fraction = (db / RANGE_DB).clamp(0.0, 1.0);
        let bar = egui::Rect::from_min_max(
            egui::pos2(rect.right() - rect.width() * fraction, rect.top()),
            rect.right_bottom(),
        );
        ui.painter().rect_filled(bar, 2.0, theme.soloed);
        ui.label(RichText::new(format!("-{db:.1} dB")).monospace().small().color(theme.dim));
    });
}

/// Each column edits its settings in place and says whether a gesture ended:
/// a switch or a pick is a decision of its own, sealed at once; a slider
/// seals when it is let go.
fn compressor(ui: &mut Ui, c: &mut CompSettings, reduction: Option<f32>, theme: &Theme) -> bool {
    let toggled = heading(ui, "Compressor", &mut c.on, theme);
    if let Some(db) = reduction {
        reduction_meter(ui, db, theme);
    }
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
