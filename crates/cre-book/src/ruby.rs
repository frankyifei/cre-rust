//! `<ruby>` markup rewriting (Blitz has no ruby box).

use crate::html::{find_matching_close_tag, find_tag_end, next_tag_start, tag_name};

/// Points the `<image>` hrefs of inline SVG blocks at `epub://book/...`.
///
/// The parse-time inliner then needs no document directory: chunks can hold
/// documents from several directories.
/// Turns `<ruby>base<rt>ann</rt></ruby>` into two stacked, centred inline
/// blocks.
///
/// Blitz has no ruby layout: it lays the annotation out as ordinary inline text
/// after the base character, so the note sits to the right of its character and
/// the line keeps the annotation's own advance. A ruby cell is a small vertical
/// stack instead - annotation above the base, both centred, the cell as wide as
/// the wider of the two - which is how crengine and browsers draw it.
/// Turns `<ruby>` markup into the stacked cell Blitz can lay out.
///
/// Blitz has no ruby box, so each base + annotation pair becomes an
/// inline-block with the note absolutely positioned above the base (see
/// [`ruby_cell_markup`]). Exposed so every format that can carry ruby (EPUB,
/// KF8, MOBI, TXT) shares the same conversion.
pub fn rewrite_ruby(html: &str) -> String {
    if !html
        .as_bytes()
        .windows(5)
        .any(|window| window.eq_ignore_ascii_case(b"<ruby"))
    {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + html.len() / 16);
    let mut cursor = 0usize;
    while let Some(open_start) = next_tag_start(html, cursor, "<ruby") {
        let Some((close_start, close_end)) =
            find_matching_close_tag(html, open_start, "<ruby", "</ruby")
        else {
            break;
        };
        out.push_str(&html[cursor..open_start]);
        let inner = &html[find_tag_end(html, open_start).unwrap_or(close_start)..close_start];
        match ruby_cell(inner) {
            Some(cell) => out.push_str(&cell),
            None => out.push_str(&html[open_start..close_end]),
        }
        cursor = close_end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// The two stacked blocks of one ruby cell, or `None` when the markup is not a
/// plain base + annotation pair.
pub fn ruby_cell(inner: &str) -> Option<String> {
    let lower = inner.to_ascii_lowercase();
    let rt_start = next_tag_start(&lower, 0, "<rt")?;
    let rt_body_start = find_tag_end(inner, rt_start)?;
    let rt_end = next_tag_start(&lower, rt_body_start, "</rt")?;
    // Only the first pair is handled; anything else (multiple annotations,
    // nested markup we do not understand) is left to the normal inline path.
    if next_tag_start(&lower, rt_end + "</rt".len(), "<rt").is_some() {
        return None;
    }
    let base = strip_ruby_wrappers(&inner[..rt_start]);
    let annotation = strip_ruby_wrappers(&inner[rt_body_start..rt_end]);
    if base.trim().is_empty() || annotation.trim().is_empty() {
        return None;
    }
    // The cell keeps the width of the annotated characters, the note is centred
    // above them and squeezed to that width whatever its length, and a zhuyin
    // tone mark sits on a row of its own above the letters, flush right over its
    // character. That is how Chrome typesets ruby, and it is also the only way
    // the characters keep their normal rhythm: sizing the cell to the note
    // instead pushes every character on the line apart.
    Some(ruby_cell_markup(&base, &annotation))
}

/// One ruby cell: the base in flow, the note absolutely positioned above it.
///
/// The base comes first so the inline-block reports its baseline (a block that
/// follows the note would make the character drop below the surrounding text),
/// and the note lives in a box exactly as wide as the base, which is what makes
/// the centring and the flush-right tone mark line up with the character rather
/// than with the cell (the cell also holds the book's letter-spacing).
pub fn ruby_cell_markup(base: &str, annotation: &str) -> String {
    let (tones, letters) = if is_zhuyin(annotation) {
        split_tone_marks(annotation)
    } else {
        (String::new(), annotation.trim().to_string())
    };
    let has_tones = !tones.trim().is_empty();
    let has_letters = !letters.trim().is_empty();
    let base_em = ruby_em(base).max(0.5);
    // The note is drawn at half size; squeeze it onto the character when it is
    // wider, so a three- or four-letter note still fits over one ideograph.
    let letters_em = ruby_em(&letters) * 0.5;
    let scale = if letters_em > base_em * 1.02 {
        (base_em / letters_em).clamp(0.35, 1.0)
    } else {
        1.0
    };
    // The mark is top-anchored and the letters bottom-anchored, so the note band
    // is only a little taller than one half-size row: a full extra row left a
    // visible gap between the mark and the letters under it.
    let pad = if has_tones { "0.82em" } else { "0.5em" };
    let mut note = String::new();
    if has_tones {
        note.push_str(&format!(
            r#"<span style="position:absolute;right:0;top:0;white-space:nowrap;font-size:50%;line-height:1;letter-spacing:normal;">{tones}</span>"#
        ));
    }
    if has_letters {
        let letters = if scale < 0.995 {
            format!(
                r#"<span style="display:inline-block;transform:scaleX({scale:.3});transform-origin:center;">{letters}</span>"#
            )
        } else {
            letters
        };
        note.push_str(&format!(
            r#"<span style="position:absolute;left:50%;bottom:0;transform:translateX(-50%);white-space:nowrap;font-size:50%;line-height:1;letter-spacing:normal;">{letters}</span>"#
        ));
    }
    // Reserve the rows the note occupies, so the line box grows like it did when
    // the note was laid out in flow.
    format!(
        r#"<span style="display:inline-block;position:relative;vertical-align:baseline;padding-top:{pad};"><span style="display:block;white-space:nowrap;line-height:1.15;">{base}</span><span style="position:absolute;left:0;top:0;width:{base_em:.3}em;height:{pad};">{note}</span></span>"#
    )
}

/// True when the annotation uses bopomofo (zhuyin) letters.
pub fn is_zhuyin(text: &str) -> bool {
    text.chars()
        .any(|ch| matches!(ch as u32, 0x3100..=0x312F | 0x31A0..=0x31BF))
}

/// Splits the tone marks of a zhuyin annotation from its letters.
///
/// The tone letters are the spacing modifier letters `ˉ ˊ ˇ ˋ ˙`; a book that
/// stores the tone as a combining mark gets that mark moved up as well.
pub fn split_tone_marks(annotation: &str) -> (String, String) {
    let mut tones = String::new();
    let mut letters = String::new();
    for ch in annotation.chars() {
        let code = ch as u32;
        let is_tone = matches!(
            code,
            0x02C7 | 0x02C9 | 0x02CA..=0x02CB | 0x02D9 | 0x0300..=0x036F | 0xFE20..=0xFE2F
        );
        if is_tone {
            tones.push(ch);
        } else if !ch.is_whitespace() {
            letters.push(ch);
        }
    }
    (tones, letters)
}

/// Approximate advance of `text` in em, for squeezing a ruby note onto its base.
///
/// Full-width characters count one em, Latin and other narrow ones half; the
/// result only has to be close enough to fit the note into the character.
pub fn ruby_em(text: &str) -> f32 {
    text.chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '<' && *ch != '>')
        .map(|ch| {
            let code = ch as u32;
            match code {
                0x0300..=0x036F | 0x02C7 | 0x02C9 | 0x02CA..=0x02CB | 0x02D9 | 0xFE20..=0xFE2F => {
                    0.0
                }
                0x2E80..=0xF9FF | 0xFF00..=0xFF60 => 1.0,
                _ => 0.5,
            }
        })
        .sum()
}

/// Drops `<rb>`/`<rp>` wrappers and their closing tags, keeping the text.
pub fn strip_ruby_wrappers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(open) = text[cursor..].find('<').map(|offset| cursor + offset) {
        out.push_str(&text[cursor..open]);
        let Some(end) = find_tag_end(text, open) else {
            return out;
        };
        let name = tag_name(&text[open..end]);
        if !matches!(name.as_str(), "rb" | "rp" | "rt") {
            out.push_str(&text[open..end]);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}
