//! The arrangement: tracks as rows, clips placed along them, and the playhead.
//!
//! A view in the sense of decision D5 — it writes only placements (where and
//! on which track a clip sits, which clip it plays) and the transport's
//! position. Notes, levels and routing belong to other views. Everything it
//! changes goes out as a [`Command`] or a transport request; how the timeline
//! is being looked at — zoom, scroll, selection — is kept in [`View`], which is
//! a preference of whoever is looking and never enters the project or its
//! history.
//!
//! The geometry is kept apart from the painting and tested on its own: where a
//! tick is on screen, where a dragged clip lands, and how the grid snaps. Those
//! are the parts a person notices when they are one pixel or one tick wrong.

use std::sync::Arc;

use eframe::egui::{self, Align2, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use engine_core::TransportCommand;
use project_engine::Timing;
use project_model::{
    AssetId, AudioWindow, Command, Content, LoopRegion, Meter, PPQ, Placement, PlacementId, Project, Tick, TrackId,
    Window,
};

use crate::peaks::Peaks;
use crate::theme::Theme;
use crate::ui::Actions;

/// Width of the column of track names on the left.
const HEADER_WIDTH: f32 = 140.0;
/// Height of the bar ruler along the top.
const RULER_HEIGHT: f32 = 22.0;
/// Height of the tempo lane, under the ruler.
const TEMPO_HEIGHT: f32 = 18.0;
const ROW_HEIGHT: f32 = 64.0;
/// The zoom range, in pixels per beat.
const MIN_ZOOM: f32 = 4.0;
const MAX_ZOOM: f32 = 400.0;
/// Above this zoom the grid is sixteenths rather than beats.
const FINE_GRID_ZOOM: f32 = 80.0;
/// How close to a MIDI clip's end the pointer must be to trim it rather than
/// move it.
const EDGE: f32 = 6.0;

/// What a drag on a clip is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    /// Picked up this many ticks after its start, so it moves with the
    /// pointer instead of jumping its start to it.
    Move {
        id: PlacementId,
        grab: f64,
    },
    TrimStart {
        id: PlacementId,
    },
    TrimEnd {
        id: PlacementId,
    },
}

/// How the timeline is being looked at. Not part of the project.
#[derive(Debug, Clone)]
pub struct View {
    pub pixels_per_beat: f32,
    /// The tick at the left edge of the lanes. Fractional, so scrolling is
    /// smooth at any zoom.
    pub scroll: f64,
    /// Pixels scrolled down past the first track.
    pub scroll_y: f32,
    pub selected: Option<PlacementId>,
    /// A MIDI clip double-clicked, for the note editor to open on.
    pub open: Option<PlacementId>,
    drag: Option<Drag>,
    /// Where a shift-drag on the ruler started, drawing the loop region.
    loop_drag_start: Option<Tick>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            pixels_per_beat: 40.0,
            scroll: 0.0,
            scroll_y: 0.0,
            selected: None,
            open: None,
            drag: None,
            loop_drag_start: None,
        }
    }
}

impl View {
    /// Pixels from the lanes' left edge to a tick.
    pub fn x_of(&self, tick: Tick) -> f32 {
        ((tick.0 as f64 - self.scroll) / PPQ as f64 * f64::from(self.pixels_per_beat)) as f32
    }

    /// The tick under a point this far from the lanes' left edge. Negative
    /// left of the timeline's start.
    pub fn tick_at(&self, x: f32) -> f64 {
        self.scroll + f64::from(x) / f64::from(self.pixels_per_beat) * PPQ as f64
    }

    /// Pixels a length of ticks spans.
    pub fn width_of(&self, ticks: u64) -> f32 {
        (ticks as f64 / PPQ as f64 * f64::from(self.pixels_per_beat)) as f32
    }

    /// The grid clips snap to: beats, or sixteenths once zoomed in enough to
    /// place them by eye.
    pub fn grid(&self, meter: Meter) -> u64 {
        if self.pixels_per_beat >= FINE_GRID_ZOOM { PPQ / 4 } else { meter.beat_length().0 }
    }

