//! The application: a window over the project, and the engine that plays it.
//!
//! Three pieces that do not know much about each other. [`studio`] decides what
//! a change means, [`ui`] shows the project and reports what the person did,
//! and the audio callback below plays whatever graph it has been handed. The
//! window can be rewritten without touching the other two, which is the point
//! of the split: the engine is the part that has to stay still.

mod studio;
mod theme;
mod ui;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui;
use engine_core::{AutomationSender, Graph, GraphSwapper, Node, ParamId, automation_channel, graph_channel};
use engine_dsp::SineOsc;
use engine_io::{AudioEngine, EngineOptions, Stats, default_output_config, map_frames};
use project_model::Track;

use studio::{Engine, Studio};
use theme::Theme;
use ui::{Actions, Health};

/// The graph is stereo whatever the device is: panning needs a pair, and the
/// device's own layout is reached by mapping afterwards.
const GRAPH_CHANNELS: usize = 2;
/// Largest block the graph is built for; a bigger callback is rendered in
/// several passes rather than sizing every node's buffer for the worst case.
const MAX_GRAPH_FRAMES: usize = 2_048;

/// What a track plays until the model can hold a clip.
///
/// A quiet tone, at a pitch taken from the track's own id so that two tracks
/// are told apart by ear as well as by eye.
struct Tone {
    osc: SineOsc,
    frequency: f32,
}

impl Node for Tone {
    fn process(&mut self, buffer: &mut engine_core::AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        for frame in 0..frames {
            let sample = self.osc.next_sample(self.frequency) * 0.4;
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] = sample;
            }
        }
    }
}

fn tone_for(track: &Track, sample_rate: f32) -> Box<dyn Node> {
    // A minor pentatonic, so a stack of tracks is bearable to sit next to.
    const SCALE: [f32; 5] = [220.0, 261.63, 293.66, 329.63, 392.0];
    let step = track.id.0 as usize;
    let octave = 2f32.powi((step / SCALE.len()) as i32);
    Box::new(Tone { osc: SineOsc::new(sample_rate), frequency: SCALE[step % SCALE.len()] * octave })
}

/// The real engine: a queue for parameters and a hand-over for graphs.
struct LiveEngine {
    params: AutomationSender,
    swapper: GraphSwapper,
}

impl Engine for LiveEngine {
    fn send(&mut self, param: ParamId, value: f32) -> bool {
        self.params.set(param, value).is_ok()
    }

    fn swap(&mut self, graph: Box<Graph>) -> bool {
        let accepted = self.swapper.swap(graph).is_ok();
        // Retired graphs are freed here. Freeing one means freeing every buffer
        // in it, which is allocator work the audio thread must never do.
        while self.swapper.collect().is_some() {}
        accepted
    }
}

struct App {
    studio: Studio<LiveEngine>,
    themes: [Theme; 2],
    theme: usize,
    /// Pushed into egui only when it changes, rather than every frame.
    theme_applied: Option<usize>,
    stats: Arc<Stats>,
    /// Kept alive: dropping it stops the stream.
    _engine: AudioEngine,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let options = EngineOptions::default();
        let (rate, channels) = default_output_config(options.host.as_deref(), options.device.as_deref())?;
        let sample_rate = rate as f32;
        // Printed rather than only shown in the window: a run checked from a
        // terminal, or from another machine, otherwise has no evidence that
        // the audio path opened at all.
        println!("device: {rate} Hz, {channels} channels");

        let (swapper, mut player) = graph_channel(None, GRAPH_CHANNELS, MAX_GRAPH_FRAMES, sample_rate);
        let (params, mut queue) = automation_channel(256);

        // Everything the callback needs is built here and moved in: allocating
        // inside it would trip the no-allocation check the engine wraps it in.
        let mut mix = vec![0.0f32; MAX_GRAPH_FRAMES * GRAPH_CHANNELS];
        let mut frame = 0u64;
        let position = Arc::new(AtomicU64::new(0));
        let callback_position = Arc::clone(&position);

