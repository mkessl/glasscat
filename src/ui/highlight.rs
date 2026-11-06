//! Colours for code blocks: keywords, strings, numbers, comments, types
//! and calls, by a small lexer that knows the common languages well
//! enough for reading. Colours come from the terminal's palette, so they
//! follow its theme.

use crossterm::style::Color;

use super::screen::Style;
use super::text::Span;

const KEYWORD: Style = Style::fg(Color::Magenta);
const STRING: Style = Style::fg(Color::Green);
const NUMBER: Style = Style::fg(Color::Cyan);
const TYPE: Style = Style::fg(Color::Yellow);
const CALL: Style = Style::fg(Color::Blue);
const COMMENT: Style = Style::dim().italic();
const ADDED: Style = Style::fg(Color::Green);
const REMOVED: Style = Style::fg(Color::Red);

pub struct Lang {
    names: &'static [&'static str],
    keywords: &'static [&'static str],
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static str,
}

const C_LIKE: &str = "\"'";

#[rustfmt::skip]
static LANGS: &[Lang] = &[
    Lang {
        names: &["rust", "rs"],
        keywords: &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while",
        ],
        line_comment: &["//"],
        block_comment: Some(("/*", "*/")),
        quotes: C_LIKE,
    },
    Lang {
        names: &["python", "py", "python3"],
        keywords: &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
            "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return",
            "True", "try", "while", "with", "yield",
        ],
        line_comment: &["#"],
        block_comment: None,
        quotes: C_LIKE,
    },
    Lang {
        names: &[
            "javascript", "js", "jsx", "mjs", "typescript", "ts", "tsx", "java", "kotlin", "kt",
            "csharp", "cs", "c#", "swift", "scala", "dart",
        ],
        keywords: &[
            "abstract", "as", "async", "await", "break", "case", "catch", "class", "const",
            "continue", "default", "delete", "do", "else", "enum", "export", "extends", "false",
            "final", "finally", "for", "fun", "func", "function", "if", "implements", "import",
            "in", "instanceof", "interface", "let", "new", "null", "override", "package", "private",
            "protected", "public", "return", "static", "super", "switch", "this", "throw", "throws",
            "true", "try", "typeof", "undefined", "val", "var", "void", "while", "yield",
        ],
        line_comment: &["//"],
        block_comment: Some(("/*", "*/")),
        quotes: "\"'`",
    },
    Lang {
        names: &["go", "golang"],
        keywords: &[
            "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
            "false", "for", "func", "go", "goto", "if", "import", "interface", "map", "nil",
            "package", "range", "return", "select", "struct", "switch", "true", "type", "var",
        ],
        line_comment: &["//"],
        block_comment: Some(("/*", "*/")),
        quotes: "\"'`",
    },
    Lang {
        names: &["c", "h", "cpp", "c++", "cc", "hpp", "cxx", "objc"],
        keywords: &[
            "auto", "bool", "break", "case", "char", "class", "const", "constexpr", "continue",
            "default", "delete", "do", "double", "else", "enum", "extern", "false", "float", "for",
            "if", "include", "define", "inline", "int", "long", "namespace", "new", "nullptr",
            "NULL", "private", "public", "return", "short", "signed", "sizeof", "static", "struct",
            "switch", "template", "this", "true", "typedef", "union", "unsigned", "using",
            "virtual", "void", "while",
        ],
        line_comment: &["//"],
        block_comment: Some(("/*", "*/")),
        quotes: C_LIKE,
    },
    Lang {
        names: &[
            "sh", "bash", "shell", "zsh", "fish", "console", "shellsession", "ps1", "powershell",
        ],
        keywords: &[
            "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if",
            "in", "local", "return", "then", "until", "while",
        ],
        line_comment: &["#"],
        block_comment: None,
        quotes: C_LIKE,
    },
    Lang {
        names: &["ruby", "rb"],
        keywords: &[
            "begin", "class", "def", "do", "else", "elsif", "end", "ensure", "false", "if",
            "module", "nil", "require", "rescue", "return", "self", "true", "unless", "until",
            "when", "while", "yield",
        ],
        line_comment: &["#"],
        block_comment: None,
        quotes: C_LIKE,
    },
    Lang {
        names: &["lua"],
        keywords: &[
            "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in",
            "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
        ],
        line_comment: &["--"],
        block_comment: None,
        quotes: C_LIKE,
    },
    Lang {
        names: &["sql", "postgresql", "mysql", "sqlite"],
        keywords: &[
            "AND", "AS", "BY", "CREATE", "DELETE", "DROP", "FROM", "GROUP", "INSERT", "INTO",
            "JOIN", "LEFT", "LIMIT", "NOT", "NULL", "ON", "OR", "ORDER", "SELECT", "SET", "TABLE",
            "UPDATE", "VALUES", "WHERE", "and", "as", "by", "create", "delete", "from", "group",
            "insert", "into", "join", "limit", "not", "null", "on", "or", "order", "select", "set",
            "table", "update", "values", "where",
        ],
        line_comment: &["--"],
        block_comment: Some(("/*", "*/")),
        quotes: C_LIKE,
    },
    Lang {
        names: &["json", "jsonc", "json5"],
        keywords: &["true", "false", "null"],
        line_comment: &["//"],
        block_comment: None,
        quotes: "\"",
    },
    Lang {
        names: &[
            "toml", "yaml", "yml", "ini", "conf", "dockerfile", "makefile", "make",
        ],
        keywords: &["true", "false", "null"],
        line_comment: &["#"],
        block_comment: None,
        quotes: C_LIKE,
    },
];