    /// Zooms by `factor` keeping the tick under `anchor` (pixels from the
    /// lanes' left edge) where it is, as zooming towards the pointer should.
    pub fn zoom(&mut self, factor: f32, anchor: f32) {
        let fixed = self.tick_at(anchor);
        self.pixels_per_beat = (self.pixels_per_beat * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.scroll = (fixed - f64::from(anchor) / f64::from(self.pixels_per_beat) * PPQ as f64).max(0.0);
    }
}

/// A tick rounded to the nearest line of `grid`, never before the start.
/// `None` for the grid leaves it where it is, rounded to a whole tick.
pub fn snap(ticks: f64, grid: Option<u64>) -> Tick {
    let ticks = ticks.max(0.0);
    match grid {
        Some(grid) if grid > 0 => Tick(((ticks / grid as f64).round() as u64) * grid),
        _ => Tick(ticks.round() as u64),
    }
}

/// Where a clip picked up `grab` ticks into itself lands, with the pointer
/// `x` pixels from the lanes' left edge.
pub fn drop_at(view: &View, x: f32, grab: f64, grid: Option<u64>) -> Tick {
    snap(view.tick_at(x) - grab, grid)
}

/// The loop region a shift-drag on the ruler is drawing, from where it
/// started to where the pointer is now — in either order, both snapped to the
/// beat. `None` while the two ends have not yet parted, so a drag that has not
/// moved anywhere does not collapse the loop to nothing.
pub fn loop_drag_region(start: Tick, now: Tick) -> Option<LoopRegion> {
    let (lo, hi) = (start.min(now), start.max(now));
    (hi > lo).then_some(LoopRegion { start: lo, end: hi })
}

/// Which row a point this far below the top of the lanes falls in, clamped to
/// the rows there are.
pub fn row_at(view: &View, y: f32, rows: usize) -> Option<usize> {
    let row = ((y + view.scroll_y) / ROW_HEIGHT).floor();
    (rows > 0).then(|| (row.max(0.0) as usize).min(rows - 1))
}

/// The window a placement plays: `None` when it covers the whole clip, so a
/// clip trimmed back out to its full length is exactly what it was.
fn window(offset: u64, length: u64, clip: u64) -> Option<Window> {
    (offset != 0 || length != clip).then_some(Window { offset: Tick(offset), length: Tick(length) })
}

/// The end of a MIDI placement dragged to `pointer`: at least a grid step
/// long, and never past the end of its clip.
pub fn trim_end(start: Tick, offset: u64, clip: u64, pointer: f64, grid: Option<u64>) -> Option<Window> {
    let room = clip.saturating_sub(offset).max(1);
    let step = grid.unwrap_or(1).clamp(1, room);
    let length = snap(pointer, grid).0.saturating_sub(start.0).clamp(step, room);
    window(offset, length, clip)
}

/// The start of a MIDI placement dragged to `pointer`, its end staying where
/// it is in time. The window's offset moves by the same amount as the start,
/// so every note keeps its place on the timeline; it cannot move before the
/// clip's own beginning, nor closer than a grid step to the end.
pub fn trim_start(
    start: Tick,
    offset: u64,
    length: u64,
    clip: u64,
    pointer: f64,
    grid: Option<u64>,
) -> (Tick, Option<Window>) {
    let end = start.0 + length;
    let step = grid.unwrap_or(1).clamp(1, length.max(1));
    let earliest = start.0.saturating_sub(offset);
    let latest = end.saturating_sub(step).max(earliest);
    let new_start = snap(pointer, grid).0.clamp(earliest, latest);
    let new_offset = offset + new_start - earliest - (start.0 - earliest);
    (Tick(new_start), window(new_offset, end - new_start, clip))
}

/// The part of its file an audio placement plays, as offset and length in
/// frames at `rate`, cut short by the end of a file `file` frames long.
fn audio_part(placement: &Placement, file: u64, rate: u32) -> (u64, u64) {
    let (offset, length) = placement.audio.map_or((0, file), |w| w.at(rate));
    let offset = offset.min(file);
    (offset, length.min(file - offset))
}

/// An audio window, or `None` when it is the whole file.
fn audio_window(offset: u64, length: u64, file: u64, rate: u32) -> Option<AudioWindow> {
    (offset != 0 || length < file).then_some(AudioWindow { offset, length, rate })
}

/// The end of an audio placement dragged to the frame `pointer`: at least
/// `min` frames long, and never past the end of the file.
pub fn trim_audio_end(start: u64, offset: u64, file: u64, pointer: u64, min: u64) -> u64 {
    let room = file.saturating_sub(offset).max(1);
    pointer.saturating_sub(start).clamp(min.clamp(1, room), room)
}

/// The start of an audio placement dragged to the tick `pointer`, its end
/// staying where it is in time and the file's offset moving by the same
/// frames, so what is heard stays where it was. Returns the new start, offset
/// and length.
///
/// The start is a tick and the offset a frame, so the start is settled first
/// and the offset worked out from the frame that tick actually falls on — the
/// sound cannot slide by the rounding between the two.
pub fn trim_audio_start(
    start: Tick,
    offset: u64,
    length: u64,
    pointer: Tick,
    min: u64,
    timing: &Timing,
) -> (Tick, u64, u64) {
    let at = timing.frame_of(start);
    let end = at + length;
    let earliest = at.saturating_sub(offset);
    let latest = end.saturating_sub(min.max(1)).max(earliest);
    let wanted = timing.frame_of(pointer).clamp(earliest, latest);
    // Back to a tick, then forward to the first frame not before `earliest`.
    let mut tick = timing.tick_of(wanted);
    if timing.frame_of(tick) < earliest {
        tick.0 += 1;
    }
    let frame = timing.frame_of(tick);
    if frame >= end {
        return (start, offset, length);
    }
    (tick, offset + frame - earliest - (at - earliest), end - frame)
}

/// How long a placement is on the timeline, in ticks.
///
/// A MIDI clip knows its length; an audio clip lasts as long as its file,
/// which is frames, turned into ticks at the current tempo. An audio file not
/// read yet is drawn a bar long, so it can still be seen and grabbed.
fn length_of(
    project: &Project,
    placement: &Placement,
    timing: &Timing,
    peaks: &dyn Fn(AssetId) -> Option<Arc<Peaks>>,
) -> u64 {
    let Some(clip) = project.clip(placement.clip) else { return 0 };
    match &clip.content {
        Content::Midi { length, .. } => placement.window.map_or(length.0, |w| w.length.0),
        Content::Audio { asset } => match peaks(*asset) {
            Some(peaks) => {
                let (_, frames) = audio_part(placement, peaks.frames(), timing.sample_rate);
                let start = timing.frame_of(placement.start);
                timing.tick_of(start + frames).0.saturating_sub(placement.start.0).max(1)
            }
            None => project.meter().bar_length().0,
        },
    }
}

/// Everything the timeline needs besides the project, bundled so the call
/// stays readable.
pub struct Context<'a> {
    pub theme: &'a Theme,
    pub timing: Timing,
    /// Where playback is, in timeline frames.
    pub playhead: u64,
    pub playing: bool,
    pub peaks: &'a dyn Fn(AssetId) -> Option<Arc<Peaks>>,
    /// Whether the keyboard is the timeline's: the last click was in it.
    pub keys: bool,
}