        let (engine, _commands) = AudioEngine::start_with(&options, move |out, channels| {
            let frames = out.len() / channels.max(1);
            let mut done = 0;
            while done < frames {
                let span = (frames - done).min(MAX_GRAPH_FRAMES);
                let stereo = &mut mix[..span * GRAPH_CHANNELS];
                player.process_automated(stereo, &mut queue, frame);
                map_frames(stereo, GRAPH_CHANNELS, &mut out[done * channels..(done + span) * channels], channels);
                frame += span as u64;
                done += span;
            }
            callback_position.store(frame, Ordering::Relaxed);
        })?;

        let stats = Arc::clone(&engine.stats);
        let studio = Studio::new(
            LiveEngine { params, swapper },
            Box::new(move |track| tone_for(track, sample_rate)),
            GRAPH_CHANNELS,
            MAX_GRAPH_FRAMES,
            sample_rate,
        );

        println!(
            "host {} | device {}\n{} Hz, {} ch, {} | buffer: {}",
            engine.info.host,
            engine.info.device,
            engine.info.sample_rate,
            engine.info.channels,
            engine.info.sample_format,
            engine.info.buffer_size
        );

        let themes = Theme::all();
        println!("themes: {}", themes.iter().map(|t| t.name).collect::<Vec<_>>().join(", "));
        themes[0].apply(&cc.egui_ctx);
        Ok(Self { studio, themes, theme: 0, theme_applied: Some(0), stats, _engine: engine })
    }

    fn carry_out(&mut self, actions: Actions) {
        for command in actions.commands {
            // A command the project refuses is the person asking for something
            // that is no longer there — a track someone else removed, say. It
            // is not an error worth stopping the frame for.
            let _ = self.studio.apply(command);
        }
        if actions.seal {
            // Nothing after this folds into the gesture that just ended.
            self.studio.seal();
        }
        if actions.undo {
            self.studio.undo();
        }
        if actions.redo {
            self.studio.redo();
        }
        if let Some(index) = actions.theme {
            self.theme = index;
        }
    }
}

impl Drop for App {
    /// What the engine made of the session. A window closed without a word
    /// would leave a run unfalsifiable.
    fn drop(&mut self) {
        let stats = self.stats.snapshot();
        println!(
            "\ncallbacks {} | xruns {} | overloads {} | load p50 {:.1}% p99 {:.1}% max {:.1}% | rebuilds refused {}",
            stats.callbacks,
            stats.total_xruns(),
            stats.overloads,
            stats.load_percentile(0.5) as f64 / 10.0,
            stats.load_percentile(0.99) as f64 / 10.0,
            stats.max_load_permille as f64 / 10.0,
            self.studio.refusals(),
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        if self.theme_applied != Some(self.theme) {
            self.themes[self.theme].apply(&ctx);
            self.theme_applied = Some(self.theme);
        }
        let theme = self.themes[self.theme];

        let mut actions = Actions::default();
        ctx.input_mut(|input| {
            if input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z) {
                actions.undo = true;
            }
            if input.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z) {
                actions.redo = true;
            }
        });

        egui::Panel::top("bar").show(root, |ui| {
            ui::top_bar(ui, &theme, &self.themes, self.studio.can_undo(), self.studio.can_redo(), &mut actions);
        });

        let snapshot = self.stats.snapshot();
        egui::Panel::bottom("status").show(root, |ui| {
            let health = Health {
                xruns: snapshot.total_xruns(),
                load_percent: snapshot.load_percentile(0.99) as f64 / 10.0,
                refusals: self.studio.refusals(),
            };
            ui::status(ui, &theme, &health);
        });

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme.background)).show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui::mixer(ui, self.studio.project(), &theme, &mut actions);
            });
        });

        self.carry_out(actions);
        // Anything the engine had no room for is tried again here rather than
        // dropped, so the graph cannot stay out of step with the mixer.
        self.studio.poll();

        // The status strip reads counters the audio thread keeps moving, so the
        // window has to come back even when nobody touches it.
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 600.0]).with_title("Daw"),
        ..Default::default()
    };
    eframe::run_native("Daw", options, Box::new(|cc| Ok(Box::new(App::new(cc)?))))
}
