use cairo::{Context, FontFace};
use unicode_segmentation::UnicodeSegmentation;

pub struct LabelFonts<'a> {
    pub primary: &'a FontFace,
    pub emoji: Option<&'a FontFace>,
}

fn apply_font(c: &Context, fonts: &LabelFonts<'_>, use_emoji: bool, font_size: f64) {
    if use_emoji {
        if let Some(face) = fonts.emoji {
            c.set_font_face(face);
            c.set_font_size(font_size);
            return;
        }
    }
    c.set_font_face(fonts.primary);
    c.set_font_size(font_size);
}

fn run_width(c: &Context, text: &str, fonts: &LabelFonts<'_>, use_emoji: bool, font_size: f64) -> f64 {
    apply_font(c, fonts, use_emoji, font_size);
    c.text_extents(text).unwrap().width()
}

/// Split label text into consecutive emoji vs non-emoji grapheme runs.
pub fn text_runs(text: &str) -> Vec<(bool, String)> {
    let mut runs: Vec<(bool, String)> = Vec::new();
    for grapheme in text.graphemes(true) {
        let is_emoji = grapheme_needs_emoji_font(grapheme);
        if let Some((last_is_emoji, buf)) = runs.last_mut() {
            if *last_is_emoji == is_emoji {
                buf.push_str(grapheme);
                continue;
            }
        }
        runs.push((is_emoji, grapheme.to_string()));
    }
    runs
}

pub fn grapheme_needs_emoji_font(grapheme: &str) -> bool {
    grapheme.chars().any(is_emoji_codepoint)
}

fn is_emoji_codepoint(ch: char) -> bool {
    let cp = ch as u32;
    if (0x1F1E6..=0x1F1FF).contains(&cp) {
        return true;
    }
    if (0x1F300..=0x1FAFF).contains(&cp) || (0x2600..=0x27BF).contains(&cp) {
        return true;
    }
    matches!(
        cp,
        0x00A9
            | 0x00AE
            | 0x203C
            | 0x2049
            | 0x2122
            | 0x2139
            | 0x2194
            | 0x2195
            | 0x2196
            | 0x2197
            | 0x2198
            | 0x2199
            | 0x21A9
            | 0x21AA
            | 0x231A
            | 0x231B
            | 0x2328
            | 0x23CF
            | 0x23E9
            | 0x23EA
            | 0x23EB
            | 0x23EC
            | 0x23ED
            | 0x23EE
            | 0x23EF
            | 0x23F0
            | 0x23F1
            | 0x23F2
            | 0x23F3
            | 0x23F8
            | 0x23F9
            | 0x23FA
            | 0x24C2
            | 0x25AA
            | 0x25AB
            | 0x25B6
            | 0x25C0
            | 0x25FB
            | 0x25FC
            | 0x25FD
            | 0x25FE
            | 0x260E
            | 0x2611
            | 0x2614
            | 0x2615
            | 0x2618
            | 0x261D
            | 0x2620
            | 0x2622
            | 0x2623
            | 0x2626
            | 0x262A
            | 0x262E
            | 0x262F
            | 0x2638
            | 0x2639
            | 0x263A
            | 0x2640
            | 0x2642
            | 0x2648
            | 0x2649
            | 0x264A
            | 0x264B
            | 0x264C
            | 0x264D
            | 0x264E
            | 0x264F
            | 0x2650
            | 0x2651
            | 0x2652
            | 0x2653
            | 0x265F
            | 0x2660
            | 0x2663
            | 0x2665
            | 0x2666
            | 0x2668
            | 0x267B
            | 0x267E
            | 0x267F
            | 0x2692
            | 0x2693
            | 0x2694
            | 0x2695
            | 0x2696
            | 0x2697
            | 0x2699
            | 0x269B
            | 0x269C
            | 0x26A0
            | 0x26A1
            | 0x26A7
            | 0x26AA
            | 0x26AB
            | 0x26B0
            | 0x26B1
            | 0x26BD
            | 0x26BE
            | 0x26C4
            | 0x26C5
            | 0x26C8
            | 0x26CE
            | 0x26CF
            | 0x26D1
            | 0x26D3
            | 0x26D4
            | 0x26E9
            | 0x26EA
            | 0x26F0
            | 0x26F1
            | 0x26F2
            | 0x26F3
            | 0x26F4
            | 0x26F5
            | 0x26F7
            | 0x26F8
            | 0x26F9
            | 0x26FA
            | 0x26FD
            | 0x2702
            | 0x2705
            | 0x2708
            | 0x2709
            | 0x270A
            | 0x270B
            | 0x270C
            | 0x270D
            | 0x270F
            | 0x2712
            | 0x2714
            | 0x2716
            | 0x271D
            | 0x2721
            | 0x2728
            | 0x2733
            | 0x2734
            | 0x2744
            | 0x2747
            | 0x274C
            | 0x274E
            | 0x2753
            | 0x2754
            | 0x2755
            | 0x2757
            | 0x2763
            | 0x2764
            | 0x2795
            | 0x2796
            | 0x2797
            | 0x27A1
            | 0x27B0
            | 0x27BF
            | 0x2934
            | 0x2935
            | 0x2B05
            | 0x2B06
            | 0x2B07
            | 0x2B1B
            | 0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x3030
            | 0x303D
            | 0x3297
            | 0x3299
            | 0xFE0F
    ) || (0x10000..=0x10FFFF).contains(&cp)
}

fn label_ink_metrics(
    c: &Context,
    runs: &[(bool, String)],
    fonts: &LabelFonts<'_>,
    font_size: f64,
) -> (f64, f64, f64) {
    let mut total_width = 0.0_f64;
    let mut y_bearing_min = 0.0_f64;
    let mut ink_bottom = 0.0_f64;
    for (is_emoji, run) in runs {
        apply_font(c, fonts, *is_emoji, font_size);
        let e = c.text_extents(run).unwrap();
        total_width += e.width();
        y_bearing_min = y_bearing_min.min(e.y_bearing());
        ink_bottom = ink_bottom.max(e.y_bearing() + e.height());
    }
    (total_width, y_bearing_min, ink_bottom - y_bearing_min)
}

/// Draw `text` centered horizontally in the button and vertically between `label_top` and `label_bottom`.
pub fn show_label_centered(
    c: &Context,
    text: &str,
    center_x: f64,
    label_top: f64,
    label_bottom: f64,
    font_size: f64,
    fonts: &LabelFonts<'_>,
) {
    let runs = text_runs(text);
    if runs.is_empty() {
        return;
    }

    let (total_width, y_bearing_min, ink_height) = label_ink_metrics(c, &runs, fonts, font_size);
    let center_y = (label_top + label_bottom) / 2.0;
    let baseline = center_y - (y_bearing_min + ink_height / 2.0);
    let mut x = (center_x - total_width / 2.0).round();

    for (is_emoji, run) in runs {
        apply_font(c, fonts, is_emoji, font_size);
        c.move_to(x, baseline);
        if c.show_text(&run).is_err() {
            apply_font(c, fonts, false, font_size);
            c.move_to(x, baseline);
            let _ = c.show_text(&run);
        }
        x += run_width(c, &run, fonts, is_emoji, font_size);
    }
}
