//! Name patterns for "Select items matching…": a small glob dialect matched
//! against a whole display name, case-insensitively.
//!
//! `*` is any run of characters (also none), `?` is exactly one character,
//! `[abc]` / `[a-c]` is one of the listed characters and `[!abc]` is one that
//! is not listed. Every other character, regex metacharacters such as `.`,
//! `+` or `(` included, stands for itself. A `[` without a closing `]`
//! stands for itself. There is no escape character.

/// Whether `name` matches the whole of `pattern`. Both sides are compared
/// in Unicode lowercase. An empty pattern matches nothing.
pub fn matches(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let tokens = parse(&pattern.to_lowercase());
    let name: Vec<char> = name.to_lowercase().chars().collect();
    run(&tokens, &name)
}

enum Token {
    Star,
    Any,
    Literal(char),
    Class {
        negated: bool,
        singles: Vec<char>,
        ranges: Vec<(char, char)>,
    },
}

impl Token {
    fn accepts(&self, ch: char) -> bool {
        match self {
            Self::Star => true,
            Self::Any => true,
            Self::Literal(literal) => *literal == ch,
            Self::Class {
                negated,
                singles,
                ranges,
            } => {
                let hit = singles.contains(&ch)
                    || ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&ch));
                hit != *negated
            }
        }
    }
}

/// Reads the class that opens at `chars[at]` (a `[`): the token and the
/// index after its `]`, or `None` when it is never closed.
fn parse_class(chars: &[char], at: usize) -> Option<(Token, usize)> {
    let mut i = at + 1;
    let negated = chars.get(i) == Some(&'!');
    if negated {
        i += 1;
    }
    let first = i;
    let mut singles = Vec::new();
    let mut ranges = Vec::new();
    while let Some(&ch) = chars.get(i) {
        // A `]` right after the opening is a member, not the end.
        if ch == ']' && i > first {
            return Some((
                Token::Class {
                    negated,
                    singles,
                    ranges,
                },
                i + 1,
            ));
        }
        match (chars.get(i + 1), chars.get(i + 2)) {
            (Some('-'), Some(&hi)) if hi != ']' => {
                ranges.push((ch, hi));
                i += 3;
            }
            _ => {
                singles.push(ch);
                i += 1;
            }
        }
    }
    None
}

fn parse(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while let Some(&ch) = chars.get(i) {
        i += 1;
        tokens.push(match ch {
            '*' => Token::Star,
            '?' => Token::Any,
            '[' => match parse_class(&chars, i - 1) {
                Some((class, next)) => {
                    i = next;
                    class
                }
                None => Token::Literal('['),
            },
            other => Token::Literal(other),
        });
    }
    tokens
}

/// Greedy matching with one backtrack point: the last `*` seen.
fn run(tokens: &[Token], name: &[char]) -> bool {
    let (mut t, mut n) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while let Some(&ch) = name.get(n) {
        match tokens.get(t) {
            Some(Token::Star) => {
                resume = Some((t, n));
                t += 1;
            }
            Some(token) if token.accepts(ch) => {
                t += 1;
                n += 1;
            }
            _ => match resume {
                Some((star, at)) => {
                    resume = Some((star, at + 1));
                    t = star + 1;
                    n = at + 1;
                }
                None => return false,
            },
        }
    }
    tokens
        .get(t..)
        .unwrap_or_default()
        .iter()
        .all(|token| matches!(token, Token::Star))
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn star_is_any_run_including_none() {
        assert!(matches("*.txt", "notes.txt"));
        assert!(matches("*.txt", ".txt"));
        assert!(matches("a*b", "ab"));
        assert!(matches("a*b", "a-long-middle-b"));
        assert!(matches("*", "anything"));
        assert!(!matches("*.txt", "notes.txt.bak"));
        assert!(matches("a*b*c", "aXbYbZc"));
        assert!(!matches("a*b*c", "aXbYbZ"));
    }

    #[test]
    fn question_mark_is_exactly_one_character() {
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "abbc"));
        assert!(matches("?", "é"));
    }

    #[test]
    fn bracket_classes_list_ranges_and_negate() {
        assert!(matches("[abc].txt", "b.txt"));
        assert!(!matches("[abc].txt", "d.txt"));
        assert!(matches("file[0-9]", "file7"));
        assert!(!matches("file[0-9]", "filex"));
        assert!(matches("[!abc]x", "dx"));
        assert!(!matches("[!abc]x", "ax"));
        assert!(matches("[!0-9]*", "name1"));
        assert!(!matches("[!0-9]*", "1name"));
    }

    #[test]
    fn an_unclosed_bracket_is_a_plain_character() {
        assert!(matches("[abc", "[abc"));
        assert!(!matches("[abc", "a"));
        assert!(matches("a[", "a["));
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        assert!(matches("a.b", "a.b"));
        assert!(!matches("a.b", "axb"));
        assert!(matches("a+b", "a+b"));
        assert!(!matches("a+b", "aab"));
        assert!(matches("(1)", "(1)"));
        assert!(matches("a|b", "a|b"));
        assert!(!matches("a|b", "a"));
        assert!(matches("^x$", "^x$"));
        assert!(matches("a{2}", "a{2}"));
        assert!(matches("a\\b", "a\\b"));
    }

    #[test]
    fn matching_ignores_case_including_unicode() {
        assert!(matches("README*", "readme.md"));
        assert!(matches("*.JPG", "photo.jpg"));
        assert!(matches("[A-C]x", "bx"));
        assert!(matches("ÁRBOL*", "árbol.txt"));
        assert!(matches("straße", "STRAßE"));
        assert!(matches("ÑANDÚ", "ñandú"));
    }

    #[test]
    fn the_pattern_must_cover_the_whole_name() {
        assert!(!matches("note", "notes"));
        assert!(!matches("otes", "notes"));
        assert!(matches("notes", "notes"));
    }

    #[test]
    fn an_empty_pattern_matches_nothing() {
        assert!(!matches("", ""));
        assert!(!matches("", "name"));
    }
}
