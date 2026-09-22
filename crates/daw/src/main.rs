//! The application: a window over the project, and the engine that plays it.
//!
//! Three pieces that do not know much about each other. [`studio`] decides what
//! a change means, [`ui`] shows the project and reports what the person did,
//! and the audio callback below plays whatever graph it has been handed. The
//! window can be rewritten without touching the other two, which is the point
//! of the split: the engine is the part that has to stay still.

mod clip;
mod editor;
mod import;
mod layout;
mod peaks;
mod studio;
mod theme;
mod timeline;
mod ui;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui;
use engine_core::{
    AudioBuffer, AutomationSender, Clock, Graph, GraphSwapper, Node, NoteAction, NoteEvent, NoteSender, ParamId,
    Sequencer, Synth, TransportCommand, TransportControl, automation_channel, graph_channel, note_channel,
    transport_channel,
};
use engine_dsp::{Adsr, SineOsc, Waveform};
use engine_io::{AudioEngine, EngineOptions, Stats, default_output_config, map_frames};
use project_engine::{Position, Timing, audio_spans, note_plan};
use project_model::{Asset, Command, NewClip, Project, Session, Source, Track};

use clip::{Lane, Layers, Library, Played, Silence};
use layout::Layout;
use studio::{Engine, Sources, Studio};
use theme::Theme;
use ui::{Actions, Bar, Health};

/// What a project file is called. Anything else dropped on the window is
/// taken for audio.
const PROJECT_EXTENSION: &str = "daw";

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

/// The files that have been read, and what to make of a track that names one.
///
/// Owned outright rather than shared. The studio asks for a node when it needs
/// one and gives this back afterwards, so nothing here is ever borrowed twice
/// at once — and the compiler says so rather than a panic at the worst moment.
struct FileSources {
    library: Library,
    /// The device's rate, which is what files are decoded to: resampling inside
    /// the callback is work nobody has budgeted for.
    rate: u32,
    sample_rate: f32,
    /// The timeline, which every file plays from.
    clock: Arc<Clock>,
}

