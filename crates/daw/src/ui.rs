//! The window. A view over the project, and nothing else.
//!
//! It reads the project and reports what the person did; it never changes
//! anything itself. Everything it produces is a [`Command`], which is what
//! keeps the history complete and what makes this file replaceable without
//! touching anything that matters.

use eframe::egui::{self, RichText, Ui};
use engine_core::TransportCommand;
use project_engine::Position;
use project_model::{Command, Meter, Project, Source, Tempo, TrackId};

use crate::fx_view::Strip;
use crate::layout::Layout;
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
    /// The layout controls were asked for, or dismissed.
    pub toggle_layout: bool,
    /// The arrangement (true) or the mixer (false) was chosen.
    pub arrange: Option<bool>,
    /// Where the last click landed: in the note editor (true) or the
    /// timeline (false). Keys go there, so Delete removes the note being
    /// edited rather than the clip around it.
    pub keys_to_editor: Option<bool>,
    /// A note to hear, as pitch and velocity: one just placed, picked up, or
    /// moved to another pitch in the editor.
    pub preview: Option<(u8, u8)>,
    /// A track whose equaliser was asked for.
    pub eq: Option<TrackId>,
    /// A track whose effects were asked for.
    pub fx: Option<Strip>,
    /// The project is to be written to disk.
    pub save: bool,
    /// Save under a new name was asked for.
    pub save_as: bool,
    /// A project picked from the Open menu.
    pub open: Option<std::path::PathBuf>,
    /// A new, empty project was asked for.
    pub new_project: bool,
    /// Play, pause, stop or seek. Not a [`Command`]: where the playhead is
    /// belongs to the engine, not to the project, and never enters the history.
    pub transport: Option<TransportCommand>,
}

/// How the engine is getting on, for the strip along the bottom.
pub struct Health<'a> {
    pub xruns: u64,
    pub load_percent: f64,
    pub refusals: u64,
    pub files: usize,
    /// The last thing that went wrong, if anything did. Shown rather than
    /// logged: a file that failed to open is the person's business.
    pub trouble: Option<&'a str>,
}

/// What the top bar shows besides its buttons.
pub struct Bar<'a> {
    pub can_undo: bool,
    pub can_redo: bool,
    /// The project's file name, or `None` before it is first saved.
    pub project: Option<&'a str>,
    /// Changes not saved yet.
    pub dirty: bool,
    /// The projects folder, for the Open menu.
    pub folder: Option<&'a std::path::Path>,
    /// Whether the arrangement is showing, rather than the mixer.
    pub arrange: bool,
    /// Whether play was the last thing asked of the transport.
    pub playing: bool,
    pub position: Position,
    pub tempo: Tempo,
    pub meter: Meter,
}

