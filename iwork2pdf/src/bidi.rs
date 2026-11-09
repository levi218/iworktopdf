//! A reduced Unicode bidi algorithm (UAX #9) for one line: strong right-to-left letters (Hebrew,
//! Arabic) and strong left-to-right letters and digits get levels around the paragraph level,
//! neutrals take the direction of matching neighbours (else the paragraph's), and runs are reversed
//! from the highest level down. Right-to-left runs are drawn reversed with mirrored brackets.
//! Arabic letters are first joined into their presentation forms (U+FE70-FEFF), in logical order.

/// A character's bidi class, reduced to three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    LeftToRight,
    RightToLeft,
    Neutral,
}

fn direction_of(character: char) -> Direction {
    let right_to_left = ('\u{0590}'..='\u{08FF}').contains(&character)
        || ('\u{FB1D}'..='\u{FDFF}').contains(&character)
        || ('\u{FE70}'..='\u{FEFF}').contains(&character);
    if right_to_left {
        Direction::RightToLeft
    } else if character.is_alphanumeric() {
        Direction::LeftToRight
    } else {
        Direction::Neutral
    }
}

/// True when the string holds a right-to-left letter.
pub fn has_rtl(text: &str) -> bool {
    text.chars()
        .any(|character| direction_of(character) == Direction::RightToLeft)
}

/// True when the first strong (directional) character of the texts is right-to-left.
pub fn first_strong_rtl<'a>(texts: impl Iterator<Item = &'a str>) -> bool {
    texts
        .flat_map(str::chars)
        .map(direction_of)
        .find(|direction| *direction != Direction::Neutral)
        == Some(Direction::RightToLeft)
}

fn mirror(character: char) -> char {
    match character {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        other => other,
    }
}

/// Arabic letters U+0621-064A with their first presentation form (isolated, then final, initial,
/// medial) and whether they join on both sides (else only to the preceding letter); `None` for
/// the unassigned U+063B-0640.
const ARABIC_FORMS: [Option<(u16, bool)>; 42] = [
    Some((0xFE80, false)), // hamza (joins neither side; its single form)
    Some((0xFE81, false)),
    Some((0xFE83, false)),
    Some((0xFE85, false)),
    Some((0xFE87, false)),
    Some((0xFE89, true)),
    Some((0xFE8D, false)), // alef
    Some((0xFE8F, true)),
    Some((0xFE93, false)), // teh marbuta
    Some((0xFE95, true)),
    Some((0xFE99, true)),
    Some((0xFE9D, true)),
    Some((0xFEA1, true)),
    Some((0xFEA5, true)),
    Some((0xFEA9, false)), // dal
    Some((0xFEAB, false)),
    Some((0xFEAD, false)), // reh
    Some((0xFEAF, false)),
    Some((0xFEB1, true)),
    Some((0xFEB5, true)),
    Some((0xFEB9, true)),
    Some((0xFEBD, true)),
    Some((0xFEC1, true)),
    Some((0xFEC5, true)),
    Some((0xFEC9, true)),
    Some((0xFECD, true)),
    None,
    None,
    None,
    None,
    None,
    None, // U+0640 tatweel (handled as a joiner)
    Some((0xFED1, true)),
    Some((0xFED5, true)),
    Some((0xFED9, true)),
    Some((0xFEDD, true)), // lam
    Some((0xFEE1, true)),
    Some((0xFEE5, true)),
    Some((0xFEE9, true)),
    Some((0xFEED, false)), // waw
    Some((0xFEEF, false)),
    Some((0xFEF1, true)),
];

/// A letter's presentation forms (first form, joins both sides), if it is a joining letter.
fn arabic_forms(character: char) -> Option<(u16, bool)> {
    let index = u32::from(character).checked_sub(0x0621)? as usize;
    ARABIC_FORMS.get(index).copied().flatten()
}

/// Harakat and other combining marks are transparent to joining.
fn is_transparent(character: char) -> bool {
    matches!(u32::from(character), 0x064B..=0x065F | 0x0670)
}

