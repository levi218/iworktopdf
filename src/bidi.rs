//! A reduced Unicode bidi algorithm (UAX #9) for one line: strong right-to-left letters (Hebrew,
//! Arabic) and strong left-to-right letters and digits get levels around the paragraph level,
//! neutrals take the direction of matching neighbours (else the paragraph's), and runs are reversed
//! from the highest level down. Right-to-left runs are drawn reversed with mirrored brackets.
//! Contextual shaping (Arabic joining) is not attempted.

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
    let characters: Vec<(char, usize)> = pieces
        .iter()
        .enumerate()
        .flat_map(|(piece, (text, _))| text.chars().map(move |character| (character, piece)))
        .collect();
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