pub fn top_bar(ui: &mut Ui, theme: &Theme, themes: &[Theme], bar: &Bar<'_>, actions: &mut Actions) {
    let Bar { can_undo, can_redo, project, .. } = *bar;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Daw").color(theme.accent).strong());
        // A dot while there is something to save, as most programs do.
        let title = format!("{}{}", project.unwrap_or("untitled"), if bar.dirty { " ●" } else { "" });
        ui.label(RichText::new(title).color(if bar.dirty { theme.text } else { theme.dim }));
        ui.separator();

        // One view at a time, each for one task (decision D5): placing
        // material in time, or balancing it.
        if ui.selectable_label(bar.arrange, "Arrange").clicked() && !bar.arrange {
            actions.arrange = Some(true);
        }
        if ui.selectable_label(!bar.arrange, "Mix").clicked() && bar.arrange {
            actions.arrange = Some(false);
        }
        ui.separator();

        if ui.button("New").clicked() {
            actions.new_project = true;
        }
        ui.menu_button("Open", |ui| {
            let listed = bar.folder.map(crate::projects::list).unwrap_or_default();
            if listed.is_empty() {
                ui.label(RichText::new("no project saved yet").color(theme.dim));
            }
            for project in listed {
                if ui.button(&project.name).clicked() {
                    actions.open = Some(project.path);
                    ui.close();
                }
            }
            ui.separator();
            ui.label(RichText::new("or drop a .daw file on the window").color(theme.dim).small());
        });
        if ui.button("Save").on_hover_text("Ctrl+S").clicked() {
            actions.save = true;
        }
        if ui.button("Save as…").clicked() {
            actions.save_as = true;
        }

        if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
            actions.undo = true;
        }
        if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
            actions.redo = true;
        }

        ui.separator();
        if ui.button("Add track").clicked() {
            actions.commands.push(Command::AddTrack { name: "Track".into(), source: Default::default(), clip: None });
        }

        ui.separator();
        transport(ui, theme, bar, actions);

        ui.separator();
        if ui.button("Layout").clicked() {
            actions.toggle_layout = true;
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

/// The play/pause button, its icon drawn rather than typed: the font egui
/// ships has a triangle but no pause sign, which showed as two empty boxes
/// while playing.
fn play_pause(ui: &mut Ui, playing: bool, theme: &Theme) -> egui::Response {
    let size = egui::vec2(26.0, ui.spacing().interact_size.y);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = ui.style().interact(&response);
    let painter = ui.painter();
    painter.rect_filled(rect, visuals.corner_radius, visuals.bg_fill);
    let (centre, half) = (rect.center(), rect.height() * 0.22);
    if playing {
        // Two bars.
        for side in [-1.0f32, 1.0] {
            let x = centre.x + side * half * 0.55;
            let bar = egui::Rect::from_center_size(egui::pos2(x, centre.y), egui::vec2(half * 0.6, half * 2.0));
            painter.rect_filled(bar, 1.0, theme.text);
        }
    } else {
        let points = vec![
            egui::pos2(centre.x - half * 0.8, centre.y - half),
            egui::pos2(centre.x - half * 0.8, centre.y + half),
            egui::pos2(centre.x + half, centre.y),
        ];
        painter.add(egui::Shape::convex_polygon(points, theme.text, egui::Stroke::NONE));
    }
    response
}

/// Play/pause, stop, where the playhead is, and the tempo and time signature.
fn transport(ui: &mut Ui, theme: &Theme, bar: &Bar<'_>, actions: &mut Actions) {
    let command = if bar.playing { TransportCommand::Pause } else { TransportCommand::Play };
    if play_pause(ui, bar.playing, theme).on_hover_text("play / pause (space)").clicked() {
        actions.transport = Some(command);
    }
    if ui.button("■").on_hover_text("stop and go back to the start").clicked() {
        actions.transport = Some(TransportCommand::Stop);
    }
    ui.label(RichText::new(bar.position.musical()).color(theme.accent).monospace());
    ui.label(RichText::new(bar.position.clock()).color(theme.dim).monospace().small());

    // Dragged like a fader: one gesture, one step in the history.
    let mut bpm = bar.tempo.bpm();
    let range = Tempo::MIN.bpm()..=Tempo::MAX.bpm();
    let tempo = ui.add(egui::DragValue::new(&mut bpm).range(range).speed(0.1).max_decimals(3).suffix(" bpm"));
    if tempo.changed()
        && let Some(tempo) = Tempo::from_bpm(bpm)
    {
        actions.commands.push(Command::SetTempo { tempo });
    }
    if tempo.drag_stopped() || tempo.lost_focus() {
        actions.seal = true;
    }

    let mut beats = bar.meter.beats();
    if ui.add(egui::DragValue::new(&mut beats).range(1..=32)).changed()
        && let Some(meter) = Meter::new(beats, bar.meter.unit())
    {
        actions.commands.push(Command::SetMeter { meter });
    }
    ui.label("/");
    egui::ComboBox::from_id_salt("meter-unit").width(36.0).selected_text(bar.meter.unit().to_string()).show_ui(
        ui,
        |ui| {
            for unit in [2u8, 4, 8, 16] {
                if ui.selectable_label(unit == bar.meter.unit(), unit.to_string()).clicked()
                    && unit != bar.meter.unit()
                    && let Some(meter) = Meter::new(bar.meter.beats(), unit)
                {
                    actions.commands.push(Command::SetMeter { meter });
                }
            }
        },
    );
}

pub fn mixer(ui: &mut Ui, project: &Project, theme: &Theme, layout: &Layout, actions: &mut Actions) {
    if project.track_count() == 0 {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No tracks yet.").color(theme.dim));
            ui.label(RichText::new("Add one to hear something.").color(theme.dim).small());
        });
        return;
    }

    for track in project.tracks() {
        strip(ui, project, track, theme, layout, actions);
        ui.add_space(layout.strip_spacing);
    }

    // The buses, under the tracks: shared effects the tracks send to.
    for bus in project.buses() {
        bus_strip(ui, bus, theme, layout, actions);
        ui.add_space(layout.strip_spacing);
    }
    if ui.button("Add FX bus").on_hover_text("a shared reverb that tracks send to").clicked() {
        actions.commands.push(Command::AddBus { name: "Reverb".into() });
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Master").color(theme.dim).strong());
        let mut gain = project.master_gain();
        ui.spacing_mut().slider_width = layout.gain_width;
        let slider = ui.add(egui::Slider::new(&mut gain, 0.0..=2.0).show_value(false));
        if slider.changed() {
            actions.commands.push(Command::SetMasterGain { gain });
        }
        if slider.drag_stopped() {
            actions.seal = true;
        }
        ui.label(RichText::new(format!("{:.2}", project.master_gain())).color(theme.dim).monospace());
    });
}

