//! Tiny, fast SQL syntax highlighter producing an egui `LayoutJob` (cached by content hash).

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontId};

const KEYWORDS: &[&str] = &[
    "ADD", "ALTER", "ALWAYS", "AND", "AS", "ASC", "ATTRIBUTE", "BEFORE", "AFTER", "BEGIN", "BY", "CACHE", "CASCADE",
    "CHECK", "COLLATE", "COLUMN", "COMMENT", "COMMIT", "CONCURRENTLY", "CONSTRAINT", "CREATE", "CYCLE", "DEFAULT",
    "DELETE", "DESC", "DISABLE", "DOMAIN", "DROP", "EACH", "ENABLE", "ENUM", "EXECUTE", "EXISTS", "EXTENSION",
    "FOR", "FORCE", "FOREIGN", "FROM", "FUNCTION", "GENERATED", "IDENTITY", "IF", "IN", "INCREMENT", "INDEX",
    "INSERT", "INTO", "IS", "KEY", "LANGUAGE", "LEVEL", "LOGGED", "MATERIALIZED", "MAXVALUE", "MINVALUE", "NO",
    "NONE", "NOT", "NULL", "OF", "ON", "OR", "OWNED", "PARTITION", "PERMISSIVE", "POLICY", "PRIMARY", "PROCEDURE",
    "REFERENCES", "REPLACE", "RESTRICTIVE", "RETURNS", "ROLLBACK", "ROW", "ROWS", "SCHEMA", "SECURITY", "SELECT",
    "SEQUENCE", "SET", "START", "STORED", "TABLE", "TO", "TRIGGER", "TYPE", "UNIQUE", "UNLOGGED", "UPDATE", "USING",
    "VALUE", "VALUES", "VERSION", "VIEW", "WHERE", "WITH", "WITHOUT", "JOIN", "LEFT", "INNER", "OUTER", "GROUP",
    "ORDER", "LIMIT", "CASE", "WHEN", "THEN", "ELSE", "END", "RETURN", "DECLARE", "STABLE", "IMMUTABLE",
    "VOLATILE", "STRICT", "DEFINER", "INVOKER", "TRUE", "FALSE", "UNION", "ALL", "DISTINCT", "HAVING", "LIKE",
    "BETWEEN", "DEFERRABLE", "INITIALLY", "DEFERRED", "IMMEDIATE", "MATCH", "SIMPLE", "FULL", "ACTION",
    "RESTRICT", "INCLUDE", "OVERRIDING", "SYSTEM", "INHERITS", "ONLY", "VALIDATE", "VALID", "RENAME", "OWNER",
    "GRANT", "REVOKE", "PUBLIC",
];

struct Palette {
    text: Color32,
    keyword: Color32,
    string: Color32,
    comment: Color32,
    number: Color32,
    ident: Color32,
    hazard: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            text: Color32::from_rgb(212, 216, 224),
            keyword: Color32::from_rgb(110, 168, 255),
            string: Color32::from_rgb(152, 205, 130),
            comment: Color32::from_rgb(120, 128, 140),
            number: Color32::from_rgb(230, 178, 110),
            ident: Color32::from_rgb(222, 160, 230),
            hazard: Color32::from_rgb(255, 120, 110),
        }
    } else {
        Palette {
            text: Color32::from_rgb(36, 41, 47),
            keyword: Color32::from_rgb(0, 82, 204),
            string: Color32::from_rgb(10, 120, 40),
            comment: Color32::from_rgb(110, 119, 129),
            number: Color32::from_rgb(170, 90, 0),
            ident: Color32::from_rgb(130, 40, 150),
            hazard: Color32::from_rgb(200, 30, 30),
        }
    }
}

pub fn highlight(text: &str, dark: bool, font: FontId) -> LayoutJob {
    thread_local! {
        static CACHE: RefCell<HashMap<u64, LayoutJob>> = RefCell::new(HashMap::new());
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    dark.hash(&mut h);
    font.size.to_bits().hash(&mut h);
    let key = h.finish();
    if let Some(j) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return j;
    }
    let job = build(text, dark, font);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 64 {
            c.clear();
        }
        c.insert(key, job.clone());
    });
    job
}

fn build(text: &str, dark: bool, font: FontId) -> LayoutJob {
    let p = palette(dark);
    let mut job = LayoutJob::default();
    let fmt = |c: Color32| TextFormat::simple(font.clone(), c);
    let bytes = text.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        let c = bytes[i];
        let start = i;
        let color;
        if c == b'-' && i + 1 < n && bytes[i + 1] == b'-' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            let line = &text[start..i];
            color = if line.contains("[DATA LOSS") || line.contains("MANUAL") { p.hazard } else { p.comment };
        } else if c == b'\'' {
            i += 1;
            while i < n {
                if bytes[i] == b'\'' {
                    if i + 1 < n && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            color = p.string;
        } else if c == b'"' {
            i += 1;
            while i < n && bytes[i] != b'"' {
                i += 1;
            }
            i = (i + 1).min(n);
            color = p.ident;
        } else if c == b'$' {
            // Dollar quote: $tag$ ... $tag$
            let mut j = i + 1;
            while j < n && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j < n && bytes[j] == b'$' {
                let tag = &text[i..=j];
                if let Some(end) = text[j + 1..].find(tag) {
                    i = j + 1 + end + tag.len();
                } else {
                    i = n;
                }
                color = p.string;
            } else {
                i = j.max(i + 1);
                color = p.text;
            }
        } else if c.is_ascii_digit() {
            while i < n && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            color = p.number;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &text[start..i];
            color = if KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word)) && word.chars().all(|ch| !ch.is_ascii_lowercase()) {
                p.keyword
            } else {
                p.text
            };
        } else {
            // Advance one UTF-8 char.
            i += text[i..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            while i < n && !matches!(bytes[i], b'-' | b'\'' | b'"' | b'$' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_') {
                i += text[i..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            }
            color = p.text;
        }
        job.append(&text[start..i], 0.0, fmt(color));
    }
    job
}
