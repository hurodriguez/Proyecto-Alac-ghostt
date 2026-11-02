//! Safe wrapper around libghostty-vt, the engine which parses the PTY stream and owns the
//! terminal state (screen contents, scrollback, modes, colors, cursor).
//!
//! The rest of this crate only reads from the engine: after every chunk of PTY output the
//! [`Term`](super::Term) copies the changed rows into a [`Grid`](crate::grid::Grid) mirror which is
//! used for rendering, selection, search and vi mode.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use ghostty_sys as gt;

use crate::term::cell::{Cell, Flags, Hyperlink};
use crate::term::{ClipboardType, TermMode};
use crate::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor, Rgb};

/// Maximum number of grapheme codepoints read per cell.
const MAX_GRAPHEME: usize = 16;

/// Event raised by the engine while processing input.
pub enum Pending {
    /// Bytes which must be written back to the PTY.
    PtyWrite(Vec<u8>),
    Bell,
    TitleChanged,
    ClipboardStore(ClipboardType, String),
}

/// Data shared with the C callbacks.
#[derive(Default)]
struct Sink {
    events: Vec<Pending>,
    cell_width: u32,
    cell_height: u32,
    cols: u16,
    rows: u16,
}

/// A viewport row, as captured by the engine.
pub struct ViewRow {
    pub id: [u64; 2],
    pub dirty: bool,
    pub wrap: bool,
    /// Only populated when the row needs to be re-read.
    pub cells: Vec<Cell>,
}

/// Cursor state of the active screen.
pub struct CursorState {
    pub column: usize,
    pub line: usize,
    pub pending_wrap: bool,
    pub style: CursorStyle,
}

/// Terminal colors configured by the embedder.
#[derive(Clone)]
pub struct DefaultColors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Rgb>,
    pub palette: [Rgb; 256],
}

pub struct Engine {
    term: gt::GhosttyTerminal,
    state: gt::GhosttyRenderState,
    rows: gt::GhosttyRenderStateRowIterator,
    cells: gt::GhosttyRenderStateRowCells,
    sink: Box<Sink>,
    defaults: Option<DefaultColors>,
}

// SAFETY: the engine is only ever accessed through `&mut` or behind a mutex.
unsafe impl Send for Engine {}

fn rgb(c: gt::GhosttyColorRgb) -> Rgb {
    Rgb { r: c.r, g: c.g, b: c.b }
}

fn mode(value: u16, ansi: bool) -> gt::GhosttyMode {
    (value & 0x7FFF) | ((ansi as u16) << 15)
}

