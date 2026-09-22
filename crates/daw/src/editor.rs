//! The note editor: one MIDI clip's notes, on a grid of time and pitch.
//!
//! A view in the sense of decision D5 — it writes only the content of a clip:
//! its notes and its length. Where the clip sits, and how loud its track is,
//! belong to the timeline and the mixer. It is opened on a placement rather
//! than a clip, so it knows which window of the clip that placement plays and
//! where the playhead is inside it; what it edits is the clip, and a clip
//! placed several times changes everywhere, which the header says.
//!
//! The geometry — where a dragged note goes, how far it may stretch, where a
//! new one lands — is kept apart from the painting and tested on its own, since
//! the rule that matters most, that no edit ever leaves a note outside its
//! clip, lives there.

use eframe::egui::{self, Align2, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use project_engine::Timing;
use project_model::{ClipId, Command, Content, Meter, Note, NoteId, PPQ, Placement, PlacementId, Project, Tick};

use crate::theme::Theme;
use crate::timeline::snap;
use crate::ui::Actions;

/// Width of the keyboard down the left side.
const KEYS_WIDTH: f32 = 44.0;
/// Height of the strip along the top holding the clip's name and controls.
const HEADER_HEIGHT: f32 = 26.0;
/// How close to a note's right end the pointer must be to stretch it rather
/// than move it.
const EDGE: f32 = 6.0;
const MIN_ZOOM: f32 = 10.0;
const MAX_ZOOM: f32 = 600.0;

/// How the open clip is being looked at, and what is being done to it. Not in
/// the project.
#[derive(Debug, Clone)]
pub struct Editor {
    pub placement: PlacementId,
    pub pixels_per_beat: f32,
    /// The tick at the left edge, from the clip's start.
    pub scroll: f64,
    /// The pitch at the top edge.
    pub top: f32,
    pub row_height: f32,
    pub selected: Option<NoteId>,
    /// The length a new note is given: the last one drawn or stretched, which
    /// is usually the one wanted next.
    pub length: Tick,
    drag: Option<Drag>,
    /// Whether the view has been fitted to the clip yet. Done on the first
    /// frame, when the size of the panel is known.
    framed: bool,
}

#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Picked up this many ticks after its start.
    Move {
        note: NoteId,
        grab: f64,
    },
    Stretch {
        note: NoteId,
    },
}

impl Editor {
    pub fn new(placement: PlacementId) -> Self {
        Self {
            placement,
            pixels_per_beat: 60.0,
            scroll: 0.0,
            top: 84.0,
            row_height: 12.0,
            selected: None,
            length: Tick(PPQ),
            drag: None,
            framed: false,
        }
    }

    /// Fits the view to what this placement plays: its part of the clip across
    /// the width, and its notes' pitches centred — middle C when there are
    /// none yet.
    pub fn frame(&mut self, from: u64, span: u64, notes: impl Iterator<Item = u8>, size: Vec2) {
        let beats = (span as f64 / PPQ as f64).max(1.0);
        self.pixels_per_beat = ((f64::from(size.x) * 0.95 / beats) as f32).clamp(MIN_ZOOM, MAX_ZOOM);
        self.scroll = from as f64;
        let (low, high) = notes.fold((u8::MAX, u8::MIN), |(lo, hi), p| (lo.min(p), hi.max(p)));
        let centre = if low > high { 60.0 } else { (f32::from(low) + f32::from(high)) / 2.0 };
        let visible = size.y / self.row_height;
        self.top = (centre + visible / 2.0).clamp(visible - 1.0, 127.0);
        self.framed = true;
    }

    pub fn x_of(&self, tick: u64) -> f32 {
        ((tick as f64 - self.scroll) / PPQ as f64 * f64::from(self.pixels_per_beat)) as f32
    }

    pub fn tick_at(&self, x: f32) -> f64 {
        self.scroll + f64::from(x) / f64::from(self.pixels_per_beat) * PPQ as f64
    }

    /// Pixels from the grid's top to the top of a pitch's row.
    pub fn y_of(&self, pitch: u8) -> f32 {
        (self.top - f32::from(pitch)) * self.row_height
    }

    /// The pitch whose row a point this far below the grid's top is in.
    pub fn pitch_at(&self, y: f32) -> u8 {
        (self.top - (y / self.row_height).floor()).clamp(0.0, 127.0) as u8
    }

    /// Sixteenths once they are wide enough to aim at, beats otherwise.
    pub fn grid(&self, meter: Meter) -> u64 {
        if self.pixels_per_beat >= 48.0 { PPQ / 4 } else { meter.beat_length().0 }
    }
}

