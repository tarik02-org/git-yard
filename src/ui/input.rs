//! Small pieces of interaction state shared by both UIs.

use std::time::{Duration, Instant};

const TOAST_TTL: Duration = Duration::from_secs(6);

/// A short message shown in the footer until it expires.
#[derive(Debug, Default)]
pub struct Toast {
    message: Option<(Instant, String)>,
}

impl Toast {
    pub fn say(&mut self, message: impl Into<String>) {
        self.message = Some((Instant::now(), message.into()));
    }

    pub fn current(&self) -> Option<&str> {
        self.message
            .as_ref()
            .filter(|(at, _)| at.elapsed() < TOAST_TTL)
            .map(|(_, message)| message.as_str())
    }
}

/// Keeps `cursor` inside a window of `height` rows starting at `offset`,
/// with `padding` rows of margin where the list allows it.
pub fn follow(offset: usize, cursor: usize, height: usize, len: usize, padding: usize) -> usize {
    let height = height.max(1);
    let padding = padding.min(height.saturating_sub(1) / 2);
    let mut offset = offset;
    if cursor < offset + padding {
        offset = cursor.saturating_sub(padding);
    } else if cursor + padding >= offset + height {
        offset = cursor + padding + 1 - height;
    }
    offset.min(len.saturating_sub(height))
}

/// Two clicks on one row within this interval open it.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_keeps_a_margin_and_stays_in_range() {
        assert_eq!(follow(0, 0, 10, 100, 3), 0);
        assert_eq!(follow(0, 8, 10, 100, 3), 2);
        assert_eq!(follow(20, 21, 10, 100, 3), 18);
        assert_eq!(follow(0, 99, 10, 100, 3), 90);
        assert_eq!(follow(5, 2, 10, 5, 3), 0);
    }
}
