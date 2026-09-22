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
use project_model::{AssetId, Command, Content, Meter, PPQ, Placement, PlacementId, Project, Tick, TrackId};

use crate::peaks::Peaks;
use crate::theme::Theme;
use crate::ui::Actions;

/// Width of the column of track names on the left.
const HEADER_WIDTH: f32 = 140.0;
/// Height of the bar ruler along the top.
const RULER_HEIGHT: f32 = 22.0;
const ROW_HEIGHT: f32 = 64.0;
/// The zoom range, in pixels per beat.
const MIN_ZOOM: f32 = 4.0;
const MAX_ZOOM: f32 = 400.0;
/// Above this zoom the grid is sixteenths rather than beats.
const FINE_GRID_ZOOM: f32 = 80.0;

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
    /// A clip being dragged, and how far into it it was picked up, in ticks,
    /// so it moves with the pointer instead of jumping its start to it.
    drag: Option<(PlacementId, f64)>,
}

impl Default for View {
    fn default() -> Self {
        Self { pixels_per_beat: 40.0, scroll: 0.0, scroll_y: 0.0, selected: None, open: None, drag: None }
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

/// Which row a point this far below the top of the lanes falls in, clamped to
/// the rows there are.
pub fn row_at(view: &View, y: f32, rows: usize) -> Option<usize> {
    let row = ((y + view.scroll_y) / ROW_HEIGHT).floor();
    (rows > 0).then(|| (row.max(0.0) as usize).min(rows - 1))
}

/// How long a placement is on the timeline, in ticks.
///
/// A MIDI clip knows its length; an audio clip lasts as long as its file,
/// which is frames, turned into ticks at the current tempo. An audio file not
/// read yet is drawn a bar long, so it can still be seen and grabbed.
fn length_of(
    project: &Project,
    placement: &Placement,
    timing: Timing,
    peaks: &dyn Fn(AssetId) -> Option<Arc<Peaks>>,
) -> u64 {
    let Some(clip) = project.clip(placement.clip) else { return 0 };
    match &clip.content {
        Content::Midi { length, .. } => placement.window.map_or(length.0, |w| w.length.0),
        Content::Audio { asset } => {
            peaks(*asset).map_or(project.meter().bar_length().0, |p| timing.tick_of(p.frames()).0.max(1))
        }
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
    let lanes = Rect::from_min_max(Pos2::new(ruler.left(), ruler.bottom()), rect.right_bottom());
    let headers = Rect::from_min_max(Pos2::new(rect.left(), ruler.bottom()), Pos2::new(ruler.left(), rect.bottom()));
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
        let length = length_of(project, placement, cx.timing, cx.peaks);
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
        handle_clip(ui, response, project, placement, length, view, lanes, &tracks, actions);
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

    draw_ruler(ui, &painter, view, ruler, meter, cx, actions);
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
            Some(peaks) => draw_waveform(painter, &peaks, inner, body, cx),
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

fn draw_waveform(painter: &egui::Painter, peaks: &Peaks, inner: Rect, body: Rect, cx: &Context<'_>) {
    let visible = body.intersect(painter.clip_rect());
    let centre = inner.center().y;
    let half = inner.height() / 2.0;
    let frames_per_pixel = peaks.frames() as f64 / f64::from(body.width().max(1.0));
    let colour = cx.theme.accent.gamma_multiply(0.8);
    // One line per visible pixel, from the lowest sample under it to the highest.
    let mut x = visible.left().floor();
    while x < visible.right() {
        let from = ((f64::from(x - body.left())) * frames_per_pixel).max(0.0) as u64;
        let to = ((f64::from(x + 1.0 - body.left())) * frames_per_pixel).ceil() as u64;
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
    actions: &mut Actions,
) {
    let meter = project.meter();
    if response.clicked() || response.drag_started() {
        view.selected = Some(placement.id);
    }
    if response.double_clicked() && project.clip(placement.clip).is_some_and(|c| c.length().is_some()) {
        view.open = Some(placement.id);
    }
    if response.drag_started()
        && let Some(pos) = response.interact_pointer_pos()
    {
        view.drag = Some((placement.id, view.tick_at(pos.x - lanes.left()) - placement.start.0 as f64));
    }
    if response.dragged()
        && let Some((id, grab)) = view.drag
        && id == placement.id
        && let Some(pos) = response.interact_pointer_pos()
    {
        // Alt lets go of the grid, for placing by ear.
        let grid = (!ui.input(|i| i.modifiers.alt)).then(|| view.grid(meter));
        let start = drop_at(view, pos.x - lanes.left(), grab, grid);
        let track = row_at(view, pos.y - lanes.top(), tracks.len()).map_or(placement.track, |row| tracks[row]);
        if start != placement.start || track != placement.track {
            // Sent on every frame the clip moves; the history folds them into
            // one step, closed when the mouse is let go.
            actions.commands.push(Command::MovePlacement { id: placement.id, track, start });
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
        let start = Tick(placement.start.0 + length_of(project, placement, cx.timing, cx.peaks));
        actions.commands.push(Command::DuplicatePlacement { id, track: placement.track, start, linked: false });
    }
}

fn draw_ruler(
    ui: &Ui,
    painter: &egui::Painter,
    view: &View,
    ruler: Rect,
    meter: Meter,
    cx: &Context<'_>,
    actions: &mut Actions,
) {
    let theme = cx.theme;
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

    // Clicking or dragging along the ruler moves playback there.
    let response = ui.interact(ruler, ui.id().with("ruler"), Sense::click_and_drag());
    if (response.clicked() || response.dragged())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let tick = snap(view.tick_at(pos.x - ruler.left()), None);
        actions.transport = Some(TransportCommand::Seek(cx.timing.frame_of(tick)));
    }
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
    fn a_point_below_the_last_row_belongs_to_the_last_row() {
        let v = View::default();
        assert_eq!(row_at(&v, 10.0, 3), Some(0));
        assert_eq!(row_at(&v, ROW_HEIGHT * 1.5, 3), Some(1));
        assert_eq!(row_at(&v, ROW_HEIGHT * 10.0, 3), Some(2));
        assert_eq!(row_at(&v, 10.0, 0), None);
    }
}