/// A note dragged so that the point it was picked up by is at `pointer`,
/// moved to `pitch` — kept whole inside a clip `length` long.
pub fn moved(note: Note, pointer: f64, grab: f64, pitch: u8, grid: Option<u64>, length: Tick) -> Note {
    let latest = length.0.saturating_sub(note.length.0);
    let start = snap(pointer - grab, grid).0.min(latest);
    Note { start: Tick(start), pitch: pitch.min(127), ..note }
}

/// A note whose end is dragged to `pointer`: at least one grid step long (one
/// tick without a grid), and never past the clip's end.
pub fn stretched(note: Note, pointer: f64, grid: Option<u64>, length: Tick) -> Note {
    let step = grid.unwrap_or(1).max(1);
    let end = snap(pointer, grid).0.clamp(note.start.0 + step, length.0.max(note.start.0 + 1));
    Note { length: Tick(end - note.start.0), ..note }
}

/// A new note at `pointer`, snapped down to the grid line it falls after, of
/// `wanted` length or less so it fits the clip. `None` if nothing fits there.
pub fn drawn(pointer: f64, pitch: u8, wanted: Tick, grid: Option<u64>, length: Tick) -> Option<Note> {
    let step = grid.unwrap_or(1).max(1);
    let start = (pointer.max(0.0) as u64 / step) * step;
    let room = length.0.checked_sub(start).filter(|room| *room > 0)?;
    Some(Note::new(pitch.min(127), 100, Tick(start), Tick(wanted.0.clamp(1, room))))
}

/// Everything the editor needs besides the project and its own state.
pub struct Context<'a> {
    pub theme: &'a Theme,
    pub timing: Timing,
    /// Where playback is, in timeline frames.
    pub playhead: u64,
    /// Whether the keyboard is the editor's: the last click was in it.
    pub keys: bool,
}

/// Draws the editor. Returns false once there is nothing left to edit — the
/// placement was removed, or the close button pressed.
pub fn show(ui: &mut Ui, project: &Project, editor: &mut Editor, cx: &Context<'_>, actions: &mut Actions) -> bool {
    let Some(placement) = project.placement(editor.placement).copied() else { return false };
    let Some(clip) = project.clip(placement.clip) else { return false };
    let Content::Midi { length, notes } = &clip.content else { return false };
    let (length, meter) = (*length, project.meter());

    let mut open = true;
    header(ui, project, &placement, clip.name.as_str(), length, cx.theme, actions, &mut open);

    let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    let keys = Rect::from_min_max(rect.left_top(), Pos2::new(rect.left() + KEYS_WIDTH, rect.bottom()));
    let grid_rect = Rect::from_min_max(Pos2::new(keys.right(), rect.top()), rect.right_bottom());
    if !editor.framed {
        let (from, span) = placement.window.map_or((0, length.0), |w| (w.offset.0, w.length.0));
        editor.frame(from, span, notes.values().map(|n| n.pitch), grid_rect.size());
    }
    scroll_and_zoom(ui, editor, grid_rect);

    let painter = ui.painter_at(rect);
    draw_grid(&painter.with_clip_rect(grid_rect), editor, grid_rect, &placement, length, meter, cx.theme);

    let background = ui.interact(grid_rect, ui.id().with("note-grid"), Sense::click());
    if background.clicked() {
        editor.selected = None;
    }
    let alt = ui.input(|i| i.modifiers.alt);
    let grid = (!alt).then(|| editor.grid(meter));
    if background.double_clicked()
        && let Some(pos) = background.interact_pointer_pos()
    {
        let (pointer, pitch) = (editor.tick_at(pos.x - grid_rect.left()), editor.pitch_at(pos.y - grid_rect.top()));
        if let Some(note) = drawn(pointer, pitch, editor.length, grid, length) {
            actions.commands.push(Command::AddNote { clip: clip.id, note });
            actions.preview = Some((note.pitch, note.velocity));
        }
    }

    let note_painter = painter.with_clip_rect(grid_rect);
    for (id, note) in notes {
        let body = note_rect(editor, grid_rect, note);
        if !body.intersects(grid_rect) {
            continue;
        }
        let selected = editor.selected == Some(*id);
        let fill = cx.theme.accent.gamma_multiply(0.35 + 0.65 * f32::from(note.velocity) / 127.0);
        note_painter.rect_filled(body, 2.0, fill);
        let outline = if selected { Stroke::new(1.5, cx.theme.text) } else { Stroke::new(1.0, cx.theme.background) };
        note_painter.rect_stroke(body, 2.0, outline, StrokeKind::Inside);

        let response = ui.interact(body.intersect(grid_rect), ui.id().with(("note", id.0)), Sense::click_and_drag());
        handle_note(ui, response, editor, clip.id, *id, *note, body, grid_rect, grid, length, actions);
    }

    if editor.selected.is_some_and(|id| !notes.contains_key(&id)) {
        editor.selected = None;
    }
    if ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(rect) {
        actions.keys_to_editor = Some(true);
    }
    if cx.keys {
        keyboard(ui, editor, clip.id, actions);
    }

    // The playhead, where it falls inside this placement's part of the clip.
    let offset = placement.window.map_or(0, |w| w.offset.0);
    let played = cx.timing.tick_of(cx.playhead).0;
    if let Some(into) = played.checked_sub(placement.start.0) {
        let x = grid_rect.left() + editor.x_of(into + offset);
        if into < placement.window.map_or(length.0, |w| w.length.0) && grid_rect.x_range().contains(x) {
            painter.line_segment(
                [Pos2::new(x, grid_rect.top()), Pos2::new(x, grid_rect.bottom())],
                Stroke::new(1.5, cx.theme.accent),
            );
        }
    }

    draw_keys(&painter.with_clip_rect(keys), editor, keys, cx.theme);
    open
}

