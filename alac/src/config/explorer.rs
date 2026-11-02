//! File explorer panel configuration.

use serde::Serialize;

use alac_config_derive::ConfigDeserialize;

/// Largest value accepted for [`Explorer::max_entries`], so huge folders cannot grow memory.
const MAX_ENTRIES_LIMIT: usize = 5000;

/// Widest panel accepted, in columns.
const MAX_WIDTH_LIMIT: usize = 120;

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq)]
pub struct Explorer {
    /// Panel width in columns.
    width: usize,
    /// Largest share of the window the panel may take.
    max_width_share: f32,
    /// List entries starting with a dot.
    pub show_hidden: bool,
    /// Entries kept per directory.
    max_entries: usize,
    /// Directory refresh interval in milliseconds, `0` follows only the terminal's directory.
    refresh_ms: u64,
    /// Names that are never listed.
    pub ignore: Vec<String>,
    /// Command used to open files, the file path is passed as `$1`. Empty uses `$EDITOR`.
    pub editor: String,
}

impl Default for Explorer {
    fn default() -> Self {
        Self {
            width: 28,
            max_width_share: 0.4,
            show_hidden: false,
            max_entries: 1000,
            refresh_ms: 1000,
            ignore: Vec::new(),
            editor: String::new(),
        }
    }
}

impl Explorer {
    pub fn width(&self) -> usize {
        self.width.clamp(8, MAX_WIDTH_LIMIT)
    }

    pub fn max_width_share(&self) -> f32 {
        self.max_width_share.clamp(0.1, 0.9)
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries.clamp(1, MAX_ENTRIES_LIMIT)
    }

    /// Refresh interval, `None` when polling is disabled.
    pub fn refresh_interval(&self) -> Option<std::time::Duration> {
        (self.refresh_ms > 0).then(|| std::time::Duration::from_millis(self.refresh_ms.max(100)))
    }
}