/// The language a code fence names, such as `rust` in ```` ```rust ````.
pub fn lang(info: &str) -> Option<&'static Lang> {
    let name = info.split([' ', ',', '{']).next()?.to_ascii_lowercase();
    LANGS.iter().find(|l| l.names.contains(&name.as_str()))
}

/// Whether the fence names a diff, whose lines are coloured whole.
pub fn is_diff(info: &str) -> bool {
    matches!(
        info.split(' ')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "diff" | "patch"
    )
}

/// A diff line, coloured by what it does.
pub fn diff_line(line: &str) -> Vec<Span> {
    let style = if line.starts_with("+++") || line.starts_with("---") {
        Style::default().bold()
    } else if line.starts_with('+') {
        ADDED
    } else if line.starts_with('-') {
        REMOVED
    } else if line.starts_with("@@") {
        NUMBER
    } else {
        Style::default()
    };
    vec![(line.to_string(), style)]
}

/// One line of code in colour. `in_comment` carries a block comment over
/// to the next line.
pub fn line(text: &str, lang: &Lang, in_comment: &mut bool) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut push = |s: &str, style: Style| {
        if s.is_empty() {
            return;
        }
        match out.last_mut() {
            Some((t, st)) if *st == style => t.push_str(s),
            _ => out.push((s.to_string(), style)),
        }
    };
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let rest = &text[i..];
        if *in_comment {
            let end = lang.block_comment.map_or("", |c| c.1);
            match rest.find(end) {
                Some(n) => {
                    push(&rest[..n + end.len()], COMMENT);
                    *in_comment = false;
                    i += n + end.len();
                }
                None => {
                    push(rest, COMMENT);
                    i = b.len();
                }
            }
            continue;
        }
        let after_space = i == 0 || b[i - 1].is_ascii_whitespace();
        if lang
            .line_comment
            .iter()
            .any(|c| rest.starts_with(c) && (*c != "#" || after_space))
        {
            push(rest, COMMENT);
            break;
        }
        if let Some((start, _)) = lang.block_comment
            && rest.starts_with(start)
        {
            push(start, COMMENT);
            *in_comment = true;
            i += start.len();
            continue;
        }
        let c = b[i];
        if lang.quotes.as_bytes().contains(&c) && !(c == b'\'' && lifetime(lang, rest)) {
            let n = string_len(rest);
            push(&rest[..n], STRING);
            i += n;
            continue;
        }
        let prev_ident = i > 0 && is_ident(b[i - 1]);
        if c.is_ascii_digit() && !prev_ident {
            let n = rest
                .bytes()
                .take_while(|&x| x.is_ascii_alphanumeric() || x == b'_' || x == b'.')
                .count();
            push(&rest[..n], NUMBER);
            i += n;
            continue;
        }
        if is_ident(c) && !c.is_ascii_digit() {
            let n = rest.bytes().take_while(|&x| is_ident(x)).count();
            let word = &rest[..n];
            let next = rest[n..].trim_start().as_bytes().first().copied();
            let style = if lang.keywords.contains(&word) {
                KEYWORD
            } else if next == Some(b'(') || (next == Some(b'!') && lang.names[0] == "rust") {
                CALL
            } else if word.as_bytes()[0].is_ascii_uppercase() && word.len() > 1 {
                TYPE
            } else {
                Style::default()
            };
            push(word, style);
            i += n;
            continue;
        }
        let n = rest.chars().next().map_or(1, char::len_utf8);
        push(&rest[..n], Style::default());
        i += n;
    }
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// A Rust lifetime such as `'a`, which opens no string.
fn lifetime(lang: &Lang, rest: &str) -> bool {
    if lang.names[0] != "rust" {
        return false;
    }
    // 'x', '→' and '\n' are characters; 'a followed by anything else is not.
    match rest[1..].chars().next() {
        Some('\\') => false,
        Some(c) => !rest[1 + c.len_utf8()..].starts_with('\''),
        None => true,
    }
}

