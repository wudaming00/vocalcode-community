//! Explicit, offline interchange. Import support is not access to another app's
//! private database, cloud history, account, or subscription.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::limits::{MAX_DICTIONARY_RULES, MAX_DICTIONARY_SIDE_UTF8_BYTES};

pub const MAX_IMPORT_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_IMPORT_ROWS: usize = 1000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Dictionary,
    Snippets,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    #[default]
    Auto,
    Words,
    Tsv,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub format: String,
    pub version: u32,
    pub kind: Kind,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewRow {
    pub entry: Entry,
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub rows: Vec<PreviewRow>,
    pub added: usize,
    pub duplicates: usize,
    pub conflicts: usize,
    pub merged: Vec<Entry>,
}

fn identity(name: &str) -> String {
    name.trim().to_lowercase()
}

pub fn validate(kind: Kind, entry: &Entry) -> Result<(), String> {
    let name = &entry.name;
    let text = &entry.text;
    if name.trim().is_empty() || text.trim().is_empty() {
        return Err("Both the name/heard phrase and the text must be non-empty.".into());
    }
    if name.chars().any(char::is_control)
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err("Control characters are not supported.".into());
    }
    match kind {
        Kind::Dictionary => {
            if name.starts_with('#') || name.contains("=>") || text.contains(['\r', '\n']) {
                return Err("Dictionary phrases must be single-line, without a leading # or => in the heard phrase.".into());
            }
            if name.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
                || text.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
            {
                return Err("A dictionary phrase is too long.".into());
            }
        }
        Kind::Snippets => {
            if name.chars().count() > 60 || text.chars().count() > 4000 {
                return Err("Snippet names allow 60 characters; text allows 4,000.".into());
            }
        }
    }
    Ok(())
}

/// RFC4180-style comma-separated records: doubled quotes, BOM, embedded newline
/// in a quoted cell. Tabs/semicolons are not guessed into different schemas.
fn csv(input: &str, delimiter: char) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut chars = input.chars().peekable();
    let mut quoted = false;
    let mut closed = false;
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                cell.push(ch);
            }
            continue;
        }
        match ch {
            '"' if cell.is_empty() && !closed => quoted = true,
            '"' => {
                return Err(
                    "Unexpected quote in CSV. Quote the entire cell and double literal quotes."
                        .into(),
                )
            }
            ch if ch == delimiter || matches!(ch, '\r' | '\n') => {
                row.push(std::mem::take(&mut cell));
                closed = false;
                if row.len() > 2 {
                    return Err(
                        "CSV must have one word column or two heard/replacement columns.".into(),
                    );
                }
                if ch != delimiter {
                    if ch == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    if row.iter().any(|value| !value.trim().is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                    if rows.len() > MAX_IMPORT_ROWS + 1 {
                        return Err("Import allows at most 1,000 rows.".into());
                    }
                }
            }
            ' ' | '\t' if closed => {}
            _ if closed => return Err("Unexpected characters after a closing CSV quote.".into()),
            _ => cell.push(ch),
        }
    }
    if quoted {
        return Err("Unclosed quote in CSV.".into());
    }
    row.push(cell);
    if row.iter().any(|value| !value.trim().is_empty()) {
        rows.push(row);
    }
    if rows.iter().any(|row| row.len() > 2) {
        return Err("CSV must have one or two columns.".into());
    }
    Ok(rows)
}

/// Header removal is explicit: a legitimate dictionary entry named "word" or
/// "name" must never disappear because of heuristic header detection.
pub fn parse(input: &str, kind: Kind, csv_header: bool) -> Result<Vec<Entry>, String> {
    parse_with_layout(input, kind, csv_header, Layout::Auto)
}

