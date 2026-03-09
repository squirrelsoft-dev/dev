pub mod picker;
pub mod prompts;

/// Terminal dimensions for constraining interactive lists.
pub struct TermDimensions {
    pub max_length: usize,
    pub max_width: usize,
}

/// Returns terminal dimensions for constraining interactive lists.
/// `max_length` = rows minus overhead (prompt, input, margins), min 5.
/// `max_width` = columns minus widest prefix (`> [x] ` = 6) plus margin, min 20.
pub fn term_dimensions() -> TermDimensions {
    let (rows, cols) = console::Term::stdout()
        .size_checked()
        .map(|(h, w)| (h as usize, w as usize))
        .unwrap_or((19, 80));
    TermDimensions {
        max_length: rows.saturating_sub(4).max(5),
        max_width: cols.saturating_sub(8).max(20), // 8 = 6 for "> [x] " + 2 margin
    }
}

/// Truncate a display string to fit within terminal width (avoids line wrapping).
/// Counts chars (correct for ASCII content from OCI refs/descriptions).
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_width {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_width.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
