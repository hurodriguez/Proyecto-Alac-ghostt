//! Custom tab bar drawn above the terminal grid, replacing the native macOS one.

use crossfont::Metrics;

use alac_terminal::index::{Column, Point};

use crate::config::UiConfig;
use crate::config::window::Decorations;
use crate::display::color::Rgb;
use crate::display::{Display, SizeInfo};
use crate::gl;
use crate::macos::tabs::{self, TabItem};
use crate::renderer::rects::RenderRect;

/// Width reserved for the traffic lights with transparent decorations, in points.
const TRAFFIC_LIGHTS_INSET: f32 = 76.;
/// Minimum height of the bar in points, to fit the traffic lights.
const MIN_BAR_HEIGHT: f32 = 32.;
/// Columns on the right of a tab reserved for the close button.
const CLOSE_COLUMNS: usize = 3;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TabHit {
    Tab(usize),
    Close(usize),
    New,
}

#[derive(Default)]
pub struct TabBar {
    items: Vec<TabItem>,
    /// Horizontal extents of every clickable element, updated on draw.
    hits: Vec<(f32, f32, TabHit)>,
    /// Element currently under the cursor.
    hover: Option<TabHit>,
    /// Keep the bar even with a single tab, so the grid clears the traffic lights.
    always: bool,
}

impl TabBar {
    fn visible(&self) -> bool {
        self.items.len() > 1 || (self.always && !self.items.is_empty())
    }
}

impl Display {
    /// Height reserved above the grid by the tab bar.
    pub fn tab_bar_height(&self) -> f32 {
        if !self.tab_bar.visible() {
            return 0.;
        }

        let scale_factor = self.window.scale_factor as f32;
        (self.size_info.cell_height() * 1.5).max(MIN_BAR_HEIGHT * scale_factor).round()
    }

    /// Keep the bar visible with a single tab, returns `true` when the setting changed.
    pub fn set_tab_bar_always_visible(&mut self, always: bool) {
        self.tab_bar.always = always;
    }

    /// Refresh the list of tabs, returns `true` when it changed.
    pub fn sync_tabs(&mut self) -> bool {
        let items = tabs::snapshot(&self.raw_window_handle);
        if items == self.tab_bar.items {
            return false;
        }

        self.tab_bar.items = items;
        self.pending_update.dirty = true;
        self.damage_tracker.frame().mark_fully_damaged();
        self.window.request_redraw();
        true
    }

    /// Element under the pixel, if the pixel is inside the bar.
    ///
    /// The outer option tells if the bar was hit at all, the inner one if there is an element.
    pub fn tab_bar_hit(&self, x: usize, y: usize) -> Option<Option<TabHit>> {
        if (y as f32) >= self.tab_bar_height() || !self.tab_bar.visible() {
            return None;
        }

        let x = x as f32;
        Some(self.tab_bar.hits.iter().find(|(start, end, _)| x >= *start && x < *end).map(|h| h.2))
    }

    /// Update the hovered element from the pixel position, returns `true` when it changed.
    pub fn set_tab_hover(&mut self, position: Option<(usize, usize)>) -> bool {
        let hover = position.and_then(|(x, y)| self.tab_bar_hit(x, y).flatten());
        if hover == self.tab_bar.hover {
            return false;
        }

        self.tab_bar.hover = hover;
        self.damage_tracker.frame().mark_fully_damaged();
        self.window.request_redraw();
        true
    }