impl Sources for FileSources {
    /// A file when the track names one, the standby tone otherwise.
    ///
    /// A file the project names but the library has not read falls back to the
    /// tone rather than to silence: a track that vanished without a word would
    /// be harder to notice than one that is obviously not what was asked for.
    fn node_for(&mut self, project: &Project, track: &Track) -> Box<dyn Node> {
        match track.source {
            // Asked for a file but holding none, or holding one the library
            // has not read: the tone says so audibly, where silence would look
            // like a track that had quietly gone missing.
            Source::Clips => {
                let timing = Timing::new(project.tempo(), self.rate);
                let starts = audio_spans(project, track.id, timing);
                let clips: Vec<_> = starts
                    .iter()
                    .filter_map(|span| {
                        let audio = self.library.get(&project.asset(span.asset)?.path)?;
                        Some(Played { audio, start: span.start, offset: span.offset, length: span.length })
                    })
                    .collect();
                // Clips the library could not read: the tone says so audibly.
                if clips.len() < starts.len() {
                    return tone_for(track, self.sample_rate);
                }
                let mut layers: Vec<Box<dyn Node>> = Vec::new();
                if !clips.is_empty() {
                    layers.push(Box::new(Lane::new(clips, Arc::clone(&self.clock))));
                }
                let notes = note_plan(project, track.id, timing);
                if !notes.is_empty() {
                    layers.push(instrument(notes, Arc::clone(&self.clock), self.sample_rate));
                }
                Box::new(Layers(layers))
            }
            Source::Tone => tone_for(track, self.sample_rate),
            Source::Silent => Box::new(Silence),
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

/// How many notes a MIDI track can hold at once before it steals a voice.
const INSTRUMENT_VOICES: usize = 12;
/// Each note's share of full scale at the highest velocity, so a full chord
/// of voices stays near the top rather than far past it.
const INSTRUMENT_LEVEL: f32 = 0.2;

/// What a MIDI track plays through until tracks can choose an instrument: a
/// band-limited saw with a short attack and a soft release.
fn instrument(events: Vec<NoteEvent>, clock: Arc<Clock>, sample_rate: f32) -> Box<dyn Node> {
    let scaled = events
        .into_iter()
        .map(|e| match e.action {
            NoteAction::On { velocity } => NoteEvent::on(e.pitch, velocity * INSTRUMENT_LEVEL, e.frame),
            NoteAction::Off => e,
        })
        .collect();
    Box::new(Sequencer::new(
        INSTRUMENT_VOICES,
        INSTRUMENT_SHAPE,
        &instrument_envelope(sample_rate),
        scaled,
        clock,
        sample_rate,
    ))
}

const INSTRUMENT_SHAPE: Waveform = Waveform::Saw;

fn instrument_envelope(sample_rate: f32) -> Adsr {
    Adsr::new(0.005, 0.15, 0.6, 0.25, sample_rate)
}

/// How long a note being placed or moved in the editor is heard for.
const PREVIEW: std::time::Duration = std::time::Duration::from_millis(250);

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
    /// How big things are. A preference of whoever is looking, so it is kept
    /// here rather than in the project, and never enters the history.
    layout: Layout,
    show_layout: bool,
    /// The arrangement rather than the mixer.
    arrange: bool,
    /// Zoom, scroll and selection in the arrangement. Like the layout, a
    /// preference of whoever is looking: never in the project or its history.
    timeline: timeline::View,
    /// The note editor, when a MIDI clip is open in it.
    editor: Option<editor::Editor>,
    /// Keys go to the editor rather than the timeline: the last click was there.
    editor_keys: bool,
    /// Files already read, so a rebuild reuses what was decoded rather than
    /// reading it again.
    sources: FileSources,
    /// Play, pause, stop, and where the timeline is.
    transport: TransportControl,
    /// Notes heard while editing.
    preview: NoteSender,
    /// When each note being heard is let go.
    releases: Vec<(std::time::Instant, u8)>,
    /// The last thing that went wrong, for the strip along the bottom.
    trouble: Option<String>,
    /// Where the project was last saved or opened from. `None` until then,
    /// and the first save picks a name.
    project_path: Option<PathBuf>,
    /// Pushed into egui only when it changes, rather than every frame.
    theme_applied: Option<usize>,
    stats: Arc<Stats>,
    /// Kept alive: dropping it stops the stream.
    _engine: AudioEngine,
    /// Where to write a picture of the window, then close — set by
    /// `DAW_SCREENSHOT`, so a change to the interface can be looked at from a
    /// terminal, or by a test run, without a screenshot tool.
    screenshot: Option<PathBuf>,
    frames_drawn: u64,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // DAW_HOST picks the audio host ("pulseaudio" under WSLg); unset, the
        // platform's default. Read here rather than asked for in the window:
        // it is a property of the machine, not of the project.
        let options = EngineOptions { host: std::env::var("DAW_HOST").ok(), ..EngineOptions::default() };
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
        // Notes heard while editing, whatever the transport is doing. Outside
        // the graph, so a rebuild never cuts one off, and with the tracks'
        // instrument, so a note sounds as it will when played.
        let (preview, preview_notes) = note_channel(64);
        let mut auditioner =
            Synth::new(4, INSTRUMENT_SHAPE, &instrument_envelope(sample_rate), preview_notes, sample_rate);
        let mut auditioned = vec![0.0f32; MAX_GRAPH_FRAMES * GRAPH_CHANNELS];
        let mut frame = 0u64;
        let (transport, mut timeline) = transport_channel(sample_rate, 64);

        let (engine, _commands) = AudioEngine::start_with(&options, move |out, channels| {
            let frames = out.len() / channels.max(1);
            let mut done = 0;
            while done < frames {
                let span = (frames - done).min(MAX_GRAPH_FRAMES);
                let stereo = &mut mix[..span * GRAPH_CHANNELS];
                // The timeline position is published before the graph renders,
                // so every clip in it reads the same one; the fade after it is
                // what makes a stopped transport silent.
                timeline.begin();
                player.process_automated(stereo, &mut queue, frame);
                timeline.finish(stereo, GRAPH_CHANNELS);
                // Added after the transport's fade, so it sounds when stopped.
                let heard = &mut auditioned[..span * GRAPH_CHANNELS];
                heard.fill(0.0);
                auditioner.set_frame(frame);
                auditioner.process(&mut AudioBuffer::new(heard, GRAPH_CHANNELS));
                for (at, sample) in stereo.iter_mut().enumerate() {
                    // `heard` is planar, one channel after the other.
                    *sample += heard[(at % GRAPH_CHANNELS) * span + at / GRAPH_CHANNELS];
                }
                map_frames(stereo, GRAPH_CHANNELS, &mut out[done * channels..(done + span) * channels], channels);
                frame += span as u64;
                done += span;
            }
        })?;

        let stats = Arc::clone(&engine.stats);
        let mut sources =
            FileSources { library: Library::new(), rate, sample_rate, clock: Arc::clone(transport.clock()) };
        let studio =
            Studio::new(LiveEngine { params, swapper }, &mut sources, GRAPH_CHANNELS, MAX_GRAPH_FRAMES, sample_rate);

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
        Ok(Self {
            studio,
            themes,
            theme: 0,
            theme_applied: Some(0),
            layout: Layout::default(),
            show_layout: false,
            arrange: true,
            timeline: timeline::View::default(),
            editor: None,
            editor_keys: false,
            sources,
            transport,
            preview,
            releases: Vec::new(),
            trouble: None,
            project_path: None,
            stats,
            _engine: engine,
            screenshot: std::env::var_os("DAW_SCREENSHOT").map(PathBuf::from),
            frames_drawn: 0,
        })
    }

    /// Reads a file and gives it a track of its own.
    ///
    /// One command, so one entry in the history: a file dropped by mistake is
    /// taken back with a single undo.
    fn load(&mut self, path: PathBuf) {
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXTENSION)) {
            self.open(&path);
            return;
        }
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mid") || e.eq_ignore_ascii_case("midi")) {
            self.load_midi(&path);
            return;
        }
        // Bound before the match rather than inside it, so reading the file is
        // finished with before the graph is rebuilt from it. Holding the two at
        // once is what the borrow checker is here to refuse.
        let rate = self.sources.rate;
        let loaded = self.sources.library.load(&path, rate);
        match loaded {
            Ok(audio) => {
                // Hashed after decoding rather than before: a file that will
                // not decode is refused without reading it twice.
                let asset = match Asset::from_file(&path) {
                    Ok(asset) => asset,
                    Err(e) => {
                        self.report(format!("{}: {e}", path.display()));
                        return;
                    }
                };
                let name = path.file_stem().map_or_else(|| "Track".into(), |n| n.to_string_lossy().into_owned());
                let seconds = audio.duration().as_secs_f64();
                self.trouble = None;
                println!("loaded {} ({:.1}s, {} ch)", path.display(), seconds, audio.channels);
                let command = Command::AddTrack { name, source: Source::Clips, clip: Some(NewClip::Audio { asset }) };
                let _ = self.studio.apply(command, &mut self.sources);
            }
            Err(e) => self.report(format!("{}: {e}", path.display())),
        }
    }

    /// Reads a MIDI file into a new track holding one clip of its notes.
    ///
    /// One command, so one undo. The file's tempo is not taken: the notes are
    /// in beats, and the project's tempo plays them.
    fn load_midi(&mut self, path: &Path) {
        let score = match engine_midi::read_score(path) {
            Ok(score) => score,
            Err(e) => return self.report(format!("{}: {e}", path.display())),
        };
        let Some(clip) = import::midi_clip(&score, self.studio.project().meter()) else {
            return self.report(format!("{}: no notes in it", path.display()));
        };
        let name = path.file_stem().map_or_else(|| "MIDI".into(), |n| n.to_string_lossy().into_owned());
        println!("loaded {} ({} notes)", path.display(), score.notes.len());
        self.trouble = None;
        let command = Command::AddTrack { name, source: Source::Clips, clip: Some(clip) };
        if let Err(e) = self.studio.apply(command, &mut self.sources) {
            self.report(format!("{}: {e}", path.display()));
        }
    }

    /// Shown along the bottom and printed, so a run watched from a terminal
    /// sees it too.
    fn report(&mut self, message: String) {
        println!("{message}");
        self.trouble = Some(message);
    }

    /// Opens a project and plays it, reading every file it uses.
    ///
    /// A file that is missing, or that no longer holds what the project
    /// recorded, does not stop the project opening: the track plays the tone
    /// and the strip along the bottom says which file it was. Refusing the
    /// whole project over one sample would lose the rest of the work.
    fn open(&mut self, path: &Path) {
        let session = match Session::open(path) {
            Ok(session) => session,
            Err(e) => return self.report(format!("{}: {e}", path.display())),
        };
        let rate = self.sources.rate;
        let mut problems = Vec::new();
        for asset in session.project().assets() {
            match asset.matches_file() {
                Ok(true) => {
                    if let Err(e) = self.sources.library.load(&asset.path, rate) {
                        problems.push(format!("{}: {e}", asset.name()));
                    }
                }
                Ok(false) => problems.push(format!("{} has changed since it was added", asset.name())),
                Err(e) => problems.push(format!("{}: {e}", asset.path.display())),
            }
        }
        println!("opened {} ({} tracks)", path.display(), session.project().track_count());
        self.studio.replace(session, &mut self.sources);
        self.project_path = Some(path.to_path_buf());
        self.trouble = None;
        if !problems.is_empty() {
            self.report(problems.join("; "));
        }
    }

    /// Writes the project where it came from, or somewhere new the first time.
    fn save(&mut self) {
        let path = match self.project_path.clone() {
            Some(path) => path,
            None => match untitled() {
                Ok(path) => path,
                Err(e) => return self.report(format!("cannot choose where to save: {e}")),
            },
        };
        match self.studio.session().save(&path) {
            Ok(()) => {
                println!("saved {}", path.display());
                self.project_path = Some(path);
                self.trouble = None;
            }
            Err(e) => self.report(format!("{}: {e}", path.display())),
        }
    }

    fn carry_out(&mut self, actions: Actions) {
        for command in actions.commands {
            // A command the project refuses is the person asking for something
            // that is no longer there — a track someone else removed, say. It
            // is not an error worth stopping the frame for.
            let _ = self.studio.apply(command, &mut self.sources);
        }
        if actions.seal {
            // Nothing after this folds into the gesture that just ended.
            self.studio.seal();
        }
        if actions.undo {
            self.studio.undo(&mut self.sources);
        }
        if actions.redo {
            self.studio.redo(&mut self.sources);
        }
        if let Some(index) = actions.theme {
            self.theme = index;
        }
        if actions.toggle_layout {
            self.show_layout = !self.show_layout;
        }
        if let Some(arrange) = actions.arrange {
            self.arrange = arrange;
        }
        if let Some(editor) = actions.keys_to_editor {
            self.editor_keys = editor;
        }
        if actions.save {
            self.save();
        }
        if let Some((pitch, velocity)) = actions.preview {
            let level = f32::from(velocity) / 127.0 * INSTRUMENT_LEVEL;
            // Frame zero is always due: the note plays at the next block.
            if self.preview.send(NoteEvent::on(pitch, level, 0)).is_ok() {
                self.releases.push((std::time::Instant::now() + PREVIEW, pitch));
            }
        }
        let now = std::time::Instant::now();
        let preview = &mut self.preview;
        self.releases.retain(|(at, pitch)| {
            // Kept for another frame if the queue had no room.
            *at > now || preview.send(NoteEvent::off(*pitch, 0)).is_err()
        });
        if let Some(command) = actions.transport {
            // A full queue means the audio thread has stalled; the button
            // simply does nothing this time rather than freezing the window.
            self.transport.send(command);
        }
    }
}