pub fn show(ui: &mut Ui, project: &Project, view: &mut View, cx: &Context<'_>, actions: &mut Actions) {
    let theme = cx.theme;
    let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    let ruler = Rect::from_min_max(
        Pos2::new(rect.left() + HEADER_WIDTH, rect.top()),
        Pos2::new(rect.right(), rect.top() + RULER_HEIGHT),
    );
    let tempo = Rect::from_min_max(ruler.left_bottom(), Pos2::new(ruler.right(), ruler.bottom() + TEMPO_HEIGHT));
    let lanes = Rect::from_min_max(Pos2::new(ruler.left(), tempo.bottom()), rect.right_bottom());
    let headers = Rect::from_min_max(Pos2::new(rect.left(), tempo.bottom()), Pos2::new(ruler.left(), rect.bottom()));
    let tracks: Vec<TrackId> = project.tracks().map(|t| t.id).collect();
    let meter = project.meter();

    scroll_and_zoom(ui, view, lanes, tracks.len());
    follow_playhead(view, cx, lanes);

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.background);
    // Fixed for the frame once the wheel has been read.
    let scroll_y = view.scroll_y;
    let row_top = move |row: usize| lanes.top() + row as f32 * ROW_HEIGHT - scroll_y;

    // Rows, then the grid over them: bars stronger than beats.
    for (row, _) in tracks.iter().enumerate() {
        let top = row_top(row);
        let band = Rect::from_min_size(Pos2::new(lanes.left(), top), Vec2::new(lanes.width(), ROW_HEIGHT));
        let fill = if row % 2 == 0 { theme.panel } else { theme.background };
        painter.with_clip_rect(lanes).rect_filled(band, 0.0, fill);
    }
    draw_grid(&painter.with_clip_rect(lanes), view, lanes, meter, theme);

    // Empty lanes: a click clears the selection, a double click starts a
    // one-bar MIDI clip there. Registered before the clips, so a clip on top
    // takes the pointer first.
    let background = ui.interact(lanes, ui.id().with("lanes"), Sense::click());
    if background.clicked() {
        view.selected = None;
    }
    if background.double_clicked()
        && let Some(pos) = background.interact_pointer_pos()
        && let Some(row) = row_at(view, pos.y - lanes.top(), tracks.len())
    {
        let start =
            snap(view.tick_at(pos.x - lanes.left()) - meter.bar_length().0 as f64 / 2.0, Some(meter.bar_length().0));
        actions.commands.push(Command::AddMidiClip {
            track: tracks[row],
            name: "MIDI".into(),
            start,
            length: meter.bar_length(),
        });
    }

    let clip_painter = painter.with_clip_rect(lanes);
    for placement in project.placements() {
        let Some(row) = tracks.iter().position(|t| *t == placement.track) else { continue };
        let length = length_of(project, placement, &cx.timing, cx.peaks);
        let x = lanes.left() + view.x_of(placement.start);
        let body = Rect::from_min_size(
            Pos2::new(x, row_top(row) + 3.0),
            Vec2::new(view.width_of(length).max(4.0), ROW_HEIGHT - 6.0),
        );
        if !body.intersects(lanes) {
            continue;
        }
        draw_clip(&clip_painter, project, placement, body, view.selected == Some(placement.id), cx);

        let response =
            ui.interact(body.intersect(lanes), ui.id().with(("placement", placement.id.0)), Sense::click_and_drag());
        handle_clip(ui, response, project, placement, length, view, lanes, &tracks, cx, actions);
    }

    if ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(rect) {
        actions.keys_to_editor = Some(false);
    }
    if cx.keys {
        keyboard(ui, project, view, cx, actions);
    }

    // The playhead, over the clips.
    let head = lanes.left() + view.x_of(cx.timing.tick_of(cx.playhead));
    if (lanes.left()..=lanes.right()).contains(&head) {
        painter.line_segment(
            [Pos2::new(head, ruler.top()), Pos2::new(head, lanes.bottom())],
            Stroke::new(1.5, theme.accent),
        );
    }

    draw_ruler(ui, &painter, view, ruler, project, cx, actions);
    tempo_lane(ui, &painter, project, view, tempo, cx.theme, actions);
    draw_headers(&painter.with_clip_rect(headers), project, headers, &tracks, row_top, theme);
}

fn scroll_and_zoom(ui: &Ui, view: &mut View, lanes: Rect, rows: usize) {
    let hovered = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| lanes.contains(p));
    if !hovered {
        return;
    }
    let (delta, zoom, pointer) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.command, i.pointer.hover_pos()));
    if zoom {
        if delta.y != 0.0 {
            let anchor = pointer.map_or(0.0, |p| p.x - lanes.left());
            view.zoom((delta.y * 0.004).exp(), anchor);
        }
        return;
    }
    // Sideways — a trackpad, or the wheel with shift, which egui turns into a
    // horizontal scroll — moves through time; up and down through tracks.
    view.scroll = (view.scroll - f64::from(delta.x) / f64::from(view.pixels_per_beat) * PPQ as f64).max(0.0);
    let content = rows as f32 * ROW_HEIGHT;
    view.scroll_y = (view.scroll_y - delta.y).clamp(0.0, (content - lanes.height()).max(0.0));
}

