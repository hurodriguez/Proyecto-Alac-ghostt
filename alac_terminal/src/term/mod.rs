//! Exports the `Term` type which is a high-level API for the Grid.

use std::ops::Range;
use std::{cmp, mem, slice};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use bitflags::bitflags;
use log::debug;
use unicode_width::UnicodeWidthChar;

use crate::event::{Event, EventListener};
use crate::grid::{Dimensions, Grid, GridIterator, Scroll};
use crate::index::{Boundary, Column, Direction, Line, Point, Side};
use crate::selection::{Selection, SelectionRange, SelectionType};
use crate::term::cell::{Cell, Flags, LineLength};
use crate::term::color::Colors;
use crate::term::engine::{DefaultColors, Engine, Pending};
use crate::vi_mode::{ViModeCursor, ViMotion};
use crate::vte::ansi::{CursorShape, CursorStyle};

pub mod cell;
pub mod color;
pub mod engine;
pub mod search;

/// Minimum number of columns.
///
/// A minimum of 2 is necessary to hold fullwidth unicode characters.
pub const MIN_COLUMNS: usize = 2;

/// Minimum number of visible lines.
pub const MIN_SCREEN_LINES: usize = 1;

/// Default semantic escape characters.
pub const SEMANTIC_ESCAPE_CHARS: &str = ",│`|:\"' ()[]{}<>\t";

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct TermMode: u32 {
        const NONE                    = 0;
        const SHOW_CURSOR             = 1;
        const APP_CURSOR              = 1 << 1;
        const APP_KEYPAD              = 1 << 2;
        const MOUSE_REPORT_CLICK      = 1 << 3;
        const BRACKETED_PASTE         = 1 << 4;
        const SGR_MOUSE               = 1 << 5;
        const MOUSE_MOTION            = 1 << 6;
        const LINE_WRAP               = 1 << 7;
        const LINE_FEED_NEW_LINE      = 1 << 8;
        const ORIGIN                  = 1 << 9;
        const INSERT                  = 1 << 10;
        const FOCUS_IN_OUT            = 1 << 11;
        const ALT_SCREEN              = 1 << 12;
        const MOUSE_DRAG              = 1 << 13;
        const UTF8_MOUSE              = 1 << 14;
        const ALTERNATE_SCROLL        = 1 << 15;
        const VI                      = 1 << 16;
        const URGENCY_HINTS           = 1 << 17;
        const DISAMBIGUATE_ESC_CODES  = 1 << 18;
        const REPORT_EVENT_TYPES      = 1 << 19;
        const REPORT_ALTERNATE_KEYS   = 1 << 20;
        const REPORT_ALL_KEYS_AS_ESC  = 1 << 21;
        const REPORT_ASSOCIATED_TEXT  = 1 << 22;
        const MOUSE_MODE              = Self::MOUSE_REPORT_CLICK.bits() | Self::MOUSE_MOTION.bits() | Self::MOUSE_DRAG.bits();
        const KITTY_KEYBOARD_PROTOCOL = Self::DISAMBIGUATE_ESC_CODES.bits()
                                      | Self::REPORT_EVENT_TYPES.bits()
                                      | Self::REPORT_ALTERNATE_KEYS.bits()
                                      | Self::REPORT_ALL_KEYS_AS_ESC.bits()
                                      | Self::REPORT_ASSOCIATED_TEXT.bits();
         const ANY                    = u32::MAX;
    }
}

impl Default for TermMode {
    fn default() -> TermMode {
        TermMode::SHOW_CURSOR
            | TermMode::LINE_WRAP
            | TermMode::ALTERNATE_SCROLL
            | TermMode::URGENCY_HINTS
    }
}

/// Convert a terminal point to a viewport relative point.
#[inline]
pub fn point_to_viewport(display_offset: usize, point: Point) -> Option<Point<usize>> {
    let viewport_line = point.line.0 + display_offset as i32;
    usize::try_from(viewport_line).ok().map(|line| Point::new(line, point.column))
}