impl App {
    /// Asks for a picture once the window has settled, and writes it when it
    /// arrives.
    fn take_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot.clone() else { return };
        self.frames_drawn += 1;
        // A few frames in, so fonts and layout have settled.
        if self.frames_drawn == 20 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(image) = image {
            match write_ppm(&path, &image) {
                Ok(()) => println!("screenshot written to {}", path.display()),
                Err(e) => println!("screenshot {}: {e}", path.display()),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

/// Writes an image as binary PPM: a three-line header and the pixels. Every
/// image tool reads it, and it needs no encoder.
fn write_ppm(path: &Path, image: &egui::ColorImage) -> std::io::Result<()> {
    let [width, height] = image.size;
    let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
    bytes.extend(image.pixels.iter().flat_map(|p| [p.r(), p.g(), p.b()]));
    std::fs::write(path, bytes)
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
        self.take_screenshot(&ctx);
        if self.theme_applied != Some(self.theme) {
            self.themes[self.theme].apply(&ctx);
            self.theme_applied = Some(self.theme);
        }
        let theme = self.themes[self.theme];

        // Dropping a file on the window is how audio gets in. A picker would
        // need a dependency for a dialog; this needs none, and for a mixer it
        // is the better gesture anyway.
        let dropped: Vec<PathBuf> =
            ctx.input(|input| input.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for path in dropped {
            self.load(path);
        }

        let mut actions = Actions::default();
        ctx.input_mut(|input| {
            if input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z) {
                actions.undo = true;
            }
            if input.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z) {
                actions.redo = true;
            }
            if input.consume_key(egui::Modifiers::COMMAND, egui::Key::S) {
                actions.save = true;
            }
        });
        // Space plays and pauses, unless someone is typing a track name.
        if !ctx.egui_wants_keyboard_input() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space))
        {
            let playing = self.transport.is_playing();
            actions.transport = Some(if playing { TransportCommand::Pause } else { TransportCommand::Play });
        }