    pub fn draw_tab_bar(&mut self, config: &UiConfig, metrics: &Metrics) {
        let bar_height = self.tab_bar_height();
        if bar_height == 0. {
            return;
        }

        let size = self.size_info;
        let (width, height) = (size.width(), size.height());
        let (cell_width, cell_height) = (size.cell_width(), size.cell_height());
        let scale_factor = self.window.scale_factor as f32;

        let inset = if config.window.decorations == Decorations::Transparent {
            (TRAFFIC_LIGHTS_INSET * scale_factor).ceil()
        } else {
            0.
        };
        let first_column = (inset / cell_width).ceil() as usize;
        let start_x = first_column as f32 * cell_width;

        let background = config.colors.primary.background;
        let foreground = config.colors.primary.foreground;
        let custom = config.colors.tab_bar;

        // Unset colors are derived from the primary ones; the bar sits slightly lifted.
        let bar_bg = custom.background.unwrap_or_else(|| mix(background, foreground, 0.07));
        let active_bg = custom.active_background.unwrap_or(background);
        let inactive_bg =
            custom.inactive_background.unwrap_or_else(|| mix(bar_bg, foreground, 0.05));
        let active_fg = custom.active_foreground.unwrap_or(foreground);
        let inactive_fg =
            custom.inactive_foreground.unwrap_or_else(|| mix(foreground, background, 0.35));
        let hint_fg = custom.hint_foreground.unwrap_or_else(|| mix(foreground, background, 0.6));
        let separator = custom.separator.unwrap_or_else(|| mix(bar_bg, foreground, 0.18));
        let accent_color = custom.accent.unwrap_or(foreground);
        let hover = self.tab_bar.hover;
        let hover_tab = match hover {
            Some(TabHit::Tab(i) | TabHit::Close(i)) => Some(i),
            _ => None,
        };
        let hover_bg = custom.hover_background.unwrap_or_else(|| mix(inactive_bg, foreground, 0.08));
        let close_bg =
            custom.close_hover_background.unwrap_or_else(|| mix(hover_bg, foreground, 0.18));
        // Background of a tab, lifted while hovered.
        let tab_bg = |i: usize, active: bool| match (active, hover_tab == Some(i)) {
            (true, true) => mix(active_bg, active_fg, 0.04),
            (true, false) => active_bg,
            (false, true) => hover_bg,
            (false, false) => inactive_bg,
        };

        // Layout, in columns.
        let count = self.tab_bar.items.len();
        let columns = (width / cell_width) as usize;
        let available = columns.saturating_sub(first_column + 4);
        let tab_columns = (available / count).clamp(config.tab_bar.min_width(), config.tab_bar.max_width());
        let tab_width = tab_columns as f32 * cell_width;

        // Tab cards: the active one reaches the grid, the others float.
        let card_inset = (bar_height * 0.18).round().max(2.);
        let card_top = card_inset;
        let card_bottom = bar_height - card_inset;
        let accent = (bar_height * 0.07).round().max(2.);
        let separator_height = (bar_height * 0.36).round();
        let separator_y = ((bar_height - separator_height) / 2.).round();

        let mut rects = vec![
            RenderRect::new(0., 0., width, bar_height, bar_bg, 1.),
            RenderRect::new(0., bar_height - 1., width, 1., separator, 0.5),
        ];
        let mut hits = Vec::with_capacity(count + 1);

        for (i, tab) in self.tab_bar.items.iter().enumerate() {
            let x = start_x + (i * tab_columns) as f32 * cell_width;
            let (bg, top, bottom) = if tab.active {
                (tab_bg(i, true), 0., bar_height)
            } else {
                (tab_bg(i, false), card_top, card_bottom)
            };
            rects.push(RenderRect::new(x, top, tab_width, bottom - top, bg, 1.));

            // Highlight the active tab against the grid below.
            if tab.active {
                let accent_y = bar_height - accent;
                rects.push(RenderRect::new(x, accent_y, tab_width, accent, accent_color, 0.9));
            }

            // Close button first, so it wins over the tab itself.
            if config.tab_bar.show_close_button {
                hits.push((x + tab_width - CLOSE_COLUMNS as f32 * cell_width, x + tab_width, TabHit::Close(i)));
            }
            hits.push((x, x + tab_width, TabHit::Tab(i)));
        }

        // Thin separators between adjacent inactive tabs.
        for i in 1..count {
            if self.tab_bar.items[i - 1].active || self.tab_bar.items[i].active {
                continue;
            }
            let x = start_x + (i * tab_columns) as f32 * cell_width;
            rects.push(RenderRect::new(x, separator_y, 1., separator_height, separator, 0.8));
        }

        let plus_column = first_column + count * tab_columns;
        let plus_x = plus_column as f32 * cell_width;
        rects.push(RenderRect::new(plus_x, separator_y, 1., separator_height, separator, 0.8));
        hits.push((plus_x, plus_x + 3. * cell_width, TabHit::New));
        self.tab_bar.hits = hits;
        self.renderer.draw_rects(&size, metrics, rects);

        // Text, drawn in a viewport matching the bar, vertically centered.
        let padding_y = ((bar_height - cell_height) / 2.).floor();
        let bar_size =
            SizeInfo::new(width, bar_height, cell_width, cell_height, 0., padding_y, false);
        unsafe {
            gl::Viewport(
                0,
                (height - bar_height + padding_y) as i32,
                width as i32,
                (bar_height - 2. * padding_y) as i32,
            );
        }

        // The text projection is only refreshed on resize, so match it to the bar viewport.
        self.renderer.set_text_projection(&bar_size);

        for (i, tab) in self.tab_bar.items.iter().enumerate() {
            let bg = tab_bg(i, tab.active);
            let fg = if tab.active || hover_tab == Some(i) { active_fg } else { inactive_fg };

            let hint = if i < 9 { format!("⌘{}", i + 1) } else { String::new() };
            let hint_columns = if hint.is_empty() { 0 } else { hint.chars().count() + 2 };
            // Same space on both sides keeps the title centered: shortcut left, close right.
            let side = hint_columns.max(CLOSE_COLUMNS);
            let title_columns = tab_columns.saturating_sub(2 * side).max(1);

            // Truncate with an ellipsis and center the title in the remaining space.
            let mut title: String = tab.title.trim().chars().take(title_columns).collect();
            if tab.title.trim().chars().count() > title_columns && title_columns > 1 {
                title.pop();
                title.push('…');
            }
            let title_len = title.chars().count();
            let left = (tab_columns / 2).saturating_sub(title_len / 2).max(side);

            let mut label: Vec<char> = vec![' '; tab_columns];
            for (offset, character) in title.chars().enumerate() {
                if left + offset < tab_columns {
                    label[left + offset] = character;
                }
            }

            let column = first_column + i * tab_columns;

            let point = Point::new(0, Column(column));
            self.renderer.draw_string(
                point,
                fg,
                bg,
                label.into_iter(),
                &bar_size,
                &mut self.glyph_cache,
            );

            // Close button, highlighted while hovered.
            let hovering_close = hover == Some(TabHit::Close(i));
            let (close_fg, close_bg) = if hovering_close {
                (active_fg, close_bg)
            } else if hover_tab == Some(i) || tab.active {
                (fg, bg)
            } else {
                (hint_fg, bg)
            };
            if config.tab_bar.show_close_button {
                self.renderer.draw_string(
                    Point::new(0, Column(column + tab_columns - CLOSE_COLUMNS)),
                    close_fg,
                    close_bg,
                    " × ".chars(),
                    &bar_size,
                    &mut self.glyph_cache,
                );
            }

            // Dimmed keyboard shortcut at the left edge.
            if !hint.is_empty() && hint_columns < tab_columns {
                let hint_column = column + 1;
                let point = Point::new(0, Column(hint_column));
                self.renderer.draw_string(
                    point,
                    hint_fg,
                    bg,
                    hint.chars(),
                    &bar_size,
                    &mut self.glyph_cache,
                );
            }
        }

        let point = Point::new(0, Column(plus_column + 1));
        self.renderer.draw_string(
            point,
            inactive_fg,
            bar_bg,
            "+".chars(),
            &bar_size,
            &mut self.glyph_cache,
        );

        self.renderer.resize(&size);
        self.damage_tracker.frame().add_viewport_rect(&size, 0, 0, width as i32, bar_height as i32);
    }
}

/// Blend `from` towards `to` by `amount`, in the 0..=1 range.
pub fn mix(from: Rgb, to: Rgb, amount: f32) -> Rgb {
    let (fr, fg, fb) = from.as_tuple();
    let (tr, tg, tb) = to.as_tuple();
    let blend = |from: u8, to: u8| {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * amount).round() as u8
    };
    Rgb::new(blend(fr, tr), blend(fg, tg), blend(fb, tb))
}