/// Whether the letter joins the one after it (tatweel always does).
fn joins_next(character: char) -> bool {
    character == '\u{0640}' || arabic_forms(character).is_some_and(|(_, dual)| dual)
}

/// Whether the letter can join the one before it.
fn joins_previous(character: char) -> bool {
    character == '\u{0640}' || arabic_forms(character).is_some_and(|(first, _)| first != 0xFE80)
}

/// The lam-alef ligature's isolated form for an alef after lam (its final form is one above).
fn lam_alef(alef: char) -> Option<u16> {
    match alef {
        '\u{0622}' => Some(0xFEF5),
        '\u{0623}' => Some(0xFEF7),
        '\u{0625}' => Some(0xFEF9),
        '\u{0627}' => Some(0xFEFB),
        _ => None,
    }
}

/// Replaces Arabic letters with their contextual presentation forms (and lam + alef with their
/// ligature, dropping the alef), so a font's cmap draws them joined one glyph per character.
fn join_arabic<T: Copy>(characters: &mut Vec<(char, T)>) {
    if !characters
        .iter()
        .any(|(character, _)| arabic_forms(*character).is_some())
    {
        return;
    }
    // The nearest non-transparent neighbour on each side.
    let neighbour = |characters: &[(char, T)], index: usize, forward: bool| {
        let mut position = index;
        loop {
            position = if forward {
                position
                    .checked_add(1)
                    .filter(|&next| next < characters.len())?
            } else {
                position.checked_sub(1)?
            };
            let character = characters[position].0;
            if !is_transparent(character) {
                return Some((position, character));
            }
        }
    };
    let mut joined = Vec::with_capacity(characters.len());
    let mut skip = None;
    for index in 0..characters.len() {
        if skip == Some(index) {
            continue;
        }
        let (character, tag) = characters[index];
        let Some((first, dual)) = arabic_forms(character) else {
            joined.push((character, tag));
            continue;
        };
        let previous = neighbour(characters, index, false)
            .is_some_and(|(_, before)| joins_next(before) && joins_previous(character));
        let next_letter = neighbour(characters, index, true);
        if character == '\u{0644}' {
            if let Some((alef_index, ligature)) = next_letter
                .and_then(|(position, after)| lam_alef(after).map(|form| (position, form)))
            {
                let form = ligature + u16::from(previous);
                joined.push((char::from_u32(u32::from(form)).unwrap_or(character), tag));
                skip = Some(alef_index);
                continue;
            }
        }
        let next = dual && next_letter.is_some_and(|(_, after)| joins_previous(after));
        // Forms: isolated, final, initial, medial (right-joining letters: isolated, final).
        let offset = match (previous, next) {
            (false, false) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (true, true) => 3,
        };
        let form = if first == 0xFE80 {
            first
        } else {
            first + offset
        };
        joined.push((char::from_u32(u32::from(form)).unwrap_or(character), tag));
    }
    *characters = joined;
}

/// A stretch of one piece's characters at one embedding level: [start, end) and the level.
#[derive(Debug, Clone, Copy)]
struct Segment {
    start: usize,
    end: usize,
    level: u8,
}