/// Convert a viewport relative point to a terminal point.
#[inline]
pub fn viewport_to_point(display_offset: usize, point: Point<usize>) -> Point {
    let line = Line(point.line as i32) - display_offset;
    Point::new(line, point.column)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineDamageBounds {
    /// Damaged line number.
    pub line: usize,

    /// Leftmost damaged column.
    pub left: usize,

    /// Rightmost damaged column.
    pub right: usize,
}

impl LineDamageBounds {
    #[inline]
    pub fn new(line: usize, left: usize, right: usize) -> Self {
        Self { line, left, right }
    }

    #[inline]
    pub fn undamaged(line: usize, num_cols: usize) -> Self {
        Self { line, left: num_cols, right: 0 }
    }

    #[inline]
    pub fn reset(&mut self, num_cols: usize) {
        *self = Self::undamaged(self.line, num_cols);
    }

    #[inline]
    pub fn expand(&mut self, left: usize, right: usize) {
        self.left = cmp::min(self.left, left);
        self.right = cmp::max(self.right, right);
    }

    #[inline]
    pub fn is_damaged(&self) -> bool {
        self.left <= self.right
    }
}

/// Terminal damage information collected since the last [`Term::reset_damage`] call.
#[derive(Debug)]
pub enum TermDamage<'a> {
    /// The entire terminal is damaged.
    Full,

    /// Iterator over damaged lines in the terminal.
    Partial(TermDamageIterator<'a>),
}

/// Iterator over the terminal's viewport damaged lines.
#[derive(Clone, Debug)]
pub struct TermDamageIterator<'a> {
    line_damage: slice::Iter<'a, LineDamageBounds>,
    display_offset: usize,
}

impl<'a> TermDamageIterator<'a> {
    pub fn new(line_damage: &'a [LineDamageBounds], display_offset: usize) -> Self {
        let num_lines = line_damage.len();
        // Filter out invisible damage.
        let line_damage = &line_damage[..num_lines.saturating_sub(display_offset)];
        Self { display_offset, line_damage: line_damage.iter() }
    }
}

impl Iterator for TermDamageIterator<'_> {
    type Item = LineDamageBounds;

    fn next(&mut self) -> Option<Self::Item> {
        self.line_damage.find_map(|line| {
            line.is_damaged().then_some(LineDamageBounds::new(
                line.line + self.display_offset,
                line.left,
                line.right,
            ))
        })
    }
}

/// State of the terminal damage.
struct TermDamageState {
    /// Hint whether terminal should be damaged entirely regardless of the actual damage changes.
    full: bool,

    /// Information about damage on terminal lines.
    lines: Vec<LineDamageBounds>,

    /// Old terminal cursor point.
    last_cursor: Point,
}

impl TermDamageState {
    fn new(num_cols: usize, num_lines: usize) -> Self {
        let lines =
            (0..num_lines).map(|line| LineDamageBounds::undamaged(line, num_cols)).collect();

        Self { full: true, lines, last_cursor: Default::default() }
    }

    #[inline]
    fn resize(&mut self, num_cols: usize, num_lines: usize) {
        // Reset point, so old cursor won't end up outside of the viewport.
        self.last_cursor = Default::default();
        self.full = true;

        self.lines.clear();
        self.lines.reserve(num_lines);
        for line in 0..num_lines {
            self.lines.push(LineDamageBounds::undamaged(line, num_cols));
        }
    }

    /// Damage point inside of the viewport.
    #[inline]
    fn damage_point(&mut self, point: Point<usize>) {
        self.damage_line(point.line, point.column.0, point.column.0);
    }

    /// Expand `line`'s damage to span at least `left` to `right` column.
    #[inline]
    fn damage_line(&mut self, line: usize, left: usize, right: usize) {
        self.lines[line].expand(left, right);
    }

    /// Reset information about terminal damage.
    fn reset(&mut self, num_cols: usize) {
        self.full = false;
        self.lines.iter_mut().for_each(|line| line.reset(num_cols));
    }
}

pub struct Term<T> {
    /// Terminal focus controlling the cursor shape.
    pub is_focused: bool,

    /// Cursor for keyboard selection.
    pub vi_mode_cursor: ViModeCursor,

    pub selection: Option<Selection>,

    /// Mirror of the active screen (and its scrollback) used for rendering, selection, search
    /// and vi mode. The engine owns the real terminal state.
    grid: Grid<Cell>,

    /// Mirror of the inactive screen.
    inactive_grid: Grid<Cell>,

    /// Mode flags.
    mode: TermMode,

    /// Modified terminal colors.
    colors: Colors,

    /// Current style of the cursor.
    cursor_style: CursorStyle,

    /// Proxy for sending events to the event loop.
    event_proxy: T,

    /// Current title of the window.
    title: Option<String>,

    /// libghostty-vt terminal engine.
    engine: Box<Engine>,

    /// Identity of the viewport rows at the previous synchronization.
    row_ids: Vec<[u64; 2]>,

    /// Information about damaged cells.
    damage: TermDamageState,

    /// Config directly for the terminal.
    config: Config,
}

/// Configuration options for the [`Term`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The maximum amount of scrolling history.
    pub scrolling_history: usize,

    /// Default cursor style to reset the cursor to.
    pub default_cursor_style: CursorStyle,

    /// Cursor style for Vi mode.
    pub vi_mode_cursor_style: Option<CursorStyle>,

    /// The characters which terminate semantic selection.
    ///
    /// The default value is [`SEMANTIC_ESCAPE_CHARS`].
    pub semantic_escape_chars: String,

    /// Whether to enable kitty keyboard protocol.
    pub kitty_keyboard: bool,

    /// OSC52 support mode.
    pub osc52: Osc52,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scrolling_history: 10000,
            semantic_escape_chars: SEMANTIC_ESCAPE_CHARS.to_owned(),
            default_cursor_style: Default::default(),
            vi_mode_cursor_style: Default::default(),
            kitty_keyboard: Default::default(),
            osc52: Default::default(),
        }
    }
}