/// Explicit layouts support user-copied lists without guessing that a comma
/// inside a product name is a replacement separator, or silently eating tabs.
pub fn parse_with_layout(
    input: &str,
    kind: Kind,
    csv_header: bool,
    layout: Layout,
) -> Result<Vec<Entry>, String> {
    if input.len() > MAX_IMPORT_BYTES {
        return Err("Import is limited to 3 MiB.".into());
    }
    let input = input.trim_start_matches('\u{feff}').trim();
    if input.is_empty() {
        return Err("Choose a non-empty UTF-8 export file or paste some entries.".into());
    }
    if kind != Kind::Dictionary && layout != Layout::Auto {
        return Err(
            "Word lists and tab-separated tables are dictionary-only. Choose JSON for snippets."
                .into(),
        );
    }
    let mut entries = if layout == Layout::Auto && input.starts_with('{') {
        let doc: Document = serde_json::from_str(input).map_err(|_| "Unsupported JSON object. Use a VocalCode interchange document or a name/text snippet array.")?;
        if doc.format != "vocalcode-interchange" || doc.version != 1 || doc.kind != kind {
            return Err(
                "The file format, version, or selected content type does not match.".into(),
            );
        }
        doc.entries
    } else if layout == Layout::Auto && input.starts_with('[') {
        if kind != Kind::Snippets {
            return Err("Select Snippets for a JSON name/text array.".into());
        }
        // Vendor snippet imports may include metadata. Only the documented
        // name/text strings are consumed; no scripts, macros, or URLs execute.
        #[derive(Deserialize)]
        struct Snippet {
            name: String,
            text: String,
        }
        let values: Vec<Snippet> = serde_json::from_str(input).map_err(|_| {
            "Invalid snippet JSON. Expected an array of objects with string name and text."
        })?;
        values
            .into_iter()
            .map(|s| Entry {
                name: s.name,
                text: s.text,
            })
            .collect()
    } else {
        if kind != Kind::Dictionary {
            return Err("Snippets use JSON; dictionary words use CSV or plain lines.".into());
        }
        let mut rows = match layout {
            Layout::Auto => csv(input, ',')?,
            Layout::Tsv => csv(input, '\t')?,
            Layout::Words => input
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| vec![line.to_string()])
                .take(MAX_IMPORT_ROWS + 2)
                .collect(),
        };
        if csv_header && !rows.is_empty() {
            rows.remove(0);
        }
        rows.into_iter()
            .map(|row| Entry {
                name: row[0].trim().to_string(),
                text: row.get(1).unwrap_or(&row[0]).trim().to_string(),
            })
            .collect()
    };
    if entries.is_empty() || entries.len() > MAX_IMPORT_ROWS {
        return Err("Import requires 1–1,000 entries.".into());
    }
    for (index, entry) in entries.iter_mut().enumerate() {
        entry.name = entry.name.trim().to_string();
        // Preserve snippet indentation and intentional leading/trailing spaces.
        if kind == Kind::Dictionary {
            entry.text = entry.text.trim().to_string();
        }
        validate(kind, entry).map_err(|error| format!("Entry {}: {error}", index + 1))?;
    }
    Ok(entries)
}

/// Keep existing entries and first incoming value. Never apply recursive
/// replacements, overwrite a conflict, or silently truncate at capacity.
pub fn preview(kind: Kind, existing: &[Entry], incoming: &[Entry]) -> Result<Preview, String> {
    let mut merged = existing.to_vec();
    let mut keys: HashMap<String, String> = HashMap::new();
    for entry in existing {
        keys.entry(identity(&entry.name))
            .or_insert_with(|| entry.text.clone());
    }
    let mut result = Preview {
        rows: Vec::new(),
        added: 0,
        duplicates: 0,
        conflicts: 0,
        merged: Vec::new(),
    };
    for entry in incoming {
        validate(kind, entry)?;
        let key = identity(&entry.name);
        let status = if let Some(text) = keys.get(&key) {
            if text == &entry.text {
                result.duplicates += 1;
                "duplicate"
            } else {
                result.conflicts += 1;
                "conflict"
            }
        } else {
            keys.insert(key, entry.text.clone());
            merged.push(entry.clone());
            result.added += 1;
            "new"
        };
        result.rows.push(PreviewRow {
            entry: entry.clone(),
            status,
        });
    }
    let max = if kind == Kind::Dictionary {
        MAX_DICTIONARY_RULES
    } else {
        MAX_IMPORT_ROWS
    };
    if merged.len() > max {
        return Err(format!("This merge would exceed {max} saved entries. Split the import or remove unused entries first; nothing was saved."));
    }
    result.merged = merged;
    Ok(result)
}