/// The length of the string starting `rest`, to its closing quote or the
/// end of the line.
fn string_len(rest: &str) -> usize {
    let q = rest.as_bytes()[0];
    let mut escaped = false;
    for (j, &x) in rest.as_bytes().iter().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if x == b'\\' {
            escaped = true;
        } else if x == q {
            return j + 1;
        }
    }
    rest.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(code: &str, lang: &str) -> Vec<(String, Style)> {
        let mut c = false;
        line(code, super::lang(lang).unwrap(), &mut c)
    }

    #[test]
    fn colours_rust() {
        let s = styled(r#"let n: Vec<u8> = f("a\"b", 'c', 42); // done"#, "rust");
        let find = |t: &str| s.iter().find(|(x, _)| x == t).map(|(_, st)| *st);
        assert_eq!(find("let"), Some(KEYWORD));
        assert_eq!(find("Vec"), Some(TYPE));
        assert_eq!(find("f"), Some(CALL));
        assert_eq!(find(r#""a\"b""#), Some(STRING));
        assert_eq!(find("'c'"), Some(STRING));
        assert_eq!(find("42"), Some(NUMBER));
        assert_eq!(find("// done"), Some(COMMENT));
        let text: String = s.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text, r#"let n: Vec<u8> = f("a\"b", 'c', 42); // done"#);
        assert_eq!(styled("'→'", "rust"), [("'→'".to_string(), STRING)]);
        // A lifetime opens no string.
        let s = styled("fn f<'a>(x: &'a str) {}", "rust");
        assert!(s.iter().all(|(_, st)| *st != STRING), "{s:?}");
    }

    #[test]
    fn carries_block_comments_and_knows_shell_hashes() {
        let js = lang("js").unwrap();
        let mut c = false;
        assert_eq!(line("a /* b", js, &mut c).last().unwrap().1, COMMENT);
        assert!(c);
        let s = line("still */ x", js, &mut c);
        assert_eq!(s[0], ("still */".to_string(), COMMENT));
        assert!(!c);
        let s = styled("echo ${#x} # count", "bash");
        assert_eq!(s.last().unwrap(), &("# count".to_string(), COMMENT));
        assert!(
            s.iter()
                .any(|(t, st)| t.contains("${#x}") && *st != COMMENT)
        );
        assert!(lang("unknown").is_none());
        assert_eq!(lang("Rust ignore").unwrap().names[0], "rust");
    }
}
