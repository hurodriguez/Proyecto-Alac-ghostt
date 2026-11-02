//! Tab bar layout configuration, colors live in `colors.tab_bar`.

use serde::Serialize;

use alac_config_derive::ConfigDeserialize;

#[derive(ConfigDeserialize, Serialize, Copy, Clone, Debug, PartialEq, Eq)]
pub struct TabBar {
    /// Narrowest a tab gets, in columns.
    min_width: usize,
    /// Widest a tab gets, in columns.
    max_width: usize,
    /// Draw the button closing a tab.
    pub show_close_button: bool,
}

impl Default for TabBar {
    fn default() -> Self {
        Self { min_width: 8, max_width: 28, show_close_button: true }
    }
}

impl TabBar {
    pub fn min_width(&self) -> usize {
        self.min_width.clamp(4, 60)
    }

    pub fn max_width(&self) -> usize {
        self.max_width.clamp(self.min_width(), 80)
    }
}