pub fn export(kind: Kind, entries: Vec<Entry>) -> Result<String, String> {
    serde_json::to_string_pretty(&Document {
        format: "vocalcode-interchange".into(),
        version: 1,
        kind,
        entries,
    })
    .map_err(|e| e.to_string())
}

/// Explicit whole-utterance command only, never a substring replacement.
/// Multiline snippets are copy-only: injecting their newline into a terminal
/// could execute a command. Progressive dictation must not call this helper.
pub fn expand_snippet(text: &str, entries: &[Entry]) -> Option<String> {
    let normalized = text
        .trim()
        .trim_end_matches(['.', '。', '!', '！'])
        .trim()
        .to_lowercase();
    // "Snippets signature." is how recognisers often hear "snippet signature".
    let name = normalized
        .strip_prefix("snippet ")
        .or_else(|| normalized.strip_prefix("snippets "))
        .or_else(|| normalized.strip_prefix("snip it "))
        .or_else(|| normalized.strip_prefix("snipit "))
        .or_else(|| normalized.strip_prefix("插入词块"))?
        .trim();
    entries
        .iter()
        .find(|entry| identity(&entry.name) == name)
        // Saving accepts line breaks and tabs; expansion used to refuse them,
        // so a multi-line snippet saved fine and then never expanded. The
        // injector now delivers multi-line text by paste, where a line break
        // is a line break rather than an Enter that sends a chat message.
        // A trailing line break stays refused: in a terminal it is an Enter
        // that runs whatever the snippet typed ("rm example\n").
        .filter(|entry| {
            !entry
                .text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                && !entry
                    .text
                    .trim_end_matches([' ', '\t'])
                    .ends_with(['\n', '\r'])
        })
        .map(|entry| entry.text.replace("\r\n", "\n").replace('\r', "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(name: &str, text: &str) -> Entry {
        Entry {
            name: name.into(),
            text: text.into(),
        }
    }
    #[test]
    fn multi_line_snippets_expand_with_normalized_line_breaks() {
        let entries = [
            entry("signature", "Best,\r\nDaming\n\tVocalCode"),
            entry("bell", "ring\u{7}"),
        ];
        assert_eq!(
            expand_snippet("Snippet signature.", &entries).as_deref(),
            Some("Best,\nDaming\n\tVocalCode")
        );
        assert_eq!(expand_snippet("snippet bell", &entries), None);
    }

    #[test]
    fn csv_bom_crlf_quotes_commas_and_header() {
        let rows = parse(
            "\u{feff}heard,desired\r\n\"foo, bar\",\"Foo \"\"Bar\"\"\"\r\nCollie",
            Kind::Dictionary,
            true,
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![entry("foo, bar", "Foo \"Bar\""), entry("Collie", "Collie")]
        );
        assert_eq!(
            parse("word", Kind::Dictionary, false).unwrap()[0].name,
            "word"
        );
    }
    #[test]
    fn malformed_csv_and_invalid_rules_fail_atomically() {
        for text in [
            "a,b,c",
            "a,",
            "\"unclosed",
            "a,b\nc,\"multi\nline\"",
            "#a,b",
            "a=>b,c",
            "\"a\"oops,b",
        ] {
            assert!(parse(text, Kind::Dictionary, false).is_err(), "{text}");
        }
    }
    #[test]
    fn snippet_arrays_preserve_content_not_metadata() {
        let rows = parse(
            r#"[{"name":"签名","text":"  Hi\nCollie\n","id":7}]"#,
            Kind::Snippets,
            false,
        )
        .unwrap();
        assert_eq!(rows[0].text, "  Hi\nCollie\n");
        assert!(parse(r#"[{"name":"a","text":2}]"#, Kind::Snippets, false).is_err());
        assert!(parse("a", Kind::Snippets, false).is_err());
    }
    #[test]
    fn merge_preserves_conflicts_and_handles_incoming_duplicates() {
        let p = preview(
            Kind::Dictionary,
            &[entry("Collie", "Collie")],
            &[
                entry("collie", "Dog"),
                entry("new", "New"),
                entry("NEW", "New"),
            ],
        )
        .unwrap();
        assert_eq!((p.added, p.duplicates, p.conflicts), (1, 1, 1));
        assert_eq!(
            p.merged,
            vec![entry("Collie", "Collie"), entry("new", "New")]
        );
    }
    #[test]
    fn export_roundtrip_and_reject_unknown_version() {
        for kind in [Kind::Dictionary, Kind::Snippets] {
            let original = vec![entry("你好", "Hello")];
            let doc = export(kind, original.clone()).unwrap();
            assert_eq!(parse(&doc, kind, false).unwrap(), original);
            assert!(parse(
                &doc.replace("\"version\": 1", "\"version\": 2"),
                kind,
                false
            )
            .is_err());
        }
    }
    #[test]
    fn limits_and_controls_are_enforced() {
        assert!(parse(&"x".repeat(MAX_IMPORT_BYTES + 1), Kind::Dictionary, false).is_err());
        assert!(parse(&"x\n".repeat(MAX_IMPORT_ROWS + 2), Kind::Dictionary, false).is_err());
        assert!(validate(Kind::Snippets, &entry("x", "\u{1b}[31m")).is_err());
        let existing = vec![entry("old", "Old"); MAX_DICTIONARY_RULES];
        assert!(preview(Kind::Dictionary, &existing, &[entry("new", "New")]).is_err());
    }
    #[test]
    fn explicit_snippet_commands_only_no_terminal_newlines() {
        let entries = vec![
            entry("signature", "Regards, Collie"),
            entry("签名", "谢谢"),
            entry("danger", "rm example\n"),
        ];
        assert_eq!(
            expand_snippet("Snippet signature.", &entries).as_deref(),
            Some("Regards, Collie")
        );
        assert_eq!(
            expand_snippet("插入词块签名。", &entries).as_deref(),
            Some("谢谢")
        );
        for text in [
            "signature",
            "Please use snippet signature",
            "snippet danger",
            "snippet signature and more",
        ] {
            assert!(expand_snippet(text, &entries).is_none());
        }
    }

    #[test]
    fn visible_copied_lists_and_tabular_pairs_need_no_export_file() {
        let words = parse_with_layout(
            "Company, Inc.\n[word]\n{term}",
            Kind::Dictionary,
            false,
            Layout::Words,
        )
        .unwrap();
        assert_eq!(
            words,
            vec![
                entry("Company, Inc.", "Company, Inc."),
                entry("[word]", "[word]"),
                entry("{term}", "{term}")
            ]
        );
        let rows = parse_with_layout(
            "heard\tcorrect\r\ncollie\tCollie\r\nfoo, bar\tFoo, Bar",
            Kind::Dictionary,
            true,
            Layout::Tsv,
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![entry("collie", "Collie"), entry("foo, bar", "Foo, Bar")]
        );
        assert!(parse_with_layout("a\tb\tc", Kind::Dictionary, false, Layout::Tsv).is_err());
        assert!(parse_with_layout("one\ttwo", Kind::Dictionary, false, Layout::Words).is_err());
    }
}