        egui::Panel::top("bar").show(root, |ui| {
            let name = self.project_path.as_deref().and_then(Path::file_name).map(|n| n.to_string_lossy());
            let project = self.studio.project();
            let timing = Timing::new(project.tempo(), self.sources.rate);
            let position = Position::of(self.transport.clock().position(), timing, project.meter());
            let bar = Bar {
                can_undo: self.studio.can_undo(),
                can_redo: self.studio.can_redo(),
                project: name.as_deref(),
                arrange: self.arrange,
                playing: self.transport.is_playing(),
                position,
                tempo: project.tempo(),
                meter: project.meter(),
            };
            ui::top_bar(ui, &theme, &self.themes, &bar, &mut actions);
        });
        if self.show_layout {
            egui::Panel::top("layout").show(root, |ui| {
                layout::controls(ui, &theme, &mut self.layout);
            });
        }

        let snapshot = self.stats.snapshot();
        egui::Panel::bottom("status").show(root, |ui| {
            let health = Health {
                xruns: snapshot.total_xruns(),
                load_percent: snapshot.load_percentile(0.99) as f64 / 10.0,
                refusals: self.studio.refusals(),
                files: self.sources.library.len(),
                trouble: self.trouble.as_deref(),
            };
            ui::status(ui, &theme, &health);
        });

