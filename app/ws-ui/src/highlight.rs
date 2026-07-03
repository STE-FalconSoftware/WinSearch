//! A small, dependency-free syntax highlighter for the preview pane. It is
//! deliberately generic (strings, numbers, comments, keywords, punctuation)
//! rather than grammar-perfect — enough to make JSON and code readable without
//! pulling in a full highlighting engine. Results are memoized per (lang, text)
//! via egui's frame cache, so the layouter stays cheap on every repaint.

use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat};

struct Palette {
    default: Color32,
    keyword: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
    punct: Color32,
}

impl Palette {
    fn dark() -> Self {
        Palette {
            default: Color32::from_rgb(0xD4, 0xD4, 0xD4),
            keyword: Color32::from_rgb(0x56, 0x9C, 0xD6),
            string: Color32::from_rgb(0xCE, 0x91, 0x78),
            number: Color32::from_rgb(0xB5, 0xCE, 0xA8),
            comment: Color32::from_rgb(0x6A, 0x99, 0x55),
            punct: Color32::from_rgb(0x9C, 0xA3, 0xAF),
        }
    }
}

/// Highlight `code` for `lang`, memoized across frames.
pub fn highlight(ctx: &egui::Context, code: &str, lang: &str) -> LayoutJob {
    type Cache = egui::util::cache::FrameCache<LayoutJob, Highlighter>;
    ctx.memory_mut(|m| m.caches.cache::<Cache>().get((lang, code)))
}

#[derive(Default)]
struct Highlighter;

impl egui::util::cache::ComputerMut<(&str, &str), LayoutJob> for Highlighter {
    fn compute(&mut self, (lang, code): (&str, &str)) -> LayoutJob {
        compute_job(lang, code)
    }
}

fn compute_job(lang: &str, code: &str) -> LayoutJob {
    let pal = Palette::dark();
    let font = FontId::monospace(13.0);
    let mut job = LayoutJob::default();
    let mut push = |text: &str, color: Color32| {
        job.append(
            text,
            0.0,
            TextFormat {
                font_id: font.clone(),
                color,
                ..Default::default()
            },
        );
    };

    let ch: Vec<char> = code.chars().collect();
    let n = ch.len();
    let line_comment = line_comment_for(lang);
    let block = block_comments_for(lang);
    let kw = keywords();

    let mut i = 0;
    while i < n {
        let c = ch[i];

        // Line comment
        if let Some(lc) = line_comment {
            if matches_at(&ch, i, lc) {
                let start = i;
                while i < n && ch[i] != '\n' {
                    i += 1;
                }
                push(&collect(&ch, start, i), pal.comment);
                continue;
            }
        }
        // Block comment /* ... */
        if block && c == '/' && i + 1 < n && ch[i + 1] == '*' {
            let start = i;
            i += 2;
            while i < n && !(ch[i] == '*' && i + 1 < n && ch[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            push(&collect(&ch, start, i), pal.comment);
            continue;
        }
        // String / char literal
        if c == '"' || c == '\'' || c == '`' {
            let quote = c;
            let start = i;
            i += 1;
            while i < n {
                if ch[i] == '\\' {
                    i += 2;
                    continue;
                }
                if ch[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(&collect(&ch, start, i.min(n)), pal.string);
            continue;
        }
        // Number
        if c.is_ascii_digit() {
            let start = i;
            while i < n && (ch[i].is_ascii_alphanumeric() || ch[i] == '.' || ch[i] == '_') {
                i += 1;
            }
            push(&collect(&ch, start, i), pal.number);
            continue;
        }
        // Identifier / keyword
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < n && (ch[i].is_alphanumeric() || ch[i] == '_') {
                i += 1;
            }
            let word = collect(&ch, start, i);
            let color = if kw.contains(&word.as_str()) {
                pal.keyword
            } else {
                pal.default
            };
            push(&word, color);
            continue;
        }
        // Punctuation vs whitespace/other
        let color = if "{}[]()<>.,;:+-*/%=&|!?".contains(c) {
            pal.punct
        } else {
            pal.default
        };
        push(&c.to_string(), color);
        i += 1;
    }

    job
}

fn collect(ch: &[char], a: usize, b: usize) -> String {
    ch[a..b.min(ch.len())].iter().collect()
}

fn matches_at(ch: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    if i + p.len() > ch.len() {
        return false;
    }
    (0..p.len()).all(|k| ch[i + k] == p[k])
}

fn line_comment_for(lang: &str) -> Option<&'static str> {
    match lang {
        "py" | "rb" | "sh" | "bash" | "ps1" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf"
        | "r" | "pl" | "dockerfile" | "makefile" | "gitignore" | "env" => Some("#"),
        "sql" | "lua" | "hs" => Some("--"),
        "json" => None,
        _ => Some("//"),
    }
}

fn block_comments_for(lang: &str) -> bool {
    matches!(
        lang,
        "rs" | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cc"
            | "cs"
            | "java"
            | "go"
            | "css"
            | "scss"
            | "php"
            | "kt"
            | "swift"
            | "scala"
            | "dart"
    )
}

fn keywords() -> std::collections::HashSet<&'static str> {
    [
        "true",
        "false",
        "null",
        "nil",
        "none",
        "undefined",
        "if",
        "else",
        "elif",
        "for",
        "while",
        "do",
        "return",
        "fn",
        "func",
        "function",
        "def",
        "lambda",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "impl",
        "pub",
        "let",
        "const",
        "var",
        "mut",
        "static",
        "use",
        "import",
        "include",
        "require",
        "from",
        "as",
        "match",
        "case",
        "switch",
        "default",
        "break",
        "continue",
        "new",
        "delete",
        "this",
        "self",
        "super",
        "async",
        "await",
        "yield",
        "try",
        "catch",
        "finally",
        "throw",
        "throws",
        "public",
        "private",
        "protected",
        "abstract",
        "final",
        "void",
        "int",
        "long",
        "float",
        "double",
        "bool",
        "boolean",
        "char",
        "string",
        "str",
        "in",
        "is",
        "of",
        "and",
        "or",
        "not",
        "with",
        "where",
        "type",
        "namespace",
        "package",
        "module",
        "extends",
        "implements",
        "override",
    ]
    .into_iter()
    .collect()
}
