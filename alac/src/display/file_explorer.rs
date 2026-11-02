//! Side panel listing the files of the terminal's working directory.
//!
//! The panel only holds data while it is visible: hiding it frees the entry list.

use std::fs;
use std::path::{Path, PathBuf};

use crossfont::Metrics;

use alac_terminal::index::{Column, Point};

use crate::config::UiConfig;
use crate::config::explorer::Explorer;
use crate::display::color::Rgb;
use crate::display::tab_bar::mix;
use crate::display::{Display, SizeInfo};
use crate::gl;
use crate::renderer::rects::RenderRect;

/// Rows above the entries: directory header and parent link.
const HEADER_ROWS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExplorerHit {
    /// Directory name row, jumps back to the terminal's directory.
    Header,
    /// The `..` row.
    Parent,
    /// Index into the entry list.
    Entry(usize),
}

#[derive(Default)]
pub struct FileExplorer {
    visible: bool,
    /// Directory shown in the panel.
    root: PathBuf,
    /// Last working directory reported by the terminal.
    cwd: Option<PathBuf>,
    entries: Vec<Entry>,
    scroll: usize,
    /// Fractional scroll left over from smooth scrolling.
    scroll_accum: f32,
    /// Rows that fit on screen, updated on draw.
    rows: usize,
    hover: Option<ExplorerHit>,
    /// Panel width in columns and largest window share, from the config.
    columns: usize,
    share: f32,
}

impl FileExplorer {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    fn set_root(&mut self, root: PathBuf, config: &Explorer) {
        self.entries = read_entries(&root, config);
        self.root = root;
        self.scroll = 0;
        self.hover = None;
    }

    fn visible_entries(&self) -> usize {
        self.rows.saturating_sub(HEADER_ROWS)
    }

    fn clamp_scroll(&mut self) {
        let max = self.entries.len().saturating_sub(self.visible_entries());
        self.scroll = self.scroll.min(max);
    }
}

/// List `path`, directories first, hidden and ignored files left out.
fn read_entries(path: &Path, config: &Explorer) -> Vec<Entry> {
    let Ok(dir) = fs::read_dir(path) else { return Vec::new() };

    let mut entries: Vec<Entry> = dir
        .filter_map(Result::ok)
        .filter_map(|item| {
            let name = item.file_name().into_string().ok()?;
            if (!config.show_hidden && name.starts_with('.')) || config.ignore.contains(&name) {
                return None;
            }

            let file_type = item.file_type().ok()?;
            let is_dir = if file_type.is_symlink() { item.path().is_dir() } else { file_type.is_dir() };
            Some(Entry { name, is_dir })
        })
        .take(config.max_entries())
        .collect();

    sort_entries(&mut entries);
    entries
}

/// Color family of an entry, mapped to the terminal palette when drawing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Tone {
    Dir,
    Red,
    Green,
    Yellow,
    Magenta,
    Cyan,
    Plain,
}

/// Icon and color for an entry, picked from its extension.
fn classify(name: &str, is_dir: bool) -> (char, Tone) {
    if is_dir {
        return ('📁', Tone::Dir);
    }

    let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext).to_ascii_lowercase();
    match extension.as_str() {
        "rs" => ('🦀', Tone::Red),
        "py" | "pyw" | "ipynb" => ('🐍', Tone::Yellow),
        "js" | "mjs" | "cjs" | "ts" | "jsx" | "tsx" => ('🟨', Tone::Yellow),
        "c" | "h" | "cc" | "cpp" | "hpp" | "go" | "java" | "kt" | "swift" | "rb" | "php" | "lua" => {
            ('🧩', Tone::Green)
        },
        "sh" | "zsh" | "bash" | "fish" => ('🐚', Tone::Green),
        "html" | "htm" | "css" | "scss" | "xml" | "svg" => ('🌐', Tone::Magenta),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "icns" | "bmp" => ('🎨', Tone::Magenta),
        "toml" | "yaml" | "yml" | "json" | "ini" | "conf" | "cfg" | "plist" | "lock" => {
            ('🔧', Tone::Cyan)
        },
        "md" | "txt" | "rst" | "pdf" | "log" => ('📝', Tone::Plain),
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "dmg" | "7z" => ('📦', Tone::Red),
        _ => ('📄', Tone::Plain),
    }
}

fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase()));
}