        // A clip double-clicked in the timeline opens here, under it, so the
        // arrangement stays in sight while its notes are edited.
        if let Some(placement) = self.timeline.open.take() {
            self.editor = Some(editor::Editor::new(placement));
            self.editor_keys = true;
        }
        if self.arrange
            && let Some(open) = self.editor.as_mut()
        {
            let project = self.studio.project();
            let cx = editor::Context {
                theme: &theme,
                timing: Timing::new(project.tempo(), self.sources.rate),
                playhead: self.transport.clock().position(),
                keys: self.editor_keys,
            };
            let still_open = egui::Panel::bottom("editor")
                .resizable(true)
                .default_size(280.0)
                .show(root, |ui| editor::show(ui, project, open, &cx, &mut actions))
                .inner;
            if !still_open {
                self.editor = None;
                self.editor_keys = false;
            }
        }

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme.background)).show(root, |ui| {
            if self.arrange {
                let project = self.studio.project();
                let library = &self.sources.library;
                let peaks = |asset| project.asset(asset).and_then(|a| library.peaks(&a.path));
                let cx = timeline::Context {
                    theme: &theme,
                    timing: Timing::new(project.tempo(), self.sources.rate),
                    playhead: self.transport.clock().position(),
                    playing: self.transport.is_playing(),
                    peaks: &peaks,
                    keys: !(self.editor.is_some() && self.editor_keys),
                };
                timeline::show(ui, project, &mut self.timeline, &cx, &mut actions);
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui::mixer(ui, self.studio.project(), &theme, &self.layout, &mut actions);
                });
            }
        });

        self.carry_out(actions);
        // Anything the engine had no room for is tried again here rather than
        // dropped, so the graph cannot stay out of step with the mixer.
        self.studio.poll(&mut self.sources);

        // The status strip reads counters the audio thread keeps moving, so the
        // window has to come back even when nobody touches it.
        // Faster while playing, so the counter moves smoothly.
        let busy = self.transport.clock().is_running() || !self.releases.is_empty();
        let every = if busy { 30 } else { 200 };
        ctx.request_repaint_after(std::time::Duration::from_millis(every));
    }
}

/// A name for a project saved for the first time: `untitled.daw` in a `Daw`
/// folder in the home directory, numbered so an earlier one is never
/// overwritten.
///
/// Chosen rather than asked for because asking needs a file dialog, and a
/// dialog is a dependency; the name shows in the top bar, and the file can be
/// moved and dropped back on the window.
fn untitled() -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| std::io::Error::other("no home directory"))?;
    let folder = PathBuf::from(home).join("Daw");
    std::fs::create_dir_all(&folder)?;
    let path = (1..)
        .map(|n| {
            let name = if n == 1 { "untitled".into() } else { format!("untitled-{n}") };
            folder.join(format!("{name}.{PROJECT_EXTENSION}"))
        })
        .find(|path| !path.exists())
        .expect("some number is free");
    Ok(path)
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 640.0]).with_title("Daw"),
        ..Default::default()
    };
    // A project named on the command line is opened at start, which is also
    // how a file manager opens one.
    let project = std::env::args_os().nth(1).map(PathBuf::from);
    eframe::run_native(
        "Daw",
        options,
        Box::new(|cc| {
            let mut app = App::new(cc)?;
            if let Some(path) = project {
                app.open(&path);
            }
            Ok(Box::new(app))
        }),
    )
}
