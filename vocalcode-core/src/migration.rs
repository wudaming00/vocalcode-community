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
    /// VocalCode's own `replacements.txt`: `heard => written` per line.
    Rules,
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

/// One line of the `replacements.txt` grammar, read exactly as the app reads
/// its own file: split at the first `=>`, both sides trimmed and non-empty.
/// `Ok(None)` is a blank line or a `#` comment.
fn rule_line(line: &str) -> Result<Option<(String, String)>, ()> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let (heard, written) = line.split_once("=>").ok_or(())?;
    let (heard, written) = (heard.trim(), written.trim());
    if heard.is_empty() || written.is_empty() {
        return Err(());
    }
    Ok(Some((heard.to_string(), written.to_string())))
}

/// Whether input that failed as CSV reads as a pasted `replacements.txt`:
/// every line that is not blank or a `#` comment contains `=>`, and at least
/// one does. Only consulted after CSV, so anything that imports as CSV keeps
/// its meaning (`arrow,=>` is still the word "arrow" written as `=>`). A
/// double quote before the `=>` is CSV quoting, not a heard phrase: splitting
/// `"lambda => x",λ` there would import the quote marks as words, so that
/// input keeps its CSV error instead.
fn looks_like_rules(input: &str) -> bool {
    let mut rules = 0usize;
    for line in input.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once("=>") {
            Some((heard, _)) if !heard.contains('"') => rules += 1,
            _ => return false,
        }
    }
    rules > 0
}

fn rules(input: &str) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    for (index, line) in input.lines().enumerate() {
        match rule_line(line) {
            Ok(Some((heard, written))) => rows.push(vec![heard, written]),
            Ok(None) => {}
            Err(()) => {
                return Err(format!(
                    "Line {} is not a “heard => written” rule.",
                    index + 1
                ))
            }
        }
        if rows.len() > MAX_IMPORT_ROWS {
            return Err("Import allows at most 1,000 rows.".into());
        }
    }
    Ok(rows)
}

/// The rules a previous VocalCode's `replacements.txt` actually applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesFile {
    pub entries: Vec<Entry>,
    /// Lines that app ignored too (no `=>`, an empty side) plus rules this
    /// build refuses (for example control characters). Counted, never fatal:
    /// one hand-edited typo must not strand the rest of someone's words.
    pub ignored: usize,
}

/// Read a whole `replacements.txt` the way the app that wrote it read it.
/// Unlike [`Layout::Rules`], which rejects a malformed pasted line so the
/// person can fix it, this is for a file the person never sees here.
pub fn parse_rules_file(input: &str) -> Result<RulesFile, String> {
    if input.len() > MAX_IMPORT_BYTES {
        return Err("Import is limited to 3 MiB.".into());
    }
    let mut result = RulesFile {
        entries: Vec::new(),
        ignored: 0,
    };
    for line in input.trim_start_matches('\u{feff}').lines() {
        let Ok(rule) = rule_line(line) else {
            result.ignored += 1;
            continue;
        };
        let Some((name, text)) = rule else {
            continue;
        };
        let entry = Entry { name, text };
        if validate(Kind::Dictionary, &entry).is_err() {
            result.ignored += 1;
            continue;
        }
        result.entries.push(entry);
        if result.entries.len() > MAX_IMPORT_ROWS {
            return Err("Import allows at most 1,000 rows.".into());
        }
    }
    Ok(result)
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
    let entries = if layout == Layout::Auto && input.starts_with('{') {
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
        if layout == Layout::Auto {
            // CSV first, exactly as before rules existed. Only input that
            // fails as CSV (a `=>` in a heard phrase, `#` comments, a third
            // column) and reads as rules is imported as rules instead.
            return match table(input, csv_header, Layout::Auto)
                .and_then(|entries| checked(kind, entries))
            {
                Err(_) if looks_like_rules(input) => {
                    checked(kind, table(input, csv_header, Layout::Rules)?)
                }
                result => result,
            };
        }
        table(input, csv_header, layout)?
    };
    checked(kind, entries)
}

/// One dictionary table as entries, before validation.
fn table(input: &str, csv_header: bool, layout: Layout) -> Result<Vec<Entry>, String> {
    let mut rows = match layout {
        Layout::Auto => csv(input, ',')?,
        Layout::Tsv => csv(input, '\t')?,
        Layout::Rules => rules(input)?,
        Layout::Words => input
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| vec![line.to_string()])
            .take(MAX_IMPORT_ROWS + 2)
            .collect(),
    };
    // `#` comments already carry the header of a rules file; dropping the
    // first rule because a CSV checkbox was left on would lose a word.
    if csv_header && layout != Layout::Rules && !rows.is_empty() {
        rows.remove(0);
    }
    Ok(rows
        .into_iter()
        .map(|row| Entry {
            name: row[0].trim().to_string(),
            text: row.get(1).unwrap_or(&row[0]).trim().to_string(),
        })
        .collect())
}