/// While playing, keeps the playhead in view by turning the page when it
/// reaches the right edge.
fn follow_playhead(view: &mut View, cx: &Context<'_>, lanes: Rect) {
    if !cx.playing || view.drag.is_some() {
        return;
    }
    let head = view.x_of(cx.timing.tick_of(cx.playhead));
    if head > lanes.width() * 0.9 || head < 0.0 {
        view.scroll = (view.tick_at(head) - view.tick_at(lanes.width() * 0.1) + view.scroll).max(0.0);
    }
}

fn draw_grid(painter: &egui::Painter, view: &View, lanes: Rect, meter: Meter, theme: &Theme) {
    let beat = meter.beat_length().0;
    let bar = meter.bar_length().0;
    // Beat lines only once they are far enough apart to read.
    let step = if view.width_of(beat) >= 12.0 { beat } else { bar };
    let first = (view.tick_at(0.0).max(0.0) as u64 / step) * step;
    let mut tick = first;
    while view.x_of(Tick(tick)) <= lanes.width() {
        let x = lanes.left() + view.x_of(Tick(tick));
        let colour = if tick.is_multiple_of(bar) { theme.control } else { theme.panel.gamma_multiply(1.4) };
        painter.line_segment([Pos2::new(x, lanes.top()), Pos2::new(x, lanes.bottom())], Stroke::new(1.0, colour));
        tick += step;
    }
}

fn draw_clip(
    painter: &egui::Painter,
    project: &Project,
    placement: &Placement,
    body: Rect,
    selected: bool,
    cx: &Context<'_>,
) {
    let theme = cx.theme;
    let Some(clip) = project.clip(placement.clip) else { return };
    let shared = project.uses_of_clip(clip.id);
    painter.rect_filled(body, 3.0, theme.control);
    let outline = if selected { Stroke::new(2.0, theme.accent) } else { Stroke::new(1.0, theme.hover) };
    painter.rect_stroke(body, 3.0, outline, StrokeKind::Inside);

    let inner = body.shrink2(Vec2::new(2.0, 16.0)).translate(Vec2::new(0.0, 6.0));
    match &clip.content {
        Content::Audio { asset } => match (cx.peaks)(*asset) {
            Some(peaks) => {
                let part = audio_part(placement, peaks.frames(), cx.timing.sample_rate);
                draw_waveform(painter, &peaks, part, inner, body, cx);
            }
            // Named by the project but not read: say so where it is drawn.
            None => {
                painter.text(
                    inner.left_center(),
                    Align2::LEFT_CENTER,
                    "file not loaded",
                    FontId::proportional(11.0),
                    theme.warning,
                );
            }
        },
        Content::Midi { length, notes } => {
            let (offset, span) = placement.window.map_or((0, length.0), |w| (w.offset.0, w.length.0));
            let pitches = notes.values().map(|n| n.pitch);
            let (low, high) = (pitches.clone().min().unwrap_or(60), pitches.max().unwrap_or(60));
            let range = f32::from(high - low + 1);
            for note in notes.values() {
                let (start, end) = (note.start.0.max(offset), note.end().0.min(offset + span));
                if start >= end {
                    continue;
                }
                let x0 = body.left() + (start - offset) as f32 / span as f32 * body.width();
                let x1 = body.left() + (end - offset) as f32 / span as f32 * body.width();
                let y = inner.bottom() - (f32::from(note.pitch - low) + 1.0) / range * inner.height();
                let bar = Rect::from_min_max(
                    Pos2::new(x0, y),
                    Pos2::new(x1.max(x0 + 1.0), y + (inner.height() / range).max(2.0)),
                );
                painter.rect_filled(bar, 1.0, theme.accent);
            }
        }
    }

    // The name, and how many placements play this clip when more than one:
    // editing it changes all of them.
    let label = if shared > 1 { format!("{} ×{shared}", clip.name) } else { clip.name.clone() };
    let colour = if shared > 1 { theme.soloed } else { theme.text };
    painter.with_clip_rect(body.intersect(painter.clip_rect())).text(
        body.left_top() + Vec2::new(5.0, 3.0),
        Align2::LEFT_TOP,
        label,
        FontId::proportional(11.0),
        colour,
    );
}