fn note_rect(editor: &Editor, grid: Rect, note: &Note) -> Rect {
    Rect::from_min_size(
        Pos2::new(grid.left() + editor.x_of(note.start.0), grid.top() + editor.y_of(note.pitch)),
        Vec2::new(
            (note.length.0 as f64 / PPQ as f64 * f64::from(editor.pixels_per_beat)).max(3.0) as f32,
            editor.row_height,
        ),
    )
}

#[allow(clippy::too_many_arguments)]
fn header(
    ui: &mut Ui,
    project: &Project,
    placement: &Placement,
    name: &str,
    length: Tick,
    theme: &Theme,
    actions: &mut Actions,
    open: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.set_height(HEADER_HEIGHT);
        ui.label(egui::RichText::new(name).color(theme.text).strong());

        // Length in bars and beats, dragged in beats. Refused by the model if
        // it would leave a note outside, which the value springing back shows.
        let beat = project.meter().beat_length().0;
        let mut beats = length.0 / beat;
        ui.label(egui::RichText::new("length").color(theme.dim));
        let response = ui.add(egui::DragValue::new(&mut beats).range(1..=4096).suffix(" beats"));
        if response.changed() {
            actions.commands.push(Command::SetClipLength { id: placement.clip, length: Tick(beats * beat) });
        }

        let shared = project.uses_of_clip(placement.clip);
        if shared > 1 {
            ui.separator();
            ui.label(
                egui::RichText::new(format!("shared by {shared} placements: edits change all of them"))
                    .color(theme.soloed),
            );
            if ui.button("Make unique").clicked() {
                actions.commands.push(Command::MakeUnique { id: placement.id });
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close").clicked() {
                *open = false;
            }
            ui.label(
                egui::RichText::new("double-click: add · drag: move · drag the end: stretch · Alt: off the grid")
                    .color(theme.dim)
                    .small(),
            );
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_note(
    ui: &Ui,
    response: egui::Response,
    editor: &mut Editor,
    clip: ClipId,
    id: NoteId,
    note: Note,
    body: Rect,
    grid_rect: Rect,
    grid: Option<u64>,
    length: Tick,
    actions: &mut Actions,
) {
    let near_end = |pos: Pos2| pos.x >= body.right() - EDGE;
    if response.hovered()
        && let Some(pos) = response.hover_pos()
        && near_end(pos)
    {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }
    if response.clicked() || response.drag_started() {
        editor.selected = Some(id);
        actions.preview = Some((note.pitch, note.velocity));
    }
    if response.drag_started()
        && let Some(pos) = response.interact_pointer_pos()
    {
        editor.drag = Some(if near_end(pos) {
            Drag::Stretch { note: id }
        } else {
            Drag::Move { note: id, grab: editor.tick_at(pos.x - grid_rect.left()) - note.start.0 as f64 }
        });
    }
    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let pointer = editor.tick_at(pos.x - grid_rect.left());
        let changed = match editor.drag {
            Some(Drag::Move { note: n, grab }) if n == id => {
                moved(note, pointer, grab, editor.pitch_at(pos.y - grid_rect.top()), grid, length)
            }
            Some(Drag::Stretch { note: n }) if n == id => stretched(note, pointer, grid, length),
            _ => note,
        };
        if changed.pitch != note.pitch {
            // Heard at each new pitch, so a note is placed by ear.
            actions.preview = Some((changed.pitch, changed.velocity));
        }
        if changed != note {
            // Every frame the note moves; the history folds them into one
            // step per note, closed when the mouse is let go.
            actions.commands.push(Command::SetNote { clip, id, note: changed });
        }
    }
    if response.drag_stopped() {
        if matches!(editor.drag, Some(Drag::Stretch { .. })) {
            editor.length = note.length;
        }
        editor.drag = None;
        actions.seal = true;
    }
    if response.secondary_clicked() {
        actions.commands.push(Command::RemoveNote { clip, id });
    }
}

fn keyboard(ui: &Ui, editor: &mut Editor, clip: ClipId, actions: &mut Actions) {
    let Some(id) = editor.selected else { return };
    if ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    let delete = ui.input_mut(|i| {
        i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
            || i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)
    });
    if delete {
        actions.commands.push(Command::RemoveNote { clip, id });
        editor.selected = None;
    }
}