/// OSC 52 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(rename_all = "lowercase"))]
pub enum Osc52 {
    /// The handling of the escape sequence is disabled.
    Disabled,
    /// Only copy sequence is accepted.
    ///
    /// This option is the default as a compromise between entirely
    /// disabling it (the most secure) and allowing `paste` (the less secure).
    #[default]
    OnlyCopy,
    /// Only paste sequence is accepted.
    OnlyPaste,
    /// Both are accepted.
    CopyPaste,
}

impl<T> Term<T> {
    #[inline]
    pub fn scroll_display(&mut self, scroll: Scroll)
    where
        T: EventListener,
    {
        let old_display_offset = self.grid.display_offset();
        self.grid.scroll_display(scroll);
        self.event_proxy.send_event(Event::MouseCursorDirty);

        // Clamp vi mode cursor to the viewport.
        let viewport_start = -(self.grid.display_offset() as i32);
        let viewport_end = viewport_start + self.bottommost_line().0;
        let vi_cursor_line = &mut self.vi_mode_cursor.point.line.0;
        *vi_cursor_line = cmp::min(viewport_end, cmp::max(viewport_start, *vi_cursor_line));
        self.vi_mode_recompute_selection();

        // Damage everything if display offset changed.
        if old_display_offset != self.grid().display_offset() {
            self.mark_fully_damaged();
        }
    }

    pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T> {
        let num_cols = dimensions.columns();
        let num_lines = dimensions.screen_lines();

        let grid = Grid::new(num_lines, num_cols, config.scrolling_history);
        let inactive_grid = Grid::new(num_lines, num_cols, 0);

        let mut engine = Engine::new(num_cols, num_lines, config.scrolling_history);
        engine.set_default_cursor(config.default_cursor_style);

        // Initialize terminal damage, covering the entire terminal upon launch.
        let damage = TermDamageState::new(num_cols, num_lines);

        Term {
            inactive_grid,
            event_proxy,
            damage,
            config,
            grid,
            engine,
            row_ids: Vec::new(),
            vi_mode_cursor: Default::default(),
            cursor_style: Default::default(),
            colors: color::Colors::default(),
            is_focused: Default::default(),
            selection: Default::default(),
            title: Default::default(),
            mode: Default::default(),
        }
    }

    /// Process output of the child program.
    pub fn advance(&mut self, bytes: &[u8])
    where
        T: EventListener,
    {
        self.engine.write(bytes);
        self.sync();
    }

    /// Configure the colors reported to programs which query them (OSC 4/10/11/12).
    pub fn set_default_colors(&mut self, colors: DefaultColors) {
        self.engine.set_default_colors(colors);
    }

    /// Update the size of a cell in pixels, reported in size queries.
    pub fn set_cell_size(&mut self, width: u32, height: u32) {
        self.engine.set_cell_size(width, height);
    }

    /// Remove the scrollback history.
    pub fn clear_history(&mut self)
    where
        T: EventListener,
    {
        self.advance(b"\x1b[3J");
    }

    /// Reset the terminal to its initial state.
    pub fn reset(&mut self)
    where
        T: EventListener,
    {
        self.engine.reset();
        self.sync();
    }