/// Draws the part of a file a placement plays, `(offset, length)` in frames,
/// across the clip's body.
fn draw_waveform(painter: &egui::Painter, peaks: &Peaks, part: (u64, u64), inner: Rect, body: Rect, cx: &Context<'_>) {
    let visible = body.intersect(painter.clip_rect());
    let centre = inner.center().y;
    let half = inner.height() / 2.0;
    let frames_per_pixel = part.1 as f64 / f64::from(body.width().max(1.0));
    let colour = cx.theme.accent.gamma_multiply(0.8);
    // One line per visible pixel, from the lowest sample under it to the highest.
    let mut x = visible.left().floor();
    while x < visible.right() {
        let from = part.0 + ((f64::from(x - body.left())) * frames_per_pixel).max(0.0) as u64;
        let to = part.0 + ((f64::from(x + 1.0 - body.left())) * frames_per_pixel).ceil() as u64;
        if let Some((low, high)) = peaks.range(from, to.max(from + 1)) {
            let (top, bottom) = (centre - high.clamp(-1.0, 1.0) * half, centre - low.clamp(-1.0, 1.0) * half);
            painter.line_segment([Pos2::new(x, top), Pos2::new(x, bottom.max(top + 1.0))], Stroke::new(1.0, colour));
        }
        x += 1.0;
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_clip(
    ui: &Ui,
    response: egui::Response,
    project: &Project,
    placement: &Placement,
    length: u64,
    view: &mut View,
    lanes: Rect,
    tracks: &[TrackId],
    cx: &Context<'_>,
    actions: &mut Actions,
) {
    let meter = project.meter();
    if response.clicked() || response.drag_started() {
        view.selected = Some(placement.id);
    }
    if response.double_clicked() && project.clip(placement.clip).is_some_and(|c| c.length().is_some()) {
        view.open = Some(placement.id);
    }
    // Either end of a clip trims it: a MIDI clip in ticks, an audio clip in
    // frames — once its file has been read, since its length is the file's.
    let clip_length = project.clip(placement.clip).and_then(|c| c.length()).map(|l| l.0);
    let file = project.clip(placement.clip).and_then(|c| c.asset()).and_then(|a| (cx.peaks)(a)).map(|p| p.frames());
    let edge = |pos: Pos2| {
        let body = response.rect;
        clip_length.or(file).and_then(|_| {
            if pos.x <= body.left() + EDGE {
                Some(Drag::TrimStart { id: placement.id })
            } else if pos.x >= body.right() - EDGE {
                Some(Drag::TrimEnd { id: placement.id })
            } else {
                None
            }
        })
    };
    if let Some(pos) = response.hover_pos()
        && edge(pos).is_some()
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if response.drag_started()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let grab = view.tick_at(pos.x - lanes.left()) - placement.start.0 as f64;
        view.drag = Some(edge(pos).unwrap_or(Drag::Move { id: placement.id, grab }));
    }
    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        // Alt lets go of the grid, for placing by ear.
        let grid = (!ui.input(|i| i.modifiers.alt)).then(|| view.grid(meter));
        let pointer = view.tick_at(pos.x - lanes.left());
        let (offset, played) = placement.window.map_or((0, length), |w| (w.offset.0, w.length.0));
        // Sent on every frame the clip changes; the history folds them into
        // one step, closed when the mouse is let go.
        match view.drag {
            Some(Drag::Move { id, grab }) if id == placement.id => {
                let start = drop_at(view, pos.x - lanes.left(), grab, grid);
                let track = row_at(view, pos.y - lanes.top(), tracks.len()).map_or(placement.track, |row| tracks[row]);
                if start != placement.start || track != placement.track {
                    actions.commands.push(Command::MovePlacement { id, track, start });
                }
            }
            Some(Drag::TrimEnd { id }) if id == placement.id => match (clip_length, file) {
                (Some(clip), _) => {
                    let window = trim_end(placement.start, offset, clip, pointer, grid);
                    if window != placement.window {
                        actions.commands.push(Command::TrimPlacement {
                            id,
                            start: placement.start,
                            window,
                            audio: None,
                        });
                    }
                }
                (None, Some(file)) => {
                    let rate = cx.timing.sample_rate;
                    let (offset, _) = audio_part(placement, file, rate);
                    let end = cx.timing.frame_of(snap(pointer, grid));
                    let min = cx.timing.frame_of(Tick(grid.unwrap_or(1)));
                    let length = trim_audio_end(cx.timing.frame_of(placement.start), offset, file, end, min);
                    let audio = audio_window(offset, length, file, rate);
                    if audio != placement.audio {
                        actions.commands.push(Command::TrimPlacement {
                            id,
                            start: placement.start,
                            window: None,
                            audio,
                        });
                    }
                }
                _ => {}
            },
            Some(Drag::TrimStart { id }) if id == placement.id => match (clip_length, file) {
                (Some(clip), _) => {
                    let (start, window) = trim_start(placement.start, offset, played, clip, pointer, grid);
                    if start != placement.start || window != placement.window {
                        actions.commands.push(Command::TrimPlacement { id, start, window, audio: None });
                    }
                }
                (None, Some(file)) => {
                    let rate = cx.timing.sample_rate;
                    let (offset, length) = audio_part(placement, file, rate);
                    let min = cx.timing.frame_of(Tick(grid.unwrap_or(1)));
                    let (start, offset, length) =
                        trim_audio_start(placement.start, offset, length, snap(pointer, grid), min, &cx.timing);
                    let audio = audio_window(offset, length, file, rate);
                    if start != placement.start || audio != placement.audio {
                        actions.commands.push(Command::TrimPlacement { id, start, window: None, audio });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if response.drag_stopped() {
        view.drag = None;
        actions.seal = true;
    }

    let shared = project.uses_of_clip(placement.clip) > 1;
    let after = Tick(placement.start.0 + length);
    response.context_menu(|ui| {
        if ui.button("Duplicate").clicked() {
            actions.commands.push(Command::DuplicatePlacement {
                id: placement.id,
                track: placement.track,
                start: after,
                linked: false,
            });
            ui.close();
        }
        if ui.button("Duplicate linked").clicked() {
            actions.commands.push(Command::DuplicatePlacement {
                id: placement.id,
                track: placement.track,
                start: after,
                linked: true,
            });
            ui.close();
        }
        if ui.add_enabled(shared, egui::Button::new("Make unique")).clicked() {
            actions.commands.push(Command::MakeUnique { id: placement.id });
            ui.close();
        }
        ui.separator();
        if ui.button("Delete").clicked() {
            actions.commands.push(Command::RemovePlacement { id: placement.id });
            ui.close();
        }
    });
}

/// Delete removes the selected clip; Ctrl+D copies it to just after itself.
fn keyboard(ui: &Ui, project: &Project, view: &mut View, cx: &Context<'_>, actions: &mut Actions) {
    let Some(id) = view.selected else { return };
    let Some(placement) = project.placement(id) else {
        // Removed by an undo, or by the other view.
        view.selected = None;
        return;
    };
    if ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    let (delete, duplicate) = ui.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
                || i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace),
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::D),
        )
    });
    if delete {
        actions.commands.push(Command::RemovePlacement { id });
        view.selected = None;
    }
    if duplicate {
        let start = Tick(placement.start.0 + length_of(project, placement, &cx.timing, cx.peaks));
        actions.commands.push(Command::DuplicatePlacement { id, track: placement.track, start, linked: false });
    }
}

