//! The window. A view over the project, and nothing else.
//!
//! It reads the project and reports what the person did; it never changes
//! anything itself. Everything it produces is a [`Command`], which is what
//! keeps the history complete and what makes this file replaceable without
//! touching anything that matters.

use eframe::egui::{self, RichText, Ui};
use project_model::{Command, Project, TrackId};

use crate::theme::Theme;

/// What happened during one frame.
#[derive(Default)]
pub struct Actions {
    pub commands: Vec<Command>,
    /// A control was let go, so what follows is a new gesture rather than a
    /// continuation of this one.
    pub seal: bool,
    pub undo: bool,
    pub redo: bool,
    /// A different theme was chosen.
    pub theme: Option<usize>,
}

/// How the engine is getting on, for the strip along the bottom.
pub struct Health {
    pub xruns: u64,
    pub load_percent: f64,
    pub refusals: u64,
}

pub fn top_bar(ui: &mut Ui, theme: &Theme, themes: &[Theme], can_undo: bool, can_redo: bool, actions: &mut Actions) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Daw").color(theme.accent).strong());
        ui.separator();

        if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
            actions.undo = true;
        }
        if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
            actions.redo = true;
        }

        ui.separator();
        if ui.button("Add track").clicked() {
            actions.commands.push(Command::AddTrack { name: "Track".into() });
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (index, candidate) in themes.iter().enumerate().rev() {
                let current = candidate.name == theme.name;
                if ui.selectable_label(current, candidate.name).clicked() && !current {
                    actions.theme = Some(index);
                }
            }
        });
    });
}

pub fn mixer(ui: &mut Ui, project: &Project, theme: &Theme, actions: &mut Actions) {
    if project.tracks.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No tracks yet.").color(theme.dim));
            ui.label(RichText::new("Add one to hear something.").color(theme.dim).small());
        });
        return;
    }

    for track in &project.tracks {
        let audible = project.is_audible(track.id);
        strip(ui, track, audible, theme, actions);
        ui.add_space(4.0);
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Master").color(theme.dim).strong());
        let mut gain = project.master_gain;
        let slider = ui.add(egui::Slider::new(&mut gain, 0.0..=2.0).show_value(false));
        if slider.changed() {
            actions.commands.push(Command::SetMasterGain { gain });
        }
        if slider.drag_stopped() {
            actions.seal = true;
        }
        ui.label(RichText::new(format!("{:.2}", project.master_gain)).color(theme.dim).monospace());
    });
}

fn strip(ui: &mut Ui, track: &project_model::Track, audible: bool, theme: &Theme, actions: &mut Actions) {
    let id: TrackId = track.id;
    egui::Frame::group(ui.style()).fill(theme.panel).show(ui, |ui| {
        ui.horizontal(|ui| {
            let mut name = track.name.clone();
            if ui.add(egui::TextEdit::singleline(&mut name).desired_width(120.0)).changed() {
                actions.commands.push(Command::RenameTrack { id, name });
            }

            // Colour carries the state as well as the label, so a glance at a
            // tall mixer is enough.
            let mute_colour = if track.muted { theme.muted } else { theme.dim };
            if ui.button(RichText::new("M").color(mute_colour).strong()).clicked() {
                actions.commands.push(Command::SetMuted { id, muted: !track.muted });
            }
            let solo_colour = if track.soloed { theme.soloed } else { theme.dim };
            if ui.button(RichText::new("S").color(solo_colour).strong()).clicked() {
                actions.commands.push(Command::SetSoloed { id, soloed: !track.soloed });
            }

            ui.label(RichText::new("gain").color(theme.dim).small());
            let mut gain = track.gain;
            let fader = ui.add(egui::Slider::new(&mut gain, 0.0..=2.0).show_value(false));
            if fader.changed() {
                actions.commands.push(Command::SetGain { id, gain });
            }
            if fader.drag_stopped() {
                actions.seal = true;
            }

            ui.label(RichText::new("pan").color(theme.dim).small());
            let mut pan = track.pan;
            let panner = ui.add(egui::Slider::new(&mut pan, -1.0..=1.0).show_value(false));
            if panner.changed() {
                actions.commands.push(Command::SetPan { id, pan });
            }
            if panner.drag_stopped() {
                actions.seal = true;
            }

            // Dimmed rather than hidden: a silenced track is still there, and
            // hiding it would lose where it sits among the others.
            let readout = format!("{:.2} / {:+.2}", track.gain, track.pan);
            let colour = if audible { theme.text } else { theme.muted };
            ui.label(RichText::new(readout).color(colour).monospace());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("remove").color(theme.warning).small()).clicked() {
                    actions.commands.push(Command::RemoveTrack { id });
                }
            });
        });
    });
}

pub fn status(ui: &mut Ui, theme: &Theme, health: &Health) {
    ui.horizontal(|ui| {
        let trouble = health.xruns > 0 || health.refusals > 0;
        let colour = if trouble { theme.warning } else { theme.dim };
        ui.label(
            RichText::new(format!(
                "load {:.1}%   xruns {}   refused {}",
                health.load_percent, health.xruns, health.refusals
            ))
            .color(colour)
            .monospace()
            .small(),
        );
    });
}