    /// Copy the changes made by the engine into the grid mirror and notify the listener.
    fn sync(&mut self)
    where
        T: EventListener,
    {
        let engine_mode = self.engine.term_mode();
        let alt = engine_mode.contains(TermMode::ALT_SCREEN);
        let mut all = false;
        if alt != self.mode.contains(TermMode::ALT_SCREEN) {
            self.swap_screens(alt);
            all = true;
        }

        let lines = self.screen_lines();
        let columns = self.columns();

        // Mirror the scrollback state of the primary screen.
        if !alt && !all {
            let history = self.engine.total_rows().saturating_sub(lines);
            if history < self.grid.history_size() {
                self.grid.clear_history();
                self.selection = None;
                self.vi_mode_cursor.point.line = cmp::max(
                    cmp::min(self.vi_mode_cursor.point.line, self.bottommost_line()),
                    Line(0),
                );
                all = true;
            }
        }

        let mut rows = self.engine.read_viewport(all);
        if rows.len() != lines {
            return;
        }

        if !alt && !all {
            // Detect how far the content moved by looking for the first row's old position.
            let shift = self.row_ids.iter().position(|id| *id == rows[0].id).unwrap_or(0);
            let history = self.engine.total_rows().saturating_sub(lines);
            let grown = history.saturating_sub(self.grid.history_size());
            let shift = if shift > 0 { shift } else { grown };
            if shift > 0 && shift < lines + 1 {
                self.scroll_mirror(shift.min(lines));
            } else if shift > lines {
                rows = self.engine.read_viewport(true);
                self.mark_fully_damaged();
            }
        }

        for (y, row) in rows.iter_mut().enumerate() {
            if !row.dirty || row.cells.len() != columns {
                continue;
            }
            let line = Line(y as i32);
            for (x, cell) in row.cells.drain(..).enumerate() {
                self.grid[line][Column(x)] = cell;
            }
            if row.wrap {
                self.grid[line][Column(columns - 1)].flags.insert(Flags::WRAPLINE);
            }
            self.damage.damage_line(y, 0, columns - 1);
        }
        self.row_ids = rows.iter().map(|row| row.id).collect();

        // Cursor.
        let cursor = self.engine.cursor();
        self.grid.cursor.point = Point::new(
            Line(cursor.line.min(lines - 1) as i32),
            Column(cursor.column.min(columns - 1)),
        );
        self.grid.cursor.input_needs_wrap = cursor.pending_wrap;
        if cursor.style != self.cursor_style {
            self.cursor_style = cursor.style;
            self.event_proxy.send_event(Event::CursorBlinkingChange);
        }

        // Modes, keeping the ones owned by the application.
        let owned = TermMode::VI | TermMode::URGENCY_HINTS;
        let old_mode = self.mode;
        self.mode = engine_mode | (self.mode & owned);
        if (old_mode ^ self.mode).intersects(TermMode::MOUSE_MODE | TermMode::SGR_MOUSE) {
            self.event_proxy.send_event(Event::MouseCursorDirty);
        }

        // Colors modified by the running program.
        self.colors = Colors::default();
        for (index, color) in self.engine.color_overrides() {
            self.colors[index] = Some(color);
        }

        for event in self.engine.take_events() {
            match event {
                Pending::PtyWrite(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    self.event_proxy.send_event(Event::PtyWrite(text));
                },
                Pending::Bell => self.event_proxy.send_event(Event::Bell),
                Pending::TitleChanged => {
                    self.title = self.engine.title();
                    let event = match &self.title {
                        Some(title) => Event::Title(title.clone()),
                        None => Event::ResetTitle,
                    };
                    self.event_proxy.send_event(event);
                },
                Pending::ClipboardStore(ty, text) => {
                    if matches!(self.config.osc52, Osc52::OnlyCopy | Osc52::CopyPaste) {
                        self.event_proxy.send_event(Event::ClipboardStore(ty, text));
                    }
                },
            }
        }
    }

    /// Scroll the mirror up after the engine moved content into the scrollback.
    fn scroll_mirror(&mut self, lines: usize) {
        let region = Line(0)..Line(self.screen_lines() as i32);
        self.selection = self.selection.take().and_then(|s| s.rotate(self, &region, lines as i32));
        self.grid.scroll_up(&region, lines);

        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let line = &mut self.vi_mode_cursor.point.line;
        *line = cmp::max(*line - lines, viewport_top);
        self.mark_fully_damaged();
    }

    /// Switch the mirrored screen, after the engine changed between primary and alternate.
    fn swap_screens(&mut self, alt: bool) {
        let lines = self.screen_lines();
        let columns = self.columns();
        mem::swap(&mut self.grid, &mut self.inactive_grid);
        self.mode.set(TermMode::ALT_SCREEN, alt);
        self.selection = None;
        self.row_ids.clear();
        self.mark_fully_damaged();

        if alt {
            self.grid.reset_region(..);
        } else if self.grid.columns() != columns || self.grid.screen_lines() != lines {
            self.rebuild(columns, lines);
        }
    }

    /// Rebuild the mirror of the active screen from the engine, including scrollback.
    fn rebuild(&mut self, columns: usize, lines: usize) {
        let history = if self.mode.contains(TermMode::ALT_SCREEN) {
            0
        } else {
            self.engine.total_rows().saturating_sub(lines)
        };
        let max_history = if self.mode.contains(TermMode::ALT_SCREEN) {
            0
        } else {
            self.config.scrolling_history
        };

        let mut grid = Grid::new(lines, columns, max_history);
        let region = Line(0)..Line(lines as i32);
        for y in 0..history + lines {
            let Some((cells, wrap)) = self.engine.read_screen_row(y, columns) else { continue };

            // History rows are pushed through the top of the screen.
            let line = if y < history { Line(0) } else { Line((y - history) as i32) };
            for (x, cell) in cells.into_iter().enumerate() {
                grid[line][Column(x)] = cell;
            }
            if wrap {
                grid[line][Column(columns - 1)].flags.insert(Flags::WRAPLINE);
            }
            if y < history {
                grid.scroll_up(&region, 1);
            }
        }

        let display_offset = cmp::min(self.grid.display_offset(), grid.history_size());
        grid.scroll_display(Scroll::Delta(display_offset as i32));
        self.grid = grid;
        self.row_ids.clear();
        self.mark_fully_damaged();
    }