/// The ruler: bar numbers, the playhead's seek gesture, and — held with
/// shift — the loop region, drawn as a band along its bottom.
fn draw_ruler(
    ui: &Ui,
    painter: &egui::Painter,
    view: &mut View,
    ruler: Rect,
    project: &Project,
    cx: &Context<'_>,
    actions: &mut Actions,
) {
    let theme = cx.theme;
    let meter = project.meter();
    painter.rect_filled(ruler, 0.0, theme.panel);
    let bar = meter.bar_length().0;
    // Label every bar, or every few once they crowd together.
    let every = (48.0 / view.width_of(bar)).ceil().max(1.0) as u64;
    let first = view.tick_at(0.0).max(0.0) as u64 / bar;
    let ruler_painter = painter.with_clip_rect(ruler);
    let mut index = first - first % every;
    while view.x_of(Tick(index * bar)) <= ruler.width() {
        let x = ruler.left() + view.x_of(Tick(index * bar));
        ruler_painter.line_segment(
            [Pos2::new(x, ruler.bottom() - 6.0), Pos2::new(x, ruler.bottom())],
            Stroke::new(1.0, theme.dim),
        );
        ruler_painter.text(
            Pos2::new(x + 3.0, ruler.top() + 3.0),
            Align2::LEFT_TOP,
            (index + 1).to_string(),
            FontId::monospace(11.0),
            theme.dim,
        );
        index += every;
    }

    // The loop region, along the ruler's bottom edge, lit while it repeats.
    if let Some(region) = project.loop_region() {
        let x0 = (ruler.left() + view.x_of(region.start)).clamp(ruler.left(), ruler.right());
        let x1 = (ruler.left() + view.x_of(region.end)).clamp(ruler.left(), ruler.right());
        if x1 > x0 {
            let band = Rect::from_min_max(Pos2::new(x0, ruler.bottom() - 4.0), Pos2::new(x1, ruler.bottom()));
            let colour = if project.loop_enabled() { theme.accent } else { theme.dim };
            ruler_painter.rect_filled(band, 0.0, colour);
        }
    }

    // Clicking or dragging along the ruler moves playback there; held with
    // shift, a drag draws the loop region instead — a gesture kept apart from
    // scrubbing since both would otherwise fight over the same drag.
    let response = ui.interact(ruler, ui.id().with("ruler"), Sense::click_and_drag());
    let shift = ui.input(|i| i.modifiers.shift);
    if shift {
        if response.drag_started()
            && let Some(pos) = response.interact_pointer_pos()
        {
            view.loop_drag_start = Some(snap(view.tick_at(pos.x - ruler.left()), Some(meter.beat_length().0)));
        }
        if response.dragged()
            && let (Some(start), Some(pos)) = (view.loop_drag_start, response.interact_pointer_pos())
        {
            let now = snap(view.tick_at(pos.x - ruler.left()), Some(meter.beat_length().0));
            if let Some(region) = loop_drag_region(start, now) {
                actions.commands.push(Command::SetLoopRegion { region: Some(region) });
                if !project.loop_enabled() {
                    actions.commands.push(Command::SetLoopEnabled { enabled: true });
                }
            }
        }
        if response.drag_stopped() {
            actions.seal = true;
            view.loop_drag_start = None;
        }
    } else if (response.clicked() || response.dragged())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let tick = snap(view.tick_at(pos.x - ruler.left()), None);
        actions.transport = Some(TransportCommand::Seek(cx.timing.frame_of(tick)));
    }
    if project.loop_region().is_some() {
        response.context_menu(|ui| {
            if ui.button("Clear loop").clicked() {
                actions.commands.push(Command::SetLoopRegion { region: None });
                actions.commands.push(Command::SetLoopEnabled { enabled: false });
                ui.close();
            }
        });
    }
}