unsafe extern "C" fn on_write_pty(
    _: gt::GhosttyTerminal,
    ud: *mut c_void,
    data: *const u8,
    len: usize,
) {
    let sink = unsafe { &mut *(ud as *mut Sink) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    sink.events.push(Pending::PtyWrite(bytes));
}

unsafe extern "C" fn on_bell(_: gt::GhosttyTerminal, ud: *mut c_void) {
    unsafe { &mut *(ud as *mut Sink) }.events.push(Pending::Bell);
}

unsafe extern "C" fn on_title(_: gt::GhosttyTerminal, ud: *mut c_void) {
    unsafe { &mut *(ud as *mut Sink) }.events.push(Pending::TitleChanged);
}

unsafe extern "C" fn on_size(
    _: gt::GhosttyTerminal,
    ud: *mut c_void,
    out: *mut gt::GhosttySizeReportSize,
) -> bool {
    let sink = unsafe { &*(ud as *const Sink) };
    unsafe {
        (*out).rows = sink.rows;
        (*out).columns = sink.cols;
        (*out).cell_width = sink.cell_width;
        (*out).cell_height = sink.cell_height;
    }
    true
}

unsafe extern "C" fn on_device_attributes(
    _: gt::GhosttyTerminal,
    _: *mut c_void,
    out: *mut gt::GhosttyDeviceAttributes,
) -> bool {
    let attrs = unsafe { &mut *out };
    attrs.primary.conformance_level = 62;
    attrs.primary.features[0] = 22; // ANSI color.
    attrs.primary.features[1] = 52; // Clipboard.
    attrs.primary.num_features = 2;
    attrs.secondary.device_type = 1;
    attrs.secondary.firmware_version = 0;
    attrs.secondary.rom_cartridge = 1;
    attrs.tertiary.unit_id = 0;
    true
}

unsafe extern "C" fn on_clipboard_write(
    _: gt::GhosttyTerminal,
    ud: *mut c_void,
    write: *const gt::GhosttyClipboardWrite,
) {
    let sink = unsafe { &mut *(ud as *mut Sink) };
    let write = unsafe { &*write };
    let ty = if write.location == gt::GHOSTTY_CLIPBOARD_LOCATION_STANDARD {
        ClipboardType::Clipboard
    } else {
        ClipboardType::Selection
    };

    if write.contents_len > 0 {
        let contents = unsafe { std::slice::from_raw_parts(write.contents, write.contents_len) };
        // Prefer plain text, fall back to the first entry.
        let content = contents
            .iter()
            .find(|c| {
                let mime = unsafe { std::slice::from_raw_parts(c.mime.ptr, c.mime.len) };
                mime == b"text/plain"
            })
            .unwrap_or(&contents[0]);
        let data = unsafe { std::slice::from_raw_parts(content.data.ptr, content.data.len) };
        sink.events.push(Pending::ClipboardStore(ty, String::from_utf8_lossy(data).into_owned()));
    } else {
        sink.events.push(Pending::ClipboardStore(ty, String::new()));
    }

    let mut reply: gt::GhosttyClipboardWriteReply = unsafe { zeroed() };
    reply.size = size_of::<gt::GhosttyClipboardWriteReply>();
    reply.result = gt::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
    if let Some(reply_fn) = write.reply {
        unsafe { reply_fn(write, &reply) };
    }
}

impl Engine {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Box<Engine> {
        let mut sink = Box::new(Sink {
            cell_width: 8,
            cell_height: 16,
            cols: cols as u16,
            rows: rows as u16,
            ..Default::default()
        });

        let mut term = null_mut();
        let mut state = null_mut();
        let mut row_iter = null_mut();
        let mut cells = null_mut();
        unsafe {
            gt::ghostty_terminal_new(null(), &mut term, cols as u16, rows as u16);
            gt::ghostty_render_state_new(null(), &mut state);
            gt::ghostty_render_state_row_iterator_new(null(), &mut row_iter);
            gt::ghostty_render_state_row_cells_new(null(), &mut cells);

            let ud = &mut *sink as *mut Sink as *mut c_void;
            gt::ghostty_terminal_set(term, gt::GHOSTTY_TERMINAL_OPT_USERDATA, ud);
            gt::ghostty_terminal_set(
                term,
                gt::GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                on_write_pty as *const c_void,
            );
            gt::ghostty_terminal_set(term, gt::GHOSTTY_TERMINAL_OPT_BELL, on_bell as *const c_void);
            gt::ghostty_terminal_set(
                term,
                gt::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                on_title as *const c_void,
            );
            gt::ghostty_terminal_set(term, gt::GHOSTTY_TERMINAL_OPT_SIZE, on_size as *const c_void);
            gt::ghostty_terminal_set(
                term,
                gt::GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                on_device_attributes as *const c_void,
            );
            gt::ghostty_terminal_set(
                term,
                gt::GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                on_clipboard_write as *const c_void,
            );
        }

        let mut engine =
            Box::new(Engine { term, state, rows: row_iter, cells, sink, defaults: None });
        engine.set_scrollback(scrollback);
        engine
    }

    pub fn set_scrollback(&mut self, lines: usize) {
        unsafe {
            gt::ghostty_terminal_set(
                self.term,
                gt::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                &lines as *const usize as *const c_void,
            );
        }
    }

    pub fn set_default_cursor(&mut self, style: CursorStyle) {
        let shape = match style.shape {
            CursorShape::Underline => gt::GHOSTTY_TERMINAL_CURSOR_STYLE_UNDERLINE,
            CursorShape::Beam => gt::GHOSTTY_TERMINAL_CURSOR_STYLE_BAR,
            CursorShape::HollowBlock => gt::GHOSTTY_TERMINAL_CURSOR_STYLE_BLOCK_HOLLOW,
            _ => gt::GHOSTTY_TERMINAL_CURSOR_STYLE_BLOCK,
        };
        unsafe {
            gt::ghostty_terminal_set(
                self.term,
                gt::GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE,
                &shape as *const _ as *const c_void,
            );
            gt::ghostty_terminal_set(
                self.term,
                gt::GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK,
                &style.blinking as *const bool as *const c_void,
            );
        }
    }

    /// Configure the colors reported to programs (OSC 4/10/11/12 queries).
    pub fn set_default_colors(&mut self, colors: DefaultColors) {
        let c = |c: Rgb| gt::GhosttyColorRgb { r: c.r, g: c.g, b: c.b };
        let palette: Vec<gt::GhosttyColorRgb> = colors.palette.iter().map(|x| c(*x)).collect();
        let (fg, bg) = (c(colors.foreground), c(colors.background));
        unsafe {
            gt::ghostty_terminal_set(
                self.term,
                gt::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                &fg as *const _ as _,
            );
            gt::ghostty_terminal_set(
                self.term,
                gt::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                &bg as *const _ as _,
            );
            match colors.cursor {
                Some(cur) => {
                    let cur = c(cur);
                    gt::ghostty_terminal_set(
                        self.term,
                        gt::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                        &cur as *const _ as _,
                    );
                },
                None => {
                    gt::ghostty_terminal_set(
                        self.term,
                        gt::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                        null(),
                    );
                },
            }
            gt::ghostty_terminal_set(
                self.term,
                gt::GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                palette.as_ptr() as _,
            );
        }
        self.defaults = Some(colors);
    }

    pub fn set_cell_size(&mut self, width: u32, height: u32) {
        self.sink.cell_width = width.max(1);
        self.sink.cell_height = height.max(1);
    }

    /// Feed PTY output to the engine.
    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { gt::ghostty_terminal_vt_write(self.term, bytes.as_ptr(), bytes.len()) };
    }

    pub fn reset(&mut self) {
        unsafe { gt::ghostty_terminal_reset(self.term) };
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.sink.cols = cols as u16;
        self.sink.rows = rows as u16;
        unsafe {
            gt::ghostty_terminal_resize(
                self.term,
                cols as u16,
                rows as u16,
                self.sink.cell_width,
                self.sink.cell_height,
            );
        }
    }

    pub fn take_events(&mut self) -> Vec<Pending> {
        std::mem::take(&mut self.sink.events)
    }

    fn get<T: Default>(&self, key: gt::GhosttyTerminalData) -> Option<T> {
        let mut out = T::default();
        let res =
            unsafe { gt::ghostty_terminal_get(self.term, key, &mut out as *mut T as *mut c_void) };
        (res == gt::GHOSTTY_SUCCESS).then_some(out)
    }

    fn get_mode(&self, value: u16, ansi: bool) -> bool {
        let mut cfg = gt::GhosttyTerminalModeConfig { mode: mode(value, ansi), value: false };
        let res = unsafe {
            gt::ghostty_terminal_get(
                self.term,
                gt::GHOSTTY_TERMINAL_DATA_MODE,
                &mut cfg as *mut _ as *mut c_void,
            )
        };
        res == gt::GHOSTTY_SUCCESS && cfg.value
    }

    pub fn title(&self) -> Option<String> {
        let mut s = gt::GhosttyString { ptr: null(), len: 0 };
        let res = unsafe {
            gt::ghostty_terminal_get(
                self.term,
                gt::GHOSTTY_TERMINAL_DATA_TITLE,
                &mut s as *mut _ as *mut c_void,
            )
        };
        if res != gt::GHOSTTY_SUCCESS || s.ptr.is_null() || s.len == 0 {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    pub fn is_alt_screen(&self) -> bool {
        self.get::<i32>(gt::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN)
            .is_some_and(|s| s == gt::GHOSTTY_TERMINAL_SCREEN_ALTERNATE as i32)
    }

    /// Number of scrollback rows plus visible rows.
    pub fn total_rows(&self) -> usize {
        self.get::<usize>(gt::GHOSTTY_TERMINAL_DATA_TOTAL_ROWS).unwrap_or(0)
    }

    /// Terminal modes translated to the application's flags.
    pub fn term_mode(&self) -> TermMode {
        let mut m = TermMode::empty();
        m.set(TermMode::SHOW_CURSOR, self.get_mode(25, false));
        m.set(TermMode::APP_CURSOR, self.get_mode(1, false));
        m.set(TermMode::APP_KEYPAD, self.get_mode(66, false));
        m.set(TermMode::MOUSE_REPORT_CLICK, self.get_mode(1000, false) || self.get_mode(9, false));
        m.set(TermMode::MOUSE_DRAG, self.get_mode(1002, false));
        m.set(TermMode::MOUSE_MOTION, self.get_mode(1003, false));
        m.set(TermMode::SGR_MOUSE, self.get_mode(1006, false));
        m.set(TermMode::UTF8_MOUSE, self.get_mode(1005, false));
        m.set(TermMode::ALTERNATE_SCROLL, self.get_mode(1007, false));
        m.set(TermMode::FOCUS_IN_OUT, self.get_mode(1004, false));
        m.set(TermMode::BRACKETED_PASTE, self.get_mode(2004, false));
        m.set(TermMode::LINE_WRAP, self.get_mode(7, false));
        m.set(TermMode::ORIGIN, self.get_mode(6, false));
        m.set(TermMode::INSERT, self.get_mode(4, true));
        m.set(TermMode::LINE_FEED_NEW_LINE, self.get_mode(20, true));
        m.set(TermMode::ALT_SCREEN, self.is_alt_screen());

        let kitty = self.get::<u8>(gt::GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS).unwrap_or(0);
        m.set(TermMode::DISAMBIGUATE_ESC_CODES, kitty & 1 != 0);
        m.set(TermMode::REPORT_EVENT_TYPES, kitty & 2 != 0);
        m.set(TermMode::REPORT_ALTERNATE_KEYS, kitty & 4 != 0);
        m.set(TermMode::REPORT_ALL_KEYS_AS_ESC, kitty & 8 != 0);
        m.set(TermMode::REPORT_ASSOCIATED_TEXT, kitty & 16 != 0);
        m
    }

    pub fn cursor(&self) -> CursorState {
        let column = self.get::<u16>(gt::GHOSTTY_TERMINAL_DATA_CURSOR_X).unwrap_or(0) as usize;
        let line = self.get::<u16>(gt::GHOSTTY_TERMINAL_DATA_CURSOR_Y).unwrap_or(0) as usize;
        let pending_wrap =
            self.get::<bool>(gt::GHOSTTY_TERMINAL_DATA_CURSOR_PENDING_WRAP).unwrap_or(false);
        // The visual style comes from the render state, which `read_viewport` refreshed.
        let mut visual: i32 = gt::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK as i32;
        unsafe {
            gt::ghostty_render_state_get(
                self.state,
                gt::GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE,
                &mut visual as *mut _ as *mut c_void,
            );
        }
        let shape = match visual {
            v if v == gt::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE as i32 => {
                CursorShape::Underline
            },
            v if v == gt::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR as i32 => CursorShape::Beam,
            v if v == gt::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW as i32 => {
                CursorShape::HollowBlock
            },
            _ => CursorShape::Block,
        };
        let blinking = self.get_mode(12, false);
        CursorState { column, line, pending_wrap, style: CursorStyle { shape, blinking } }
    }

    /// Colors which were changed by the running program (OSC 4/10/11/12) relative to the
    /// configured defaults, as `(index, color)` pairs using the application's color indices.
    pub fn color_overrides(&self) -> Vec<(usize, Rgb)> {
        let Some(defaults) = &self.defaults else { return Vec::new() };
        let mut out = Vec::new();

        let mut palette = [gt::GhosttyColorRgb { r: 0, g: 0, b: 0 }; 256];
        let res = unsafe {
            gt::ghostty_terminal_get(
                self.term,
                gt::GHOSTTY_TERMINAL_DATA_COLOR_PALETTE,
                palette.as_mut_ptr() as *mut c_void,
            )
        };
        if res == gt::GHOSTTY_SUCCESS {
            for (i, c) in palette.iter().enumerate() {
                if rgb(*c) != defaults.palette[i] {
                    out.push((i, rgb(*c)));
                }
            }
        }

        let mut color = |key, default: Option<Rgb>, index: usize| {
            let mut c = gt::GhosttyColorRgb { r: 0, g: 0, b: 0 };
            let res = unsafe {
                gt::ghostty_terminal_get(self.term, key, &mut c as *mut _ as *mut c_void)
            };
            if res == gt::GHOSTTY_SUCCESS && Some(rgb(c)) != default {
                out.push((index, rgb(c)));
            }
        };
        color(
            gt::GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND,
            Some(defaults.foreground),
            NamedColor::Foreground as usize,
        );
        color(
            gt::GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND,
            Some(defaults.background),
            NamedColor::Background as usize,
        );
        color(gt::GHOSTTY_TERMINAL_DATA_COLOR_CURSOR, defaults.cursor, NamedColor::Cursor as usize);
        out
    }

    /// Capture the visible rows. Cells are only read for dirty rows unless `all` is set.
    pub fn read_viewport(&mut self, all: bool) -> Vec<ViewRow> {
        let mut out = Vec::new();
        unsafe {
            if gt::ghostty_render_state_update(self.state, self.term) != gt::GHOSTTY_SUCCESS {
                return out;
            }
            let mut full_dirty: i32 = 0;
            gt::ghostty_render_state_get(
                self.state,
                gt::GHOSTTY_RENDER_STATE_DATA_DIRTY,
                &mut full_dirty as *mut _ as _,
            );
            let all = all || full_dirty == gt::GHOSTTY_RENDER_STATE_DIRTY_FULL as i32;

            gt::ghostty_render_state_get(
                self.state,
                gt::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                &mut self.rows as *mut _ as _,
            );
            while gt::ghostty_render_state_row_iterator_next(self.rows) {
                let mut id = gt::GhosttyRenderStateRowId { bits: [0; 2] };
                gt::ghostty_render_state_row_get(
                    self.rows,
                    gt::GHOSTTY_RENDER_STATE_ROW_DATA_ID,
                    &mut id as *mut _ as _,
                );
                let mut dirty = false;
                gt::ghostty_render_state_row_get(
                    self.rows,
                    gt::GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY,
                    &mut dirty as *mut _ as _,
                );
                let mut raw: gt::GhosttyRow = 0;
                gt::ghostty_render_state_row_get(
                    self.rows,
                    gt::GHOSTTY_RENDER_STATE_ROW_DATA_RAW,
                    &mut raw as *mut _ as _,
                );
                let mut wrap = false;
                gt::ghostty_row_get(raw, gt::GHOSTTY_ROW_DATA_WRAP, &mut wrap as *mut _ as _);

                let y = out.len();
                let mut cells = Vec::new();
                if dirty || all {
                    gt::ghostty_render_state_row_get(
                        self.rows,
                        gt::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                        &mut self.cells as *mut _ as _,
                    );
                    let mut x = 0u16;
                    while gt::ghostty_render_state_row_cells_next(self.cells) {
                        cells.push(self.read_render_cell(y, x));
                        x += 1;
                    }
                    let clean = false;
                    gt::ghostty_render_state_row_set(
                        self.rows,
                        gt::GHOSTTY_RENDER_STATE_ROW_OPTION_DIRTY,
                        &clean as *const _ as _,
                    );
                }
                out.push(ViewRow { id: id.bits, dirty: dirty || all, wrap, cells });
            }
            let clean = gt::GHOSTTY_RENDER_STATE_DIRTY_FALSE as i32;
            gt::ghostty_render_state_set(
                self.state,
                gt::GHOSTTY_RENDER_STATE_OPTION_DIRTY,
                &clean as *const _ as _,
            );
        }
        out
    }

    /// Read a row of the whole screen (scrollback followed by the active area).
    ///
    /// Returns the row cells and whether the row is soft-wrapped.
    pub fn read_screen_row(&mut self, y: usize, cols: usize) -> Option<(Vec<Cell>, bool)> {
        unsafe {
            let point = gt::GhosttyPoint {
                tag: gt::GHOSTTY_POINT_TAG_SCREEN,
                value: gt::GhosttyPointValue {
                    coordinate: gt::GhosttyPointCoordinate { x: 0, y: y as u32 },
                },
            };
            let mut reference: gt::GhosttyGridRef = zeroed();
            reference.size = size_of::<gt::GhosttyGridRef>();
            if gt::ghostty_terminal_grid_ref(self.term, point, &mut reference)
                != gt::GHOSTTY_SUCCESS
            {
                return None;
            }

            let mut wrap = false;
            let mut row: gt::GhosttyRow = 0;
            if gt::ghostty_grid_ref_row(&reference, &mut row) == gt::GHOSTTY_SUCCESS {
                gt::ghostty_row_get(row, gt::GHOSTTY_ROW_DATA_WRAP, &mut wrap as *mut _ as _);
            }

            let mut cells = Vec::with_capacity(cols);
            for x in 0..cols {
                reference.x = x as u16;
                cells.push(self.read_ref_cell(&reference));
            }
            Some((cells, wrap))
        }
    }

    unsafe fn read_render_cell(&self, y: usize, x: u16) -> Cell {
        unsafe {
            let mut raw: gt::GhosttyCell = 0;
            gt::ghostty_render_state_row_cells_get(
                self.cells,
                gt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                &mut raw as *mut _ as _,
            );

            let mut n: u32 = 0;
            gt::ghostty_render_state_row_cells_get(
                self.cells,
                gt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                &mut n as *mut _ as _,
            );
            let mut buf = [0u32; MAX_GRAPHEME];
            if n > 0 {
                let mut tmp = vec![0u32; n as usize];
                gt::ghostty_render_state_row_cells_get(
                    self.cells,
                    gt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                    tmp.as_mut_ptr() as _,
                );
                let take = tmp.len().min(MAX_GRAPHEME);
                buf[..take].copy_from_slice(&tmp[..take]);
                n = take as u32;
            }

            let mut has_style = false;
            gt::ghostty_cell_get(
                raw,
                gt::GHOSTTY_CELL_DATA_HAS_STYLING,
                &mut has_style as *mut _ as _,
            );
            let style = has_style.then(|| {
                let mut style: gt::GhosttyStyle = zeroed();
                style.size = size_of::<gt::GhosttyStyle>();
                gt::ghostty_render_state_row_cells_get(
                    self.cells,
                    gt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                    &mut style as *mut _ as _,
                );
                style
            });

            let mut has_link = false;
            gt::ghostty_cell_get(
                raw,
                gt::GHOSTTY_CELL_DATA_HAS_HYPERLINK,
                &mut has_link as *mut _ as _,
            );
            let link = if has_link { self.hyperlink_at(y, x) } else { None };

            convert_cell(raw, &buf[..n as usize], style.as_ref(), link)
        }
    }

    unsafe fn read_ref_cell(&self, reference: &gt::GhosttyGridRef) -> Cell {
        unsafe {
            let mut raw: gt::GhosttyCell = 0;
            gt::ghostty_grid_ref_cell(reference, &mut raw);

            let mut buf = [0u32; MAX_GRAPHEME];
            let mut n = 0usize;
            let mut has_text = false;
            gt::ghostty_cell_get(raw, gt::GHOSTTY_CELL_DATA_HAS_TEXT, &mut has_text as *mut _ as _);
            if has_text {
                gt::ghostty_grid_ref_graphemes(reference, buf.as_mut_ptr(), MAX_GRAPHEME, &mut n);
                n = n.min(MAX_GRAPHEME);
            }

            let mut has_style = false;
            gt::ghostty_cell_get(
                raw,
                gt::GHOSTTY_CELL_DATA_HAS_STYLING,
                &mut has_style as *mut _ as _,
            );
            let style = has_style.then(|| {
                let mut style: gt::GhosttyStyle = zeroed();
                style.size = size_of::<gt::GhosttyStyle>();
                gt::ghostty_grid_ref_style(reference, &mut style);
                style
            });

            let mut has_link = false;
            gt::ghostty_cell_get(
                raw,
                gt::GHOSTTY_CELL_DATA_HAS_HYPERLINK,
                &mut has_link as *mut _ as _,
            );
            let link = if has_link {
                let mut len = 0usize;
                gt::ghostty_grid_ref_hyperlink_uri(reference, null_mut(), 0, &mut len);
                let mut uri = vec![0u8; len];
                gt::ghostty_grid_ref_hyperlink_uri(reference, uri.as_mut_ptr(), len, &mut len);
                uri.truncate(len);
                (!uri.is_empty()).then(|| String::from_utf8_lossy(&uri).into_owned())
            } else {
                None
            };

            convert_cell(raw, &buf[..n], style.as_ref(), link)
        }
    }

    /// Hyperlink of a cell in the active area.
    fn hyperlink_at(&self, y: usize, x: u16) -> Option<String> {
        unsafe {
            let point = gt::GhosttyPoint {
                tag: gt::GHOSTTY_POINT_TAG_ACTIVE,
                value: gt::GhosttyPointValue {
                    coordinate: gt::GhosttyPointCoordinate { x, y: y as u32 },
                },
            };
            let mut reference: gt::GhosttyGridRef = zeroed();
            reference.size = size_of::<gt::GhosttyGridRef>();
            if gt::ghostty_terminal_grid_ref(self.term, point, &mut reference)
                != gt::GHOSTTY_SUCCESS
            {
                return None;
            }
            let mut len = 0usize;
            gt::ghostty_grid_ref_hyperlink_uri(&reference, null_mut(), 0, &mut len);
            let mut uri = vec![0u8; len];
            gt::ghostty_grid_ref_hyperlink_uri(&reference, uri.as_mut_ptr(), len, &mut len);
            uri.truncate(len);
            (!uri.is_empty()).then(|| String::from_utf8_lossy(&uri).into_owned())
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            gt::ghostty_render_state_row_cells_free(self.cells);
            gt::ghostty_render_state_row_iterator_free(self.rows);
            gt::ghostty_render_state_free(self.state);
            gt::ghostty_terminal_free(self.term);
        }
    }
}

fn style_color(color: &gt::GhosttyStyleColor, default: NamedColor) -> Color {
    unsafe {
        match color.tag {
            t if t == gt::GHOSTTY_STYLE_COLOR_PALETTE => Color::Indexed(color.value.palette),
            t if t == gt::GHOSTTY_STYLE_COLOR_RGB => Color::Spec(rgb(color.value.rgb)),
            _ => Color::Named(default),
        }
    }
}

/// Convert an engine cell into the application's [`Cell`].
unsafe fn convert_cell(
    raw: gt::GhosttyCell,
    graphemes: &[u32],
    style: Option<&gt::GhosttyStyle>,
    link: Option<String>,
) -> Cell {
    let mut cell = Cell::default();

    unsafe {
        let mut wide: i32 = 0;
        gt::ghostty_cell_get(raw, gt::GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as _);
        match wide {
            w if w == gt::GHOSTTY_CELL_WIDE_WIDE as i32 => cell.flags.insert(Flags::WIDE_CHAR),
            w if w == gt::GHOSTTY_CELL_WIDE_SPACER_TAIL as i32 => {
                cell.flags.insert(Flags::WIDE_CHAR_SPACER)
            },
            w if w == gt::GHOSTTY_CELL_WIDE_SPACER_HEAD as i32 => {
                cell.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER)
            },
            _ => (),
        }

        // Cells which only carry a background color.
        let mut tag: i32 = 0;
        gt::ghostty_cell_get(raw, gt::GHOSTTY_CELL_DATA_CONTENT_TAG, &mut tag as *mut _ as _);
        if tag == gt::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE as i32 {
            let mut idx: u8 = 0;
            gt::ghostty_cell_get(raw, gt::GHOSTTY_CELL_DATA_COLOR_PALETTE, &mut idx as *mut _ as _);
            cell.bg = Color::Indexed(idx);
        } else if tag == gt::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB as i32 {
            let mut c = gt::GhosttyColorRgb { r: 0, g: 0, b: 0 };
            gt::ghostty_cell_get(raw, gt::GHOSTTY_CELL_DATA_COLOR_RGB, &mut c as *mut _ as _);
            cell.bg = Color::Spec(rgb(c));
        }
    }

    if let Some((first, rest)) = graphemes.split_first() {
        cell.c = char::from_u32(*first).unwrap_or(' ');
        for c in rest.iter().filter_map(|c| char::from_u32(*c)) {
            cell.push_zerowidth(c);
        }
    }

    if let Some(style) = style {
        cell.fg = style_color(&style.fg_color, NamedColor::Foreground);
        if !matches!(cell.bg, Color::Indexed(_) | Color::Spec(_)) {
            cell.bg = style_color(&style.bg_color, NamedColor::Background);
        }
        if let Color::Indexed(_) | Color::Spec(_) =
            style_color(&style.underline_color, NamedColor::Foreground)
        {
            cell.set_underline_color(Some(style_color(
                &style.underline_color,
                NamedColor::Foreground,
            )));
        }

        let f = &mut cell.flags;
        f.set(Flags::BOLD, style.bold);
        f.set(Flags::ITALIC, style.italic);
        f.set(Flags::DIM, style.faint);
        f.set(Flags::INVERSE, style.inverse);
        f.set(Flags::HIDDEN, style.invisible);
        f.set(Flags::STRIKEOUT, style.strikethrough);
        match style.underline {
            u if u == gt::GHOSTTY_SGR_UNDERLINE_SINGLE as i32 => f.insert(Flags::UNDERLINE),
            u if u == gt::GHOSTTY_SGR_UNDERLINE_DOUBLE as i32 => f.insert(Flags::DOUBLE_UNDERLINE),
            u if u == gt::GHOSTTY_SGR_UNDERLINE_CURLY as i32 => f.insert(Flags::UNDERCURL),
            u if u == gt::GHOSTTY_SGR_UNDERLINE_DOTTED as i32 => f.insert(Flags::DOTTED_UNDERLINE),
            u if u == gt::GHOSTTY_SGR_UNDERLINE_DASHED as i32 => f.insert(Flags::DASHED_UNDERLINE),
            _ => (),
        }
    }

    if let Some(uri) = link {
        cell.set_hyperlink(Some(Hyperlink::new(Some(uri.clone()), uri)));
    }

    cell
}