    #[inline]
    fn damage_cursor(&mut self) {
        // The normal cursor coordinates are always in viewport.
        let point =
            Point::new(self.grid.cursor.point.line.0 as usize, self.grid.cursor.point.column);
        self.damage.damage_point(point);
    }

    /// Collect the information about the changes in the lines, which
    /// could be used to minimize the amount of drawing operations.
    ///
    /// The user controlled elements, like `Vi` mode cursor and `Selection` are **not** part of the
    /// collected damage state. Those could easily be tracked by comparing their old and new
    /// value between adjacent frames.
    ///
    /// After reading damage [`reset_damage`] should be called.
    ///
    /// [`reset_damage`]: Self::reset_damage
    #[must_use]
    pub fn damage(&mut self) -> TermDamage<'_> {
        // Ensure the entire terminal is damaged after entering insert mode.
        // Leaving is handled in the ansi handler.
        if self.mode.contains(TermMode::INSERT) {
            self.mark_fully_damaged();
        }

        let previous_cursor = mem::replace(&mut self.damage.last_cursor, self.grid.cursor.point);

        if self.damage.full {
            return TermDamage::Full;
        }

        // Add information about old cursor position and new one if they are not the same, so we
        // cover everything that was produced by `Term::input`.
        if self.damage.last_cursor != previous_cursor {
            // Cursor coordinates are always inside viewport even if you have `display_offset`.
            let point = Point::new(previous_cursor.line.0 as usize, previous_cursor.column);
            self.damage.damage_point(point);
        }

        // Always damage current cursor.
        self.damage_cursor();