/// The tempo map, as markers along a lane under the ruler: each change shows
/// its tempo from where it takes effect. A double click adds one on the bar;
/// a marker drags along the beats; its menu sets its tempo or removes it.
fn tempo_lane(
    ui: &Ui,
    painter: &egui::Painter,
    project: &Project,
    view: &View,
    lane: Rect,
    theme: &Theme,
    actions: &mut Actions,
) {
    let meter = project.meter();
    painter.rect_filled(lane, 0.0, theme.background);
    let lane_painter = painter.with_clip_rect(lane);

    let background = ui.interact(lane, ui.id().with("tempo-lane"), Sense::click());
    if background.double_clicked()
        && let Some(pos) = background.interact_pointer_pos()
    {
        let at = snap(view.tick_at(pos.x - lane.left()), Some(meter.bar_length().0));
        let tempo = project.tempo_map().tempo_at(at);
        actions.commands.push(Command::AddTempoChange { at, tempo });
    }

    for (index, change) in project.tempo_map().changes().iter().enumerate() {
        let x = lane.left() + view.x_of(change.at);
        let label = format!("{:.1}", change.tempo.bpm());
        let galley = painter.layout_no_wrap(label, FontId::proportional(10.0), theme.text);
        let flag =
            Rect::from_min_size(Pos2::new(x, lane.top() + 2.0), Vec2::new(galley.size().x + 8.0, lane.height() - 4.0));
        if flag.right() < lane.left() || flag.left() > lane.right() {
            continue;
        }
        lane_painter.rect_filled(flag, 2.0, theme.control);
        lane_painter
            .line_segment([Pos2::new(x, lane.top()), Pos2::new(x, lane.bottom())], Stroke::new(1.5, theme.soloed));
        lane_painter.galley(flag.left_top() + Vec2::new(5.0, 1.0), galley, theme.text);

        let response = ui.interact(flag.intersect(lane), ui.id().with(("tempo", index)), Sense::click_and_drag());
        // The first holds the start and does not move.
        if index > 0
            && response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let grid = (!ui.input(|i| i.modifiers.alt)).then(|| meter.beat_length().0);
            let to = snap(view.tick_at(pos.x - lane.left()), grid);
            if to != change.at {
                actions.commands.push(Command::MoveTempoChange { index, to });
            }
        }
        if response.drag_stopped() {
            actions.seal = true;
        }
        response.context_menu(|ui| {
            let mut bpm = change.tempo.bpm();
            let range = project_model::Tempo::MIN.bpm()..=project_model::Tempo::MAX.bpm();
            let field = ui.add(egui::DragValue::new(&mut bpm).range(range).speed(0.1).max_decimals(3).suffix(" bpm"));
            if field.changed()
                && let Some(tempo) = project_model::Tempo::from_bpm(bpm)
            {
                actions.commands.push(Command::SetTempoChange { index, tempo });
            }
            if field.drag_stopped() || field.lost_focus() {
                actions.seal = true;
            }
            if index > 0 && ui.button("Remove").clicked() {
                actions.commands.push(Command::RemoveTempoChange { index });
                ui.close();
            }
        });
    }
    painter.text(
        lane.left_top() - Vec2::new(HEADER_WIDTH - 8.0, -3.0),
        Align2::LEFT_TOP,
        "tempo",
        FontId::proportional(10.0),
        theme.dim,
    );
}