fn strip(
    ui: &mut Ui,
    project: &Project,
    track: &project_model::Track,
    theme: &Theme,
    layout: &Layout,
    actions: &mut Actions,
) {
    let id: TrackId = track.id;
    let audible = project.is_audible(id);
    egui::Frame::group(ui.style()).fill(theme.panel).show(ui, |ui| {
        ui.horizontal(|ui| {
            let mut name = track.name.clone();
            if ui.add(egui::TextEdit::singleline(&mut name).desired_width(layout.name_width)).changed() {
                actions.commands.push(Command::RenameTrack { id, name });
            }

            // Colour carries the state as well as the label, so a glance at a
            // tall mixer is enough.
            let mute_colour = if track.muted { theme.muted } else { theme.dim };
            let mute = egui::Button::new(RichText::new("M").color(mute_colour).strong())
                .min_size(egui::vec2(layout.switch_width, 0.0));
            if ui.add(mute).clicked() {
                actions.commands.push(Command::SetMuted { id, muted: !track.muted });
            }
            let solo_colour = if track.soloed { theme.soloed } else { theme.dim };
            let solo = egui::Button::new(RichText::new("S").color(solo_colour).strong())
                .min_size(egui::vec2(layout.switch_width, 0.0));
            if ui.add(solo).clicked() {
                actions.commands.push(Command::SetSoloed { id, soloed: !track.soloed });
            }
            // Lit when any band is doing something, so a shaped track can be
            // told from a flat one at a glance.
            let shaped = track.eq.iter().any(|b| b.on && (!b.shape.has_gain() || b.gain != 0.0));
            let eq_colour = if shaped { theme.accent } else { theme.dim };
            if ui.add(egui::Button::new(RichText::new("EQ").color(eq_colour).small())).clicked() {
                actions.eq = Some(id);
            }
            let effects_on = track.comp.on || track.delay.on || track.reverb.on;
            let fx_colour = if effects_on { theme.accent } else { theme.dim };
            if ui.add(egui::Button::new(RichText::new("FX").color(fx_colour).small())).clicked() {
                actions.fx = Some(Strip::Track(id));
            }

            ui.label(RichText::new("gain").color(theme.dim).small());
            let mut gain = track.gain;
            ui.spacing_mut().slider_width = layout.gain_width;
            let fader = ui.add(egui::Slider::new(&mut gain, 0.0..=2.0).show_value(false));
            if fader.changed() {
                actions.commands.push(Command::SetGain { id, gain });
            }
            if fader.drag_stopped() {
                actions.seal = true;
            }

            ui.label(RichText::new("pan").color(theme.dim).small());
            let mut pan = track.pan;
            ui.spacing_mut().slider_width = layout.pan_width;
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
                // Three states, chosen rather than cycled, and none of them
                // throws the file away: switching to the tone and back is how
                // a person hears what a setting is doing.
                let has_clips = !project.placements_on(id).is_empty();
                for (label, source, enabled) in [
                    ("silent", Source::Silent, true),
                    ("tone", Source::Tone, true),
                    ("clips", Source::Clips, has_clips),
                ] {
                    let chosen = track.source == source;
                    let colour = if chosen { theme.accent } else { theme.dim };
                    // Selection is carried by colour rather than by a frame:
                    // the chosen one is also the one that cannot be clicked.
                    let button = egui::Button::new(RichText::new(label).color(colour).small());
                    if ui.add_enabled(enabled && !chosen, button).clicked() {
                        actions.commands.push(Command::SetSource { id, source });
                    }
                }
                if has_clips && ui.button(RichText::new("eject").color(theme.warning).small()).clicked() {
                    // Takes the clips off the track for good; the tone button
                    // is how you stop listening to them without losing them.
                    actions.commands.push(Command::ClearTrack { id });
                    actions.commands.push(Command::SetSource { id, source: Source::Silent });
                }
                let colour = if track.source == Source::Clips { theme.accent } else { theme.dim };
                ui.label(RichText::new(project.playing(track)).color(colour).small());
            });
        });
    });
}