        // NOTE: damage which changes all the content when the display offset is non-zero (e.g.
        // scrolling) is handled via full damage.
        let display_offset = self.grid().display_offset();
        TermDamage::Partial(TermDamageIterator::new(&self.damage.lines, display_offset))
    }

    /// Resets the terminal damage information.
    pub fn reset_damage(&mut self) {
        self.damage.reset(self.columns());
    }

    #[inline]
    fn mark_fully_damaged(&mut self) {
        self.damage.full = true;
    }

    /// Set new options for the [`Term`].
    pub fn set_options(&mut self, options: Config)
    where
        T: EventListener,
    {
        let old_config = mem::replace(&mut self.config, options);

        let title_event = match &self.title {
            Some(title) => Event::Title(title.clone()),
            None => Event::ResetTitle,
        };

        self.event_proxy.send_event(title_event);

        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid.update_history(self.config.scrolling_history);
        } else {
            self.grid.update_history(self.config.scrolling_history);
        }

        if self.config.scrolling_history != old_config.scrolling_history {
            self.engine.set_scrollback(self.config.scrolling_history);
        }
        if self.config.default_cursor_style != old_config.default_cursor_style {
            self.engine.set_default_cursor(self.config.default_cursor_style);
        }

        // Damage everything on config updates.
        self.mark_fully_damaged();
    }

    /// Convert the active selection to a String.
    pub fn selection_to_string(&self) -> Option<String> {
        let selection_range = self.selection.as_ref().and_then(|s| s.to_range(self))?;
        let SelectionRange { start, end, .. } = selection_range;

        let mut res = String::new();

        match self.selection.as_ref() {
            Some(Selection { ty: SelectionType::Block, .. }) => {
                for line in (start.line.0..end.line.0).map(Line::from) {
                    res += self
                        .line_to_string(line, start.column..end.column, start.column.0 != 0)
                        .trim_end();
                    res += "\n";
                }

                res += self.line_to_string(end.line, start.column..end.column, true).trim_end();
            },
            Some(Selection { ty: SelectionType::Lines, .. }) => {
                res = self.bounds_to_string(start, end) + "\n";
            },
            _ => {
                res = self.bounds_to_string(start, end);
            },
        }

        Some(res)
    }

    /// Convert range between two points to a String.
    pub fn bounds_to_string(&self, start: Point, end: Point) -> String {
        let mut res = String::new();

        for line in (start.line.0..=end.line.0).map(Line::from) {
            let start_col = if line == start.line { start.column } else { Column(0) };
            let end_col = if line == end.line { end.column } else { self.last_column() };

            res += &self.line_to_string(line, start_col..end_col, line == end.line);
        }

        res.strip_suffix('\n').map(str::to_owned).unwrap_or(res)
    }

    /// Convert a single line in the grid to a String.
    fn line_to_string(
        &self,
        line: Line,
        mut cols: Range<Column>,
        include_wrapped_wide: bool,
    ) -> String {
        let mut text = String::new();

        let grid_line = &self.grid[line];
        let line_length = cmp::min(grid_line.line_length(), cols.end + 1);

        // Include wide char when trailing spacer is selected.
        if grid_line[cols.start].flags.contains(Flags::WIDE_CHAR_SPACER) {
            cols.start -= 1;
        }

        for column in (cols.start.0..line_length.0).map(Column::from) {
            let cell = &grid_line[column];

            if !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                // Push cells primary character.
                text.push(cell.c);

                // Push zero-width characters.
                for c in cell.zerowidth().into_iter().flatten() {
                    text.push(*c);
                }
            }
        }

        if cols.end >= self.columns() - 1
            && (line_length.0 == 0
                || !self.grid[line][line_length - 1].flags.contains(Flags::WRAPLINE))
        {
            text.push('\n');
        }

        // If wide char is not part of the selection, but leading spacer is, include it.
        if line_length == self.columns()
            && line_length.0 >= 2
            && grid_line[line_length - 1].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            && include_wrapped_wide
        {
            text.push(self.grid[line - 1i32][Column(0)].c);
        }

        text
    }

    /// Terminal content required for rendering.
    #[inline]
    pub fn renderable_content(&self) -> RenderableContent<'_>
    where
        T: EventListener,
    {
        RenderableContent::new(self)
    }

    /// Access to the raw grid data structure.
    pub fn grid(&self) -> &Grid<Cell> {
        &self.grid
    }

    /// Mutable access to the raw grid data structure.
    pub fn grid_mut(&mut self) -> &mut Grid<Cell> {
        &mut self.grid
    }

    /// Resize terminal to new dimensions.
    pub fn resize<S: Dimensions>(&mut self, size: S) {
        let old_cols = self.columns();
        let old_lines = self.screen_lines();

        let num_cols = size.columns();
        let num_lines = size.screen_lines();

        if old_cols == num_cols && old_lines == num_lines {
            debug!("Term::resize dimensions unchanged");
            return;
        }

        debug!("New num_cols is {num_cols} and num_lines is {num_lines}");

        // The engine reflows its own screens.
        self.engine.resize(num_cols, num_lines);
        self.selection = None;

        // Move vi mode cursor with the content.
        let delta = num_lines as i32 - old_lines as i32;
        self.vi_mode_cursor.point.line += cmp::max(delta, 0);

        self.rebuild(num_cols, num_lines);
        self.inactive_grid = Grid::new(num_lines, num_cols, 0);

        // Clamp vi cursor to viewport.
        let vi_point = self.vi_mode_cursor.point;
        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let viewport_bottom = viewport_top + self.bottommost_line();
        self.vi_mode_cursor.point.line =
            cmp::max(cmp::min(vi_point.line, viewport_bottom), viewport_top);
        self.vi_mode_cursor.point.column = cmp::min(vi_point.column, self.last_column());

        self.damage.resize(num_cols, num_lines);
        self.row_ids.clear();
        self.sync_after_resize();
    }

    fn sync_after_resize(&mut self) {
        let cursor = self.engine.cursor();
        let lines = self.screen_lines();
        let columns = self.columns();
        self.grid.cursor.point = Point::new(
            Line(cursor.line.min(lines - 1) as i32),
            Column(cursor.column.min(columns - 1)),
        );
        self.mode =
            self.engine.term_mode() | (self.mode & (TermMode::VI | TermMode::URGENCY_HINTS));
    }

    /// Active terminal modes.
    #[inline]
    pub fn mode(&self) -> &TermMode {
        &self.mode
    }

    #[inline]
    pub fn exit(&mut self)
    where
        T: EventListener,
    {
        self.event_proxy.send_event(Event::Exit);
    }

    /// Toggle the vi mode.
    #[inline]
    pub fn toggle_vi_mode(&mut self)
    where
        T: EventListener,
    {
        self.mode ^= TermMode::VI;

        if self.mode.contains(TermMode::VI) {
            let display_offset = self.grid.display_offset() as i32;
            if self.grid.cursor.point.line > self.bottommost_line() - display_offset {
                // Move cursor to top-left if terminal cursor is not visible.
                let point = Point::new(Line(-display_offset), Column(0));
                self.vi_mode_cursor = ViModeCursor::new(point);
            } else {
                // Reset vi mode cursor position to match primary cursor.
                self.vi_mode_cursor = ViModeCursor::new(self.grid.cursor.point);
            }
        }

        // Update UI about cursor blinking state changes.
        self.event_proxy.send_event(Event::CursorBlinkingChange);
    }

    /// Move vi mode cursor.
    #[inline]
    pub fn vi_motion(&mut self, motion: ViMotion)
    where
        T: EventListener,
    {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Move cursor.
        self.vi_mode_cursor = self.vi_mode_cursor.motion(self, motion);
        self.vi_mode_recompute_selection();
    }

    /// Move vi cursor to a point in the grid.
    #[inline]
    pub fn vi_goto_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        // Move viewport to make point visible.
        self.scroll_to_point(point);

        // Move vi cursor to the point.
        self.vi_mode_cursor.point = point;

        self.vi_mode_recompute_selection();
    }

    /// Update the active selection to match the vi mode cursor position.
    #[inline]
    fn vi_mode_recompute_selection(&mut self) {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Update only if non-empty selection is present.
        if let Some(selection) = self.selection.as_mut().filter(|s| !s.is_empty()) {
            selection.update(self.vi_mode_cursor.point, Side::Left);
            selection.include_all();
        }
    }

    /// Scroll display to point if it is outside of viewport.
    pub fn scroll_to_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        let display_offset = self.grid.display_offset() as i32;
        let screen_lines = self.grid.screen_lines() as i32;

        if point.line < -display_offset {
            let lines = point.line + display_offset;
            self.scroll_display(Scroll::Delta(-lines.0));
        } else if point.line >= (screen_lines - display_offset) {
            let lines = point.line + display_offset - screen_lines + 1i32;
            self.scroll_display(Scroll::Delta(-lines.0));
        }
    }

    /// Jump to the end of a wide cell.
    pub fn expand_wide(&self, mut point: Point, direction: Direction) -> Point {
        let flags = self.grid[point.line][point.column].flags;

        match direction {
            Direction::Right if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) => {
                point.column = Column(1);
                point.line += 1;
            },
            Direction::Right if flags.contains(Flags::WIDE_CHAR) => {
                point.column = cmp::min(point.column + 1, self.last_column());
            },
            Direction::Left if flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) => {
                if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    point.column -= 1;
                }

                let prev = point.sub(self, Boundary::Grid, 1);
                if self.grid[prev].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    point = prev;
                }
            },
            _ => (),
        }

        point
    }

    #[inline]
    pub fn semantic_escape_chars(&self) -> &str {
        &self.config.semantic_escape_chars
    }

    #[cfg(test)]
    pub(crate) fn set_semantic_escape_chars(&mut self, semantic_escape_chars: &str) {
        self.config.semantic_escape_chars = semantic_escape_chars.into();
    }

    /// Active terminal cursor style.
    ///
    /// While vi mode is active, this will automatically return the vi mode cursor style.
    #[inline]
    pub fn cursor_style(&self) -> CursorStyle {
        if self.mode.contains(TermMode::VI) {
            self.config.vi_mode_cursor_style.unwrap_or(self.cursor_style)
        } else {
            self.cursor_style
        }
    }

    pub fn colors(&self) -> &Colors {
        &self.colors
    }
}