fn draw_headers(
    painter: &egui::Painter,
    project: &Project,
    headers: Rect,
    tracks: &[TrackId],
    row_top: impl Fn(usize) -> f32,
    theme: &Theme,
) {
    painter.rect_filled(headers, 0.0, theme.panel);
    for (row, id) in tracks.iter().enumerate() {
        let Some(track) = project.track(*id) else { continue };
        let top = row_top(row);
        let colour = if !project.is_audible(*id) {
            theme.muted
        } else if track.soloed {
            theme.soloed
        } else {
            theme.text
        };
        painter.text(
            Pos2::new(headers.left() + 8.0, top + 8.0),
            Align2::LEFT_TOP,
            &track.name,
            FontId::proportional(13.0),
            colour,
        );
        painter.text(
            Pos2::new(headers.left() + 8.0, top + 28.0),
            Align2::LEFT_TOP,
            project.playing(track),
            FontId::proportional(11.0),
            theme.dim,
        );
        painter.line_segment(
            [Pos2::new(headers.left(), top + ROW_HEIGHT), Pos2::new(headers.right(), top + ROW_HEIGHT)],
            Stroke::new(1.0, theme.background),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(pixels_per_beat: f32, scroll: f64) -> View {
        View { pixels_per_beat, scroll, ..View::default() }
    }

    #[test]
    fn a_tick_and_its_pixel_agree_both_ways() {
        let v = view(40.0, 960.0);
        assert_eq!(v.x_of(Tick(960)), 0.0, "the scrolled-to tick is at the left edge");
        assert_eq!(v.x_of(Tick(1_920)), 40.0);
        assert_eq!(v.tick_at(40.0), 1_920.0);
        assert!(v.tick_at(-40.0) < 960.0);
    }

    #[test]
    fn snapping_goes_to_the_nearest_line_and_never_before_the_start() {
        assert_eq!(snap(1_400.0, Some(960)), Tick(960));
        assert_eq!(snap(1_500.0, Some(960)), Tick(1_920));
        assert_eq!(snap(-300.0, Some(960)), Tick(0));
        assert_eq!(snap(1_234.4, None), Tick(1_234));
    }

    #[test]
    fn a_dragged_clip_keeps_the_point_it_was_picked_up_by() {
        // Picked up a beat into itself; the pointer now over beat 5. Its start
        // goes to beat 4, not to where the pointer is.
        let v = view(40.0, 0.0);
        let at = drop_at(&v, 5.0 * 40.0, 960.0, Some(960));
        assert_eq!(at, Tick(4 * 960));
    }

    #[test]
    fn a_loop_drag_spans_from_where_it_started_however_it_is_dragged() {
        // Dragged forward: the region runs from the start to the pointer.
        let region = loop_drag_region(Tick(960), Tick(3_840)).unwrap();
        assert_eq!(region, LoopRegion { start: Tick(960), end: Tick(3_840) });
        // Dragged backward past where it started: still start before end, not
        // the order the two ends were visited in.
        let region = loop_drag_region(Tick(3_840), Tick(960)).unwrap();
        assert_eq!(region, LoopRegion { start: Tick(960), end: Tick(3_840) });
        // No distance covered yet: no region to show or send, rather than one
        // collapsed to nothing that `LoopRegion` itself refuses.
        assert_eq!(loop_drag_region(Tick(960), Tick(960)), None);
    }

    #[test]
    fn the_grid_gets_finer_when_zoomed_in() {
        let meter = Meter::default();
        assert_eq!(view(40.0, 0.0).grid(meter), 960);
        assert_eq!(view(120.0, 0.0).grid(meter), 240);
        assert_eq!(view(40.0, 0.0).grid(Meter::new(6, 8).unwrap()), 480, "a beat of 6/8 is an eighth");
    }

    #[test]
    fn zooming_keeps_the_tick_under_the_pointer_still() {
        let mut v = view(40.0, 3_840.0);
        let before = v.tick_at(200.0);
        v.zoom(2.0, 200.0);
        assert_eq!(v.pixels_per_beat, 80.0);
        assert!((v.tick_at(200.0) - before).abs() < 1e-6, "the timeline slid under the pointer");
        v.zoom(1_000.0, 0.0);
        assert_eq!(v.pixels_per_beat, MAX_ZOOM);
    }

    #[test]
    fn trimming_the_end_stays_between_a_step_and_the_clips_end() {
        // A one-bar clip placed at bar 2, played whole.
        let start = Tick(3_840);
        assert_eq!(
            trim_end(start, 0, 3_840, 3_840.0 + 1_920.0, Some(960)),
            Some(Window { offset: Tick(0), length: Tick(1_920) })
        );
        assert_eq!(trim_end(start, 0, 3_840, 99_999.0, Some(960)), None, "stretched back to the whole clip");
        assert_eq!(trim_end(start, 0, 3_840, 0.0, Some(960)).map(|w| w.length), Some(Tick(960)), "trimmed to nothing");
    }

    #[test]
    fn trimming_the_front_moves_start_and_offset_together() {
        // A one-bar clip at bar 2; the front dragged a beat later.
        let (start, window) = trim_start(Tick(3_840), 0, 3_840, 3_840, 3_840.0 + 960.0, Some(960));
        assert_eq!(start, Tick(4_800));
        assert_eq!(window, Some(Window { offset: Tick(960), length: Tick(2_880) }));
        // Anything the clip held at 1 920 still sounds at 3 840 + 1 920.
        let w = window.unwrap();
        assert_eq!(start.0 + 1_920 - w.offset.0, 3_840 + 1_920);

        // Pulled back past the clip's own start: it stops there, whole again.
        assert_eq!(trim_start(Tick(4_800), 960, 2_880, 3_840, 0.0, Some(960)), (Tick(3_840), None));
        // Pushed up to the end: a grid step is left.
        let (late, w) = trim_start(Tick(3_840), 0, 3_840, 3_840, 99_999.0, Some(960));
        assert_eq!((late, w.map(|w| w.length)), (Tick(6_720), Some(Tick(960))));
    }

    fn at_120() -> Timing {
        // A beat is 24 000 frames, a tick 25.
        Timing::constant(project_model::Tempo::default(), 48_000)
    }

    #[test]
    fn trimming_an_audio_clips_end_stays_inside_the_file() {
        // A four-second file placed at frame 96 000.
        let file = 192_000;
        assert_eq!(trim_audio_end(96_000, 0, file, 96_000 + 48_000, 24_000), 48_000);
        assert_eq!(trim_audio_end(96_000, 0, file, 999_999, 24_000), file, "stretched past the file's end");
        assert_eq!(trim_audio_end(96_000, 0, file, 0, 24_000), 24_000, "trimmed to nothing");
        assert_eq!(trim_audio_end(96_000, 48_000, file, 999_999, 1), file - 48_000);
    }

    #[test]
    fn trimming_an_audio_clips_front_keeps_the_sound_where_it_was() {
        let timing = at_120();
        // The whole file at bar 2 (frame 96 000); the front dragged a beat on.
        let (start, offset, length) = trim_audio_start(Tick(3_840), 0, 192_000, Tick(4_800), 1, &timing);
        assert_eq!((start, offset, length), (Tick(4_800), 24_000, 168_000));
        // What was at file frame 24 000 still plays at timeline frame 120 000.
        assert_eq!(timing.frame_of(start) - offset, 96_000);

        // Pulled back before the file's own start: it stops at the file's start.
        let (start, offset, length) = trim_audio_start(Tick(4_800), 24_000, 168_000, Tick(0), 1, &timing);
        assert_eq!((start, offset, length), (Tick(3_840), 0, 192_000));
    }

    #[test]
    fn a_front_trim_never_slides_the_sound_by_a_rounding() {
        // At 44.1 kHz and 128.5 BPM a tick is not a whole number of frames,
        // so the offset has to come from the frame the new start lands on.
        let timing = Timing::constant(project_model::Tempo::from_bpm(128.5).unwrap(), 44_100);
        let start = Tick(3_840);
        let at = timing.frame_of(start);
        for pointer in [3_841u64, 3_900, 4_321, 5_000] {
            let (new, offset, length) = trim_audio_start(start, 0, 441_000, Tick(pointer), 1, &timing);
            assert_eq!(timing.frame_of(new) - offset, at, "the sound slid when trimmed to {pointer}");
            assert_eq!(timing.frame_of(new) + length, at + 441_000, "the end moved");
        }
    }

    #[test]
    fn a_point_below_the_last_row_belongs_to_the_last_row() {
        let v = View::default();
        assert_eq!(row_at(&v, 10.0, 3), Some(0));
        assert_eq!(row_at(&v, ROW_HEIGHT * 1.5, 3), Some(1));
        assert_eq!(row_at(&v, ROW_HEIGHT * 10.0, 3), Some(2));
        assert_eq!(row_at(&v, 10.0, 0), None);
    }
}