/// Fit `text` in `columns` characters, ending with an ellipsis when it was cut.
fn truncate(text: &str, columns: usize) -> String {
    if text.chars().count() <= columns {
        return text.to_owned();
    }

    let mut cut: String = text.chars().take(columns.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

impl Display {
    /// Width taken by the panel for a window and cell width, `0.` when hidden.
    pub fn explorer_width_for(&self, window_width: f32, cell_width: f32) -> f32 {
        if !self.explorer.visible || cell_width <= 0. {
            return 0.;
        }

        let columns = (self.explorer.columns as f32).min((window_width * self.explorer.share / cell_width).floor());
        (columns * cell_width).max(0.)
    }

    fn explorer_width(&self) -> f32 {
        self.explorer_width_for(self.size_info.width(), self.size_info.cell_width())
    }

    /// Apply a reloaded config, returns `true` when the panel needs a new layout and redraw.
    pub fn update_explorer_config(&mut self, config: &Explorer) -> bool {
        if !self.explorer.visible {
            return false;
        }

        let (columns, share) = (config.width(), config.max_width_share());
        self.explorer.columns = columns;
        self.explorer.share = share;
        if let Some(root) = Some(self.explorer.root.clone()).filter(|root| !root.as_os_str().is_empty()) {
            self.explorer.set_root(root, config);
        }
        true
    }

    pub fn explorer_visible(&self) -> bool {
        self.explorer.visible
    }

    pub fn explorer(&self) -> &FileExplorer {
        &self.explorer
    }

    pub(super) fn explorer_redraw(&mut self) {
        self.damage_tracker.frame().mark_fully_damaged();
        self.window.request_redraw();
    }

    /// Show or hide the panel, returns the new visibility.
    pub fn toggle_explorer(&mut self, cwd: Option<PathBuf>, config: &Explorer) -> bool {
        let show = !self.explorer.visible;

        // Hiding gives the memory back.
        self.explorer = FileExplorer::default();
        if show {
            self.explorer.visible = true;
            self.explorer.columns = config.width();
            self.explorer.share = config.max_width_share();
            self.explorer.cwd = cwd.clone();
            self.explorer.set_root(cwd.unwrap_or_default(), config);
        }

        // The grid changes size, so layout has to be recomputed.
        self.pending_update.dirty = true;
        self.explorer_redraw();
        self.explorer.visible
    }

    /// Follow the terminal's directory, returns `true` when the panel changed.
    pub fn sync_explorer(&mut self, cwd: Option<PathBuf>, config: &Explorer) -> bool {
        let Some(cwd) = cwd else { return false };
        if !self.explorer.visible || self.explorer.cwd.as_ref() == Some(&cwd) {
            return false;
        }

        self.explorer.cwd = Some(cwd.clone());
        self.explorer.set_root(cwd, config);
        self.explorer_redraw();
        true
    }

    /// Show another directory in the panel without touching the terminal.
    pub fn explorer_open_dir(&mut self, root: PathBuf, config: &Explorer) {
        self.explorer.set_root(root, config);
        self.explorer_redraw();
    }

    /// Terminal's last known directory.
    pub fn explorer_cwd(&self) -> Option<PathBuf> {
        self.explorer.cwd.clone()
    }

    /// Scroll the list by `lines`, positive going down.
    pub fn explorer_scroll(&mut self, lines: f32) {
        self.explorer.scroll_accum += lines;
        let whole = self.explorer.scroll_accum.trunc();
        self.explorer.scroll_accum -= whole;

        let current = self.explorer.scroll;
        self.explorer.scroll = current.saturating_add_signed(whole as isize);
        self.explorer.clamp_scroll();

        if self.explorer.scroll != current {
            self.explorer_redraw();
        }
    }

    /// Element under the pixel, if the pixel is inside the panel.
    pub fn explorer_hit(&self, x: usize, y: usize) -> Option<Option<ExplorerHit>> {
        let width = self.explorer_width();
        let top = self.tab_bar_height();
        if width == 0. || (x as f32) >= width || (y as f32) < top {
            return None;
        }

        let row = ((y as f32 - top) / self.size_info.cell_height()) as usize;
        let hit = match row {
            _ if row >= self.explorer.rows => None,
            0 => Some(ExplorerHit::Header),
            1 => Some(ExplorerHit::Parent),
            _ => {
                let index = self.explorer.scroll + row - HEADER_ROWS;
                (index < self.explorer.entries.len()).then_some(ExplorerHit::Entry(index))
            },
        };

        Some(hit)
    }

    /// Update the hovered row, returns `true` when it changed.
    pub fn set_explorer_hover(&mut self, position: Option<(usize, usize)>) -> bool {
        let hover = position.and_then(|(x, y)| self.explorer_hit(x, y).flatten());
        if hover == self.explorer.hover {
            return false;
        }

        self.explorer.hover = hover;
        self.explorer_redraw();
        true
    }

    pub fn draw_file_explorer(&mut self, config: &UiConfig, metrics: &Metrics) {
        let panel_width = self.explorer_width();
        if panel_width == 0. {
            return;
        }

        let size = self.size_info;
        let height = size.height();
        let (cell_width, cell_height) = (size.cell_width(), size.cell_height());
        let top = self.tab_bar_height();
        let panel_height = height - top;
        let rows = (panel_height / cell_height) as usize;
        if rows == 0 {
            return;
        }

        self.explorer.rows = rows;
        self.explorer.clamp_scroll();

        let background = config.colors.primary.background;
        let foreground = config.colors.primary.foreground;
        let panel_bg = mix(background, foreground, 0.05);
        let header_bg = mix(panel_bg, foreground, 0.06);
        let hover_bg = mix(panel_bg, foreground, 0.12);
        let separator = mix(panel_bg, foreground, 0.18);
        let file_fg = mix(foreground, background, 0.15);
        let dir_fg = config.colors.normal.blue;
        let colors = &config.colors.normal;
        let tone_color = |tone: Tone| match tone {
            Tone::Dir => dir_fg,
            Tone::Red => colors.red,
            Tone::Green => colors.green,
            Tone::Yellow => colors.yellow,
            Tone::Magenta => colors.magenta,
            Tone::Cyan => colors.cyan,
            Tone::Plain => file_fg,
        };

        let rects = vec![
            RenderRect::new(0., top, panel_width, panel_height, panel_bg, 1.),
            RenderRect::new(panel_width - 1., top, 1., panel_height, separator, 0.8),
        ];
        self.renderer.draw_rects(&size, metrics, rects);

        // Text in a viewport matching the panel, rows counted from its top.
        let text_height = rows as f32 * cell_height;
        let panel_size = SizeInfo::new(panel_width, text_height, cell_width, cell_height, 0., 0., false);
        unsafe {
            gl::Viewport(0, (panel_height - text_height) as i32, panel_width as i32, text_height as i32);
        }
        self.renderer.set_text_projection(&panel_size);

        let columns = (panel_width / cell_width) as usize;
        let explorer = &self.explorer;
        let mut lines: Vec<(String, Rgb, Rgb)> = Vec::with_capacity(rows);

        let title = explorer
            .root
            .file_name()
            .map_or_else(|| explorer.root.display().to_string(), |name| name.to_string_lossy().into_owned());
        let bg_for = |hit, base| if explorer.hover == Some(hit) { hover_bg } else { base };
        let header_bg = bg_for(ExplorerHit::Header, header_bg);
        // A wide icon takes two columns, the character after it is its spacer.
        lines.push((format!(" 📂  {title}"), foreground, header_bg));

        lines.push(("  ..".to_owned(), dir_fg, bg_for(ExplorerHit::Parent, panel_bg)));

        let visible = rows.saturating_sub(HEADER_ROWS);
        for (offset, entry) in explorer.entries.iter().skip(explorer.scroll).take(visible).enumerate() {
            let index = explorer.scroll + offset;
            let bg = bg_for(ExplorerHit::Entry(index), panel_bg);
            let (icon, tone) = classify(&entry.name, entry.is_dir);
            let suffix = if entry.is_dir { "/" } else { "" };
            lines.push((format!(" {icon}  {}{suffix}", entry.name), tone_color(tone), bg));
        }

        for (row, (text, fg, bg)) in lines.into_iter().enumerate() {
            // Pad to the panel width, so the row background covers it.
            let mut text = truncate(&text, columns.saturating_sub(1));
            let pad = columns.saturating_sub(text.chars().count());
            text.extend(std::iter::repeat_n(' ', pad));

            self.renderer.draw_string(
                Point::new(row, Column(0)),
                fg,
                bg,
                text.chars(),
                &panel_size,
                &mut self.glyph_cache,
            );
        }

        self.renderer.resize(&size);
        self.damage_tracker.frame().add_viewport_rect(
            &size,
            0,
            top as i32,
            panel_width as i32,
            panel_height as i32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry { name: name.into(), is_dir }
    }

    #[test]
    fn directories_come_first_then_case_insensitive() {
        let mut entries =
            vec![entry("b.txt", false), entry("Zeta", true), entry("A.txt", false), entry("alpha", true)];
        sort_entries(&mut entries);

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zeta", "A.txt", "b.txt"]);
    }

    #[test]
    fn classify_by_extension() {
        assert_eq!(classify("src", true), ('📁', Tone::Dir));
        assert_eq!(classify("main.RS", false), ('🦀', Tone::Red));
        assert_eq!(classify("app.py", false), ('🐍', Tone::Yellow));
        assert_eq!(classify("Cargo.toml", false), ('🔧', Tone::Cyan));
        assert_eq!(classify("Makefile", false), ('📄', Tone::Plain));
        assert_eq!(classify("archive.tar.gz", false), ('📦', Tone::Red));
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn listing_skips_hidden_and_caps_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), "").unwrap();
        fs::write(dir.path().join("file.txt"), "").unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();

        let entries = read_entries(dir.path(), &Explorer::default());
        assert_eq!(entries, vec![entry("folder", true), entry("file.txt", false)]);
    }

    #[test]
    fn listing_honors_show_hidden_and_ignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), "").unwrap();
        fs::write(dir.path().join("file.txt"), "").unwrap();
        fs::create_dir(dir.path().join("node_modules")).unwrap();

        let mut config = Explorer::default();
        config.show_hidden = true;
        config.ignore = vec![String::from("node_modules")];
        let entries = read_entries(dir.path(), &config);
        assert_eq!(entries, vec![entry(".hidden", false), entry("file.txt", false)]);
    }

    #[test]
    fn missing_directory_is_empty() {
        assert!(read_entries(Path::new("/definitely/not/here"), &Explorer::default()).is_empty());
    }
}