impl<T> Dimensions for Term<T> {
    #[inline]
    fn columns(&self) -> usize {
        self.grid.columns()
    }

    #[inline]
    fn screen_lines(&self) -> usize {
        self.grid.screen_lines()
    }

    #[inline]
    fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    Clipboard,
    Selection,
}

/// Terminal cursor rendering information.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct RenderableCursor {
    pub shape: CursorShape,
    pub point: Point,
}

impl RenderableCursor {
    fn new<T>(term: &Term<T>) -> Self {
        // Cursor position.
        let vi_mode = term.mode().contains(TermMode::VI);
        let mut point = if vi_mode { term.vi_mode_cursor.point } else { term.grid.cursor.point };
        if term.grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
            point.column -= 1;
        }

        // Cursor shape.
        let shape = if !vi_mode && !term.mode().contains(TermMode::SHOW_CURSOR) {
            CursorShape::Hidden
        } else {
            term.cursor_style().shape
        };

        Self { shape, point }
    }
}

/// Visible terminal content.
///
/// This contains all content required to render the current terminal view.
pub struct RenderableContent<'a> {
    pub display_iter: GridIterator<'a, Cell>,
    pub selection: Option<SelectionRange>,
    pub cursor: RenderableCursor,
    pub display_offset: usize,
    pub colors: &'a color::Colors,
    pub mode: TermMode,
}

impl<'a> RenderableContent<'a> {
    fn new<T>(term: &'a Term<T>) -> Self {
        Self {
            display_iter: term.grid().display_iter(),
            display_offset: term.grid().display_offset(),
            cursor: RenderableCursor::new(term),
            selection: term.selection.as_ref().and_then(|s| s.to_range(term)),
            colors: &term.colors,
            mode: *term.mode(),
        }
    }
}

/// Terminal test helpers.
pub mod test {
    use super::*;

    #[cfg(feature = "serde")]
    use serde::{Deserialize, Serialize};

    use crate::event::VoidListener;

    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct TermSize {
        pub columns: usize,
        pub screen_lines: usize,
    }