/// The row limits and per-entry validation every import layout shares.
fn checked(kind: Kind, mut entries: Vec<Entry>) -> Result<Vec<Entry>, String> {
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
        .or_else(|| normalized.strip_prefix("插入词块"))
        .or_else(|| normalized.strip_prefix("插入此块"))?
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
            "a=>b\nc,d",
            "=>b",
            "a=>",
            "# only a comment\n",
            "\"a\"oops,b",
        ] {
            assert!(parse(text, Kind::Dictionary, false).is_err(), "{text}");
        }
    }

    /// The shape of the app's own `replacements.txt` and the built-in
    /// `default_replacements.txt`: `#` comments, blank lines, column-aligned
    /// `heard => written` pairs, and the first `=>` as the only separator.
    const RULES_DOCUMENT: &str =
        "\u{feff}# VocalCode built-in recognition-correction dictionary.\r\n\
        #\r\n\
        # To turn one off, map it to itself (e.g. `cloud code => cloud code`).\r\n\
        \r\n\
        # --- the product itself ---\r\n\
        wol cold cold       => VocalCode\r\n\
        vocal code          => VocalCode\r\n\
        \r\n\
        foo, bar            => Foo, Bar\r\n\
        arrow               => =>\r\n\
        cloud code          => cloud code\r\n";

    #[test]
    fn replacements_file_rules_import_by_detection_and_explicit_layout() {
        let expected = vec![
            entry("wol cold cold", "VocalCode"),
            entry("vocal code", "VocalCode"),
            entry("foo, bar", "Foo, Bar"),
            entry("arrow", "=>"),
            entry("cloud code", "cloud code"),
        ];
        assert_eq!(
            parse(RULES_DOCUMENT, Kind::Dictionary, false).unwrap(),
            expected
        );
        // A left-on CSV header checkbox must not eat the first rule.
        assert_eq!(
            parse_with_layout(RULES_DOCUMENT, Kind::Dictionary, true, Layout::Rules).unwrap(),
            expected
        );
        assert_eq!(
            parse("a=>b,c", Kind::Dictionary, false).unwrap(),
            vec![entry("a", "b,c")]
        );
        // Explicit rules refuse a typo and name the line to fix.
        let error = parse_with_layout(
            "# header\ncollie => Collie\ncollie Collie",
            Kind::Dictionary,
            false,
            Layout::Rules,
        )
        .unwrap_err();
        assert!(error.contains("Line 3"), "{error}");
        assert!(parse_with_layout("a => b", Kind::Snippets, false, Layout::Rules).is_err());
        // Plain CSV and word lists keep their exact meaning.
        assert_eq!(
            parse("collie,Collie", Kind::Dictionary, false).unwrap(),
            vec![entry("collie", "Collie")]
        );
    }

    /// Rules are only a reading for input that fails as CSV, so every row the
    /// CSV importer accepted or refused before rules existed still is.
    #[test]
    fn csv_rows_mentioning_the_arrow_keep_their_csv_meaning() {
        // The arrow as a replacement is a valid CSV row, not a broken rule.
        assert_eq!(
            parse("arrow,=>", Kind::Dictionary, false).unwrap(),
            vec![entry("arrow", "=>")]
        );
        assert_eq!(
            parse("lambda,\"x => y\"", Kind::Dictionary, false).unwrap(),
            vec![entry("lambda", "x => y")]
        );
        // A quoted heard phrase containing the arrow was refused as CSV (a
        // heard phrase cannot contain `=>`). It must stay refused, never be
        // split inside the quotes into `"lambda` / `x",λ`.
        for text in [
            "\"lambda => x\",λ",
            "\"a => b\"",
            "x,\"a => b\",c",
            "collie => Collie\n\"lambda => x\",λ",
        ] {
            let error = parse(text, Kind::Dictionary, false).unwrap_err();
            assert!(!error.contains("rule"), "{text}: {error}");
        }
        let error = parse("\"lambda => x\",λ", Kind::Dictionary, false).unwrap_err();
        assert!(error.contains("=> in the heard phrase"), "{error}");
        // Input that is not valid CSV and reads as rules still becomes rules.
        assert_eq!(
            parse(
                "# mine\ncollie => Collie\nfoo, bar => Foo, Bar",
                Kind::Dictionary,
                true
            )
            .unwrap(),
            vec![entry("collie", "Collie"), entry("foo, bar", "Foo, Bar")]
        );
        // The explicit layout is how a rule with a quoted heard phrase goes in.
        assert_eq!(
            parse_with_layout(
                "\"quoted\" => Quoted",
                Kind::Dictionary,
                false,
                Layout::Rules
            )
            .unwrap(),
            vec![entry("\"quoted\"", "Quoted")]
        );
    }

    #[test]
    fn previous_replacements_file_counts_ignored_lines_instead_of_failing() {
        let file = parse_rules_file(&format!(
            "{RULES_DOCUMENT}no separator here\nempty side =>\nbell => ring\u{7}\n"
        ))
        .unwrap();
        assert_eq!(file.entries.len(), 5);
        assert_eq!(file.entries[3], entry("arrow", "=>"));
        assert_eq!(file.ignored, 3);
        let bell = parse_rules_file("bell\u{7} => ring\n").unwrap();
        assert!(bell.entries.is_empty());
        assert_eq!(bell.ignored, 1);
        // The untouched template has nothing to import, and that is not an error.
        let template = parse_rules_file("# One rule per line: heard => written\n").unwrap();
        assert_eq!(
            template,
            RulesFile {
                entries: vec![],
                ignored: 0
            }
        );
        assert!(parse_rules_file(&"a => b\n".repeat(MAX_IMPORT_ROWS + 1)).is_err());
        assert!(parse_rules_file(&"x".repeat(MAX_IMPORT_BYTES + 1)).is_err());
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