fn scroll_and_zoom(ui: &Ui, editor: &mut Editor, grid: Rect) {
    let pointer = ui.input(|i| i.pointer.hover_pos());
    if !pointer.is_some_and(|p| grid.contains(p)) {
        return;
    }
    let (delta, command) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.command));
    if command {
        if delta.y != 0.0 {
            let anchor = pointer.map_or(0.0, |p| p.x - grid.left());
            let fixed = editor.tick_at(anchor);
            editor.pixels_per_beat = (editor.pixels_per_beat * (delta.y * 0.004).exp()).clamp(MIN_ZOOM, MAX_ZOOM);
            editor.scroll = (fixed - f64::from(anchor) / f64::from(editor.pixels_per_beat) * PPQ as f64).max(0.0);
        }
        return;
    }
    editor.scroll = (editor.scroll - f64::from(delta.x) / f64::from(editor.pixels_per_beat) * PPQ as f64).max(0.0);
    // Up the wheel goes up the keyboard.
    let visible = grid.height() / editor.row_height;
    editor.top = (editor.top + delta.y / editor.row_height).clamp(visible - 1.0, 127.0);
}

fn draw_grid(
    painter: &egui::Painter,
    editor: &Editor,
    rect: Rect,
    placement: &Placement,
    length: Tick,
    meter: Meter,
    theme: &Theme,
) {
    painter.rect_filled(rect, 0.0, theme.background);
    // Rows: the black keys darker, so a pitch can be found without the keyboard.
    let lowest = (editor.top - rect.height() / editor.row_height).floor().max(0.0) as u8;
    for pitch in lowest..=(editor.top.ceil().min(127.0) as u8) {
        let top = rect.top() + editor.y_of(pitch);
        let row = Rect::from_min_size(Pos2::new(rect.left(), top), Vec2::new(rect.width(), editor.row_height));
        let fill = if is_black(pitch) { theme.background } else { theme.panel };
        painter.rect_filled(row, 0.0, fill);
        if pitch % 12 == 0 {
            painter.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0, theme.control));
        }
    }

    // Time: bars strong, beats light, finer lines once zoomed in.
    let step = editor.grid(meter);
    let (beat, bar) = (meter.beat_length().0, meter.bar_length().0);
    let mut tick = (editor.tick_at(0.0).max(0.0) as u64 / step) * step;
    while editor.x_of(tick) <= rect.width() && tick <= length.0 {
        let x = rect.left() + editor.x_of(tick);
        let colour = if tick.is_multiple_of(bar) {
            theme.dim
        } else if tick.is_multiple_of(beat) {
            theme.control
        } else {
            theme.panel
        };
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.0, colour));
        tick += step;
    }

    // Past the clip's end, and outside the part this placement plays: shaded,
    // since notes there are real but not heard from this placement.
    let shade = theme.background.gamma_multiply(0.7);
    let end = rect.left() + editor.x_of(length.0);
    if end < rect.right() {
        painter.rect_filled(Rect::from_min_max(Pos2::new(end, rect.top()), rect.right_bottom()), 0.0, shade);
    }
    if let Some(window) = placement.window {
        let from = rect.left() + editor.x_of(window.offset.0);
        let to = rect.left() + editor.x_of(window.offset.0 + window.length.0);
        painter.rect_filled(
            Rect::from_min_max(rect.left_top(), Pos2::new(from.max(rect.left()), rect.bottom())),
            0.0,
            shade,
        );
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(to.min(end), rect.top()), Pos2::new(end, rect.bottom())),
            0.0,
            shade,
        );
    }
}