    impl TermSize {
        pub fn new(columns: usize, screen_lines: usize) -> Self {
            Self { columns, screen_lines }
        }
    }

    impl Dimensions for TermSize {
        fn total_lines(&self) -> usize {
            self.screen_lines()
        }

        fn screen_lines(&self) -> usize {
            self.screen_lines
        }

        fn columns(&self) -> usize {
            self.columns
        }
    }

    /// Construct a terminal from its content as string.
    ///
    /// A `\n` will break line and `\r\n` will break line without wrapping.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use alac_terminal::term::test::mock_term;
    ///
    /// // Create a terminal with the following cells:
    /// //
    /// // [h][e][l][l][o] <- WRAPLINE flag set
    /// // [:][)][ ][ ][ ]
    /// // [t][e][s][t][ ]
    /// mock_term(
    ///     "\
    ///     hello\n:)\r\ntest",
    /// );
    /// ```
    pub fn mock_term(content: &str) -> Term<VoidListener> {
        let lines: Vec<&str> = content.split('\n').collect();
        let num_cols = lines
            .iter()
            .map(|line| line.chars().filter(|c| *c != '\r').map(|c| c.width().unwrap()).sum())
            .max()
            .unwrap_or(0);

        // Create terminal with the appropriate dimensions.
        let size = TermSize::new(num_cols, lines.len());
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Fill terminal with content.
        for (line, text) in lines.iter().enumerate() {
            let line = Line(line as i32);
            if !text.ends_with('\r') && line + 1 != lines.len() {
                term.grid[line][Column(num_cols - 1)].flags.insert(Flags::WRAPLINE);
            }

            let mut index = 0;
            for c in text.chars().take_while(|c| *c != '\r') {
                term.grid[line][Column(index)].c = c;

                // Handle fullwidth characters.
                let width = c.width().unwrap();
                if width == 2 {
                    term.grid[line][Column(index)].flags.insert(Flags::WIDE_CHAR);
                    term.grid[line][Column(index + 1)].flags.insert(Flags::WIDE_CHAR_SPACER);
                }

                index += width;
            }
        }

        term
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::VoidListener;
    use crate::term::test::TermSize;
    use crate::vte::ansi::Color;

    fn term(cols: usize, lines: usize) -> Term<VoidListener> {
        Term::new(Config::default(), &TermSize::new(cols, lines), VoidListener)
    }

    fn row(term: &Term<VoidListener>, line: i32) -> String {
        (0..term.columns()).map(|c| term.grid()[Line(line)][Column(c)].c).collect()
    }

    #[test]
    fn prints_text_and_colors() {
        let mut term = term(10, 3);
        term.advance(b"hi \x1b[31mred\x1b[0m\r\nok");
        assert_eq!(row(&term, 0).trim_end(), "hi red");
        assert_eq!(row(&term, 1).trim_end(), "ok");
        assert_eq!(term.grid()[Line(0)][Column(3)].fg, Color::Indexed(1));
        assert_eq!(term.grid().cursor.point, Point::new(Line(1), Column(2)));
    }

    #[test]
    fn scrollback_is_mirrored() {
        let mut term = term(5, 3);
        for i in 0..8 {
            term.advance(format!("l{i}\r\n").as_bytes());
        }
        assert!(term.history_size() >= 5);
        assert_eq!(row(&term, -(term.history_size() as i32)).trim_end(), "l0");
        term.scroll_display(Scroll::Top);
        assert_eq!(term.grid().display_offset(), term.history_size());
    }

    #[test]
    fn alt_screen_and_modes() {
        let mut term = term(5, 3);
        term.advance(b"main\x1b[?1049h\x1b[H\x1b[?2004halt");
        assert!(term.mode().contains(TermMode::ALT_SCREEN | TermMode::BRACKETED_PASTE));
        assert_eq!(row(&term, 0).trim_end(), "alt");
        term.advance(b"\x1b[?1049l");
        assert!(!term.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(row(&term, 0).trim_end(), "main");
    }

    #[test]
    fn resize_reflows() {
        let mut term = term(10, 3);
        term.advance(b"abcdefghij1234");
        term.resize(TermSize::new(5, 3));
        assert_eq!(term.columns(), 5);
        let all: String = (-(term.history_size() as i32)..3).map(|l| row(&term, l)).collect();
        assert!(all.contains("abcde") && all.contains("fghij") && all.contains("1234"));
    }

    #[test]
    fn wide_chars_and_title() {
        let mut term = term(6, 2);
        term.advance("a漢b\x1b]2;hello\x07".as_bytes());
        assert!(term.grid()[Line(0)][Column(1)].flags.contains(Flags::WIDE_CHAR));
        assert!(term.grid()[Line(0)][Column(2)].flags.contains(Flags::WIDE_CHAR_SPACER));
        assert_eq!(term.title.as_deref(), Some("hello"));
    }
}
