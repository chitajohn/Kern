//! Kern CLI branding: logo and version display.

/// Returns the Kern logo as a multi-line string.
///
/// Uses ANSI 24-bit color for the teal face and black visor/eyes when
/// color is enabled (respects `NO_COLOR`).
pub fn logo() -> String {
    let color = std::env::var("NO_COLOR").is_err();
    let (t, b, r) = if color {
        ("\x1b[38;2;64;224;208m", "\x1b[38;2;0;0;0m", "\x1b[0m")
    } else {
        ("", "", "")
    };

    let teal = "\u{2588}"; // █
    let black = if color { "\u{2588}" } else { "\u{2593}" }; // █ or ▓

    // The Kern logo: a pixelated face, 10 blocks wide, 11 rows tall.
    //
    //        ████          Row 1:  4 teal (centered)
    //      ██████          Row 2:  6 teal
    //     ████████         Row 3:  8 teal
    //    ██████████        Row 4: 10 teal (full width)
    //    ██▓▓▓▓▓▓██        Row 5: visor top (6 black blocks in center)
    //    ██▓▓█▓█▓██        Row 6: visor with eyes (1 black, eye, gap, eye, 1 black)
    //    ██▓▓▓▓▓▓██        Row 7: visor bottom
    //     ████████         Row 8:  8 teal
    //      ██████          Row 9:  6 teal
    //      ██    ██        Row 10: feet
    //      ██    ██        Row 11: feet

    let rows = [
        format!("      {t}{teal}{teal}{teal}{teal}{r}"),
        format!("     {t}{teal}{teal}{teal}{teal}{teal}{teal}{r}"),
        format!("    {t}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{r}"),
        format!("   {t}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{r}"),
        format!("   {t}{teal}{teal}{r}{b}{black}{black}{black}{black}{black}{black}{r}{t}{teal}{teal}{r}"),
        format!(
            "   {t}{teal}{teal}{r}{b}{black}{r}{t}{teal}{r}{b}{black}{black}{r}{t}{teal}{r}{b}{black}{r}{t}{teal}{teal}{r}"
        ),
        format!("   {t}{teal}{teal}{r}{b}{black}{black}{black}{black}{black}{black}{r}{t}{teal}{teal}{r}"),
        format!("    {t}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{teal}{r}"),
        format!("     {t}{teal}{teal}{teal}{teal}{teal}{teal}{r}"),
        format!("     {t}{teal}{teal}{r}      {t}{teal}{teal}{r}"),
        format!("     {t}{teal}{teal}{r}      {t}{teal}{teal}{r}"),
    ];

    rows.join("\n")
}

/// Returns the branded banner for help output.
pub fn banner() -> String {
    let logo = logo();
    let version = kern_core::version::KERN_VERSION;
    format!("{logo}\nKern {version}\n")
}
