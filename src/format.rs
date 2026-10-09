use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

use crate::model::Integration;

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

pub fn age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        "now".into()
    } else if hours < 1 {
        format!("{minutes}m")
    } else if days < 1 {
        format!("{hours}h")
    } else if days < 14 {
        format!("{days}d")
    } else if days < 60 {
        format!("{}w", days / 7)
    } else if days < 730 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// `age` phrased as a time in the past.
pub fn ago(seconds: i64) -> String {
    match age(seconds) {
        text if text == "now" => "just now".into(),
        text => format!("{text} ago"),
    }
}

pub fn integration(integration: &Integration) -> String {
    match integration {
        Integration::IsBase => "base branch".into(),
        Integration::Merged { into } => format!("merged into {into}"),
        Integration::ChangesPresent { into } => {
            format!("changes present in {into} (squash/rebase)")
        }
        Integration::NotIntegrated { base, ahead } => format!("{ahead} commits not in {base}"),
        Integration::NoBase => "no base branch".into(),
        Integration::Unborn => "no commits".into(),
    }
}

pub fn truncate_end(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return text.to_owned();
    }
    let (start, _) = text.unicode_truncate(width - 1);
    format!("{start}…")
}

/// Keeps the start and, preferably, the end: `~/work/…/project.branch`.
pub fn truncate_middle(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width < 2 {
        return truncate_end(text, width);
    }
    let head = (width - 1) / 3;
    let (start, start_width) = text.unicode_truncate(head);
    let (end, _) = text.unicode_truncate_start(width - 1 - start_width);
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes_and_ages() {
        assert_eq!(size(512), "512B");
        assert_eq!(size(1536), "1.5K");
        assert_eq!(size(300 * 1024 * 1024), "300M");
        assert_eq!(age(30), "now");
        assert_eq!(age(3 * 86_400), "3d");
        assert_eq!(age(90 * 86_400), "3mo");
    }
}