fn draw_keys(painter: &egui::Painter, editor: &Editor, keys: Rect, theme: &Theme) {
    painter.rect_filled(keys, 0.0, theme.panel);
    let lowest = (editor.top - keys.height() / editor.row_height).floor().max(0.0) as u8;
    for pitch in lowest..=(editor.top.ceil().min(127.0) as u8) {
        let top = keys.top() + editor.y_of(pitch);
        let key = Rect::from_min_size(Pos2::new(keys.left(), top), Vec2::new(keys.width(), editor.row_height));
        let fill = if is_black(pitch) { theme.background } else { theme.hover };
        painter.rect_filled(key.shrink2(Vec2::new(0.0, 0.5)), 0.0, fill);
        // Middle C is C4, as most DAWs and MIDI 60 have it.
        if pitch % 12 == 0 {
            let octave = i32::from(pitch / 12) - 1;
            painter.text(
                key.right_center() - Vec2::new(3.0, 0.0),
                Align2::RIGHT_CENTER,
                format!("C{octave}"),
                FontId::proportional(9.0),
                theme.text,
            );
        }
    }
}

fn is_black(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: Tick = Tick(3_840);

    fn note(start: u64, length: u64) -> Note {
        Note::new(60, 100, Tick(start), Tick(length))
    }

    #[test]
    fn a_pitch_and_its_row_agree_both_ways() {
        let editor = Editor::new(PlacementId(0));
        for pitch in [0u8, 60, 84, 127] {
            let y = editor.y_of(pitch) + editor.row_height / 2.0;
            assert_eq!(editor.pitch_at(y), pitch, "pitch {pitch}");
        }
        assert_eq!(editor.pitch_at(-100_000.0), 127, "far above the top is clamped, not wrapped");
        assert_eq!(editor.pitch_at(100_000.0), 0);
    }

    #[test]
    fn a_moved_note_keeps_its_grab_and_its_length_and_stays_inside() {
        // Picked up 100 ticks in, dropped with the pointer at 2 000.
        let moved_to = moved(note(0, 960), 2_000.0, 100.0, 64, Some(240), BAR);
        assert_eq!((moved_to.start, moved_to.length, moved_to.pitch), (Tick(1_920), Tick(960), 64));
        // Dragged past the end: it stops where it still fits.
        let too_far = moved(note(0, 960), 9_000.0, 0.0, 60, Some(240), BAR);
        assert_eq!(too_far.end(), BAR);
        // And never before the start.
        assert_eq!(moved(note(960, 960), 0.0, 900.0, 60, Some(240), BAR).start, Tick(0));
    }

    #[test]
    fn a_stretched_note_is_never_shorter_than_a_step_nor_past_the_end() {
        assert_eq!(stretched(note(960, 240), 2_000.0, Some(240), BAR).end(), Tick(1_920));
        assert_eq!(stretched(note(960, 240), 0.0, Some(240), BAR).length, Tick(240), "stretched to nothing");
        assert_eq!(stretched(note(960, 240), 99_999.0, Some(240), BAR).end(), BAR);
        assert_eq!(stretched(note(960, 240), 1_000.0, None, BAR).length, Tick(40), "off the grid, to the tick");
    }

    #[test]
    fn a_drawn_note_starts_on_the_line_before_the_pointer_and_fits_the_clip() {
        let n = drawn(1_000.0, 62, Tick(960), Some(240), BAR).unwrap();
        assert_eq!((n.start, n.length, n.pitch), (Tick(960), Tick(960), 62));
        // Near the end, shortened to fit rather than refused.
        let last = drawn(3_700.0, 60, Tick(960), Some(240), BAR).unwrap();
        assert_eq!(last.end(), BAR);
        // Past the end, nothing.
        assert!(drawn(5_000.0, 60, Tick(960), Some(240), BAR).is_none());
    }

    #[test]
    fn opening_fits_the_played_part_and_centres_the_notes() {
        let mut editor = Editor::new(PlacementId(0));
        editor.frame(960, 3_840, [60u8, 72].into_iter(), Vec2::new(800.0, 240.0));
        // Four beats across 95 % of 800 pixels, starting where the window does.
        assert!((editor.pixels_per_beat - 190.0).abs() < 1e-3);
        assert_eq!(editor.scroll, 960.0);
        // Twenty rows of 12 pixels, centred on 66.
        assert_eq!(editor.top, 76.0);
        assert_eq!(editor.pitch_at(120.0), 66, "the middle row is not the middle of the notes");

        editor.frame(0, 3_840, std::iter::empty(), Vec2::new(800.0, 240.0));
        assert_eq!(editor.top, 70.0, "an empty clip opens on middle C");
    }

    #[test]
    fn middle_c_is_white_and_c_sharp_is_black() {
        assert!(!is_black(60));
        assert!(is_black(61));
        assert!(is_black(70));
        assert!(!is_black(71));
    }
}