/// A bus: its name, mute, fader and effects. No solo: a return stays open when
/// a track is soloed, or soloing a track would take its reverb away.
fn bus_strip(ui: &mut Ui, bus: &project_model::Bus, theme: &Theme, layout: &Layout, actions: &mut Actions) {
    let id = bus.id;
    egui::Frame::group(ui.style()).fill(theme.panel).show(ui, |ui| {
        ui.horizontal(|ui| {
            let mut name = bus.name.clone();
            let field = ui.add_sized([layout.name_width, 20.0], egui::TextEdit::singleline(&mut name));
            if field.lost_focus() && name != bus.name {
                actions.commands.push(Command::RenameBus { id, name });
            }
            let mute_colour = if bus.muted { theme.muted } else { theme.dim };
            if ui.add(egui::Button::new(RichText::new("M").color(mute_colour).strong())).clicked() {
                actions.commands.push(Command::SetBusMuted { id, muted: !bus.muted });
            }
            let effects_on = bus.comp.on || bus.delay.on || bus.reverb.on;
            let fx_colour = if effects_on { theme.accent } else { theme.dim };
            if ui.add(egui::Button::new(RichText::new("FX").color(fx_colour).small())).clicked() {
                actions.fx = Some(Strip::Bus(id));
            }
            ui.label(RichText::new("return").color(theme.dim).small());
            let mut gain = bus.gain;
            ui.spacing_mut().slider_width = layout.gain_width;
            let slider = ui.add(egui::Slider::new(&mut gain, 0.0..=2.0).show_value(false));
            if slider.changed() {
                actions.commands.push(Command::SetBusGain { id, gain });
            }
            if slider.drag_stopped() {
                actions.seal = true;
            }
            ui.label(RichText::new(format!("{:.2}", bus.gain)).color(theme.text).monospace());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("remove").color(theme.warning).small()).clicked() {
                    actions.commands.push(Command::RemoveBus { id });
                }
                ui.label(RichText::new("bus").color(theme.accent).small());
            });
        });
    });
}

pub fn status(ui: &mut Ui, theme: &Theme, health: &Health<'_>) {
    ui.horizontal(|ui| {
        let unwell = health.xruns > 0 || health.refusals > 0;
        let colour = if unwell { theme.warning } else { theme.dim };
        ui.label(
            RichText::new(format!(
                "load {:.1}%   xruns {}   refused {}   files {}",
                health.load_percent, health.xruns, health.refusals, health.files
            ))
            .color(colour)
            .monospace()
            .small(),
        );
        if let Some(trouble) = health.trouble {
            ui.separator();
            ui.label(RichText::new(trouble).color(theme.warning).small());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new("drop audio or a .mid to add a track, or a .daw project to open it")
                    .color(theme.dim)
                    .small(),
            );
        });
    });
}