/// Visual-order segments of the logical pieces of one line.
///
/// # Arguments
///
/// * `pieces` - The line's (text, tag) pieces in logical order; tags identify the piece
/// * `rtl` - The paragraph is right-to-left
///
/// # Returns
///
/// (text, tag) segments in visual order; right-to-left segments are reversed and mirrored.
///
/// # Examples
///
/// ```
/// let out = iwork2pdf::bidi::reorder(&[("abc ".to_string(), 0), ("אב".to_string(), 1)], false);
/// assert_eq!(out, vec![("abc ".to_string(), 0), ("בא".to_string(), 1)]);
/// ```
pub fn reorder<Tag: Copy>(pieces: &[(String, Tag)], rtl: bool) -> Vec<(String, Tag)> {
    // Each character with the index of the piece it came from.
    let mut characters: Vec<(char, usize)> = pieces
        .iter()
        .enumerate()
        .flat_map(|(piece, (text, _))| text.chars().map(move |character| (character, piece)))
        .collect();
    join_arabic(&mut characters);
    let count = characters.len();
    let mut directions: Vec<Direction> = characters
        .iter()
        .map(|(character, _)| direction_of(*character))
        .collect();
    let paragraph = if rtl {
        Direction::RightToLeft
    } else {
        Direction::LeftToRight
    };
    // Neutrals between two strong characters of one direction take it; others the paragraph's.
    let mut start = 0;
    while start < count {
        if directions[start] != Direction::Neutral {
            start += 1;
            continue;
        }
        let end = (start..count)
            .find(|&next| directions[next] != Direction::Neutral)
            .unwrap_or(count);
        let before = if start > 0 {
            directions[start - 1]
        } else {
            paragraph
        };
        let after = if end < count {
            directions[end]
        } else {
            paragraph
        };
        let resolved = if before == after { before } else { paragraph };
        directions[start..end].fill(resolved);
        start = end;
    }
    let levels: Vec<u8> = directions
        .iter()
        .map(|&direction| match (rtl, direction) {
            (true, Direction::LeftToRight) => 2,
            (true, _) => 1,
            (false, Direction::RightToLeft) => 1,
            (false, _) => 0,
        })
        .collect();
    // Segments: maximal stretches of one piece and one level.
    let mut segments: Vec<Segment> = Vec::new();
    let mut start = 0;
    while start < count {
        let same = |other: usize| {
            characters[other].1 == characters[start].1 && levels[other] == levels[start]
        };
        let end = (start + 1..count)
            .find(|&other| !same(other))
            .unwrap_or(count);
        segments.push(Segment {
            start,
            end,
            level: levels[start],
        });
        start = end;
    }
    // Reverse every run at or above each level, from the highest down to the lowest odd one.
    let highest = segments
        .iter()
        .map(|segment| segment.level)
        .max()
        .unwrap_or(0);
    for level in (1..=highest).rev() {
        let mut first = 0;
        while first < segments.len() {
            if segments[first].level < level {
                first += 1;
                continue;
            }
            let run_end = (first..segments.len())
                .find(|&other| segments[other].level < level)
                .unwrap_or(segments.len());
            segments[first..run_end].reverse();
            first = run_end;
        }
    }
    segments
        .iter()
        .map(|segment| {
            let span = &characters[segment.start..segment.end];
            let text: String = if segment.level % 2 == 1 {
                span.iter()
                    .rev()
                    .map(|(character, _)| mirror(*character))
                    .collect()
            } else {
                span.iter().map(|(character, _)| *character).collect()
            };
            (text, pieces[characters[segment.start].1].1)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_arabic_letters() {
        // "العربية": alef, lam, ain, reh, beh, yeh, teh marbuta.
        let mut characters: Vec<(char, ())> = "العربية".chars().map(|c| (c, ())).collect();
        join_arabic(&mut characters);
        let forms: String = characters.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            forms,
            "\u{FE8D}\u{FEDF}\u{FECC}\u{FEAE}\u{FE91}\u{FEF4}\u{FE94}"
        );
        // Lam + alef become one ligature.
        let mut ligature: Vec<(char, ())> = "لا".chars().map(|c| (c, ())).collect();
        join_arabic(&mut ligature);
        assert_eq!(ligature, vec![('\u{FEFB}', ())]);
    }

    #[test]
    fn detects_right_to_left() {
        assert!(has_rtl("abc שלום"));
        assert!(!has_rtl("abc"));
        assert!(first_strong_rtl([" ", "שלום abc"].into_iter()));
        assert!(!first_strong_rtl(["1 abc", "שלום"].into_iter()));
    }

    #[test]
    fn right_to_left_paragraph_reverses_and_mirrors() {
        let out = reorder(&[("(אב)".to_string(), 7)], true);
        assert_eq!(out, vec![("(בא)".to_string(), 7)]);
    }

    #[test]
    fn left_to_right_text_is_unchanged() {
        let pieces = [("one ".to_string(), 0), ("two".to_string(), 1)];
        assert_eq!(reorder(&pieces, false), pieces.to_vec());
    }
}
