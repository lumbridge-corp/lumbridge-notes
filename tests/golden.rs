//! The golden format suite: `lumbridge_notes::store` against the files in
//! `tests/golden/`.
//!
//! Three implementations must agree byte for byte on the note format: this
//! crate's `store.rs`, Lumbridge TUI's notebook drawer (`src/client/notebook.rs`, its own
//! `golden` test module) and the `notes-core` crate, which lives in another
//! repository. The files are blessed there from this crate's functions at a
//! pinned commit and copied here by `scripts/sync-golden.sh`; never edit them
//! here. A failure means `store` changed the format since that commit. If the
//! change is intended, bless the files again on the notes-core side and sync
//! them back.
//!
//! `roles.tsv` pins notes-core's `span_role`, which has no counterpart here;
//! the manifest still covers it, so both sides hold the same files.
//!
//! The table format is described in the notes-core README.
//! `escape`, `unescape` and `apply` are the same as in its `golden.rs`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use lumbridge_notes::buffer::Buffer;
use lumbridge_notes::library;
use lumbridge_notes::retitle::{self, LineEdit};
use lumbridge_notes::store::{self, SavePolicy};
use serde_json::Value;

/// At least this many notes, so the suite keeps covering every case
/// (front matter kinds, BOM, line endings, setext, bad TOML...).
const MIN_NOTES: usize = 30;

fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn read(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    String::from_utf8(bytes).unwrap_or_else(|_| panic!("{} is not UTF-8", path.display()))
}

/// The `.md` files in `golden/<dir>`, sorted by name.
fn md_files(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(golden().join(dir))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md"))
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// One row of a table: its line number, its input cells and the expected
/// last cell.
struct Row {
    line: usize,
    inputs: Vec<String>,
    expected: String,
}

/// The rows of `golden/<name>`, without its header. `json` lists the columns
/// that hold JSON (not escaped); every other cell is unescaped.
fn table(name: &str, json: &[usize]) -> Vec<Row> {
    let text = read(&golden().join(name));
    let mut lines = text.lines().enumerate();
    let (_, header) = lines.next().expect("a header line");
    assert!(
        header.starts_with("# "),
        "{name}: the first line is a header"
    );
    let columns = header.split('\t').count();
    lines
        .map(|(index, line)| {
            let cells: Vec<String> = line
                .split('\t')
                .enumerate()
                .map(|(column, cell)| {
                    if json.contains(&column) {
                        cell.to_owned()
                    } else {
                        unescape(cell)
                    }
                })
                .collect();
            assert_eq!(cells.len(), columns, "{name}:{}: column count", index + 1);
            let (expected, inputs) = cells.split_last().unwrap();
            Row {
                line: index + 1,
                inputs: inputs.to_vec(),
                expected: expected.clone(),
            }
        })
        .collect()
}

/// A table cell: `\\`, `\t`, `\n`, `\r`, and `\u{..}` for other controls.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            ch if (ch as u32) < 0x20 || ch == '\u{7f}' => {
                out.push_str(&format!("\\u{{{:x}}}", ch as u32))
            }
            ch => out.push(ch),
        }
    }
    out
}

fn unescape(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut chars = cell.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('u') => {
                assert_eq!(chars.next(), Some('{'), "bad escape in {cell:?}");
                let hex: String = chars.by_ref().take_while(|&ch| ch != '}').collect();
                let code = u32::from_str_radix(&hex, 16).expect("hex escape");
                out.push(char::from_u32(code).expect("a char"));
            }
            other => panic!("bad escape \\{other:?} in {cell:?}"),
        }
    }
    out
}

/// Applies retitle edits as the editor does: bottom-up, so earlier line
/// numbers stay valid, with any line break in the new text splitting a line.
fn apply(lines: &[String], edits: &[LineEdit]) -> Vec<String> {
    let mut ordered = edits.to_vec();
    ordered.sort_by_key(|edit| match edit {
        LineEdit::Replace(index, _) | LineEdit::Insert(index, _) => std::cmp::Reverse(*index),
    });
    let mut out = lines.to_vec();
    for edit in ordered {
        match edit {
            LineEdit::Replace(index, text) => out[index] = text,
            LineEdit::Insert(index, new) => {
                for (offset, line) in new.into_iter().enumerate() {
                    out.insert(index + offset, line);
                }
            }
        }
    }
    out.iter()
        .flat_map(|line| line.split('\n'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn escaping_round_trips() {
    let text = "a\\b\tc\nd\re\u{7}f\u{7f}g\u{1b}h 日本 🚀";
    assert_eq!(
        escape(text),
        "a\\\\b\\tc\\nd\\re\\u{7}f\\u{7f}g\\u{1b}h 日本 🚀"
    );
    assert_eq!(unescape(&escape(text)), text);
}

// ---------------------------------------------------------------------------
// The format
// ---------------------------------------------------------------------------

#[test]
fn notes_parse_as_blessed() {
    let expected: Value = serde_json::from_str(&read(&golden().join("expected.json"))).unwrap();
    let expected = expected.as_object().expect("expected.json is an object");
    let names = md_files("notes");
    assert!(
        names.len() >= MIN_NOTES,
        "{} notes, want at least {MIN_NOTES}",
        names.len()
    );
    let listed: BTreeSet<&str> = expected.keys().map(String::as_str).collect();
    let present: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(listed, present, "expected.json lists exactly the notes");

    let mut wrong = Vec::new();
    for name in &names {
        let path = Path::new("notes").join(name);
        let text = read(&golden().join(&path));
        let (front, body) = store::parse_note(&text);
        let ours = serde_json::json!({
            "title": store::note_title(&text, &path),
            "tags": front.tags,
            "created": front.created,
            "body_start": text.len() - body.len(),
            "heading_title": store::heading_title(body),
            "summary": front.summary(),
        });
        if ours != expected[name] {
            wrong.push(format!(
                "{name}:\n  blessed {}\n  store   {ours}",
                expected[name]
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "notes parsed differently:\n{}",
        wrong.join("\n")
    );
}

/// Fails with every row where `ours` differs from the blessed value.
fn check_table(name: &str, json: &[usize], ours: impl Fn(&[String]) -> String) {
    let rows = table(name, json);
    assert!(!rows.is_empty(), "{name} has rows");
    let wrong: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            let got = ours(&row.inputs);
            (got != row.expected).then(|| {
                format!(
                    "{name}:{}: {:?}\n  blessed {:?}\n  store   {:?}",
                    row.line, row.inputs, row.expected, got
                )
            })
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{} rows differ:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

#[test]
fn slugs_match() {
    check_table("slugs.tsv", &[], |cells| store::slug(&cells[0]));
}

#[test]
fn new_notes_match() {
    check_table("new_note.tsv", &[], |cells| {
        store::new_note_text(&cells[0], &cells[1])
    });
}

#[test]
fn retitles_match() {
    let rows = table("retitle.tsv", &[2, 3]);
    assert!(!rows.is_empty(), "retitle.tsv has rows");
    for row in rows {
        let lines: Vec<String> = serde_json::from_str(&row.inputs[2]).unwrap();
        let want: Vec<String> = serde_json::from_str(&row.expected).unwrap();
        let edits = retitle::edits(&lines, &row.inputs[0], &row.inputs[1]);
        assert_eq!(
            apply(&lines, &edits),
            want,
            "retitle.tsv:{}: {:?} -> {:?}",
            row.line,
            row.inputs[0],
            row.inputs[1]
        );
    }
}

#[test]
fn templates_fill_as_blessed() {
    check_table("fill.tsv", &[], |cells| {
        library::fill(&cells[0], &cells[1], &cells[2], &cells[3])
    });
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

static TEMP: AtomicU64 = AtomicU64::new(0);

/// A fresh folder under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "lumbridge-notes-golden-{}-{}",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Every note and round-trip file comes back byte for byte from the buffer,
/// and saving it unchanged writes the same bytes.
#[test]
fn files_round_trip() {
    let temp = TempDir::new();
    let mut count = 0;
    for dir in ["roundtrip", "notes"] {
        for name in md_files(dir) {
            let source = golden().join(dir).join(&name);
            let bytes = fs::read(&source).unwrap();
            let text = String::from_utf8(bytes.clone()).unwrap();
            let buffer = Buffer::from_text(&text);
            assert_eq!(buffer.to_text(), text, "{dir}/{name}: buffer round trip");

            let copy = temp.0.join(format!("{dir}-{name}"));
            fs::write(&copy, &bytes).unwrap();
            let loaded = store::load(&copy).unwrap();
            assert_eq!(loaded.text, text, "{dir}/{name}: load");
            let again = Buffer::from_text(&loaded.text).to_text();
            store::save(&copy, &again, loaded.stamp.as_ref(), SavePolicy::Safe)
                .unwrap_or_else(|err| panic!("{dir}/{name}: save: {err}"));
            assert_eq!(fs::read(&copy).unwrap(), bytes, "{dir}/{name}: saved bytes");
            count += 1;
        }
    }
    assert!(count >= MIN_NOTES + 8, "{count} files");
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// Whether `relative` is a fixture the manifest covers: everything but the
/// manifest itself, the bless tools (which stay with notes-core) and dot-files.
fn in_manifest(relative: &str) -> bool {
    relative != "MANIFEST.sha256"
        && !relative.starts_with("bless/")
        && !relative.rsplit('/').next().unwrap().starts_with('.')
}

fn fixtures(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if entry.file_type().unwrap().is_dir() {
            fixtures(&entry.path(), &format!("{name}/"), out);
        } else if in_manifest(&name) {
            out.insert(name);
        }
    }
}

#[test]
fn manifest_matches_the_files() {
    let manifest = read(&golden().join("MANIFEST.sha256"));
    let mut listed = BTreeSet::new();
    let mut wrong = Vec::new();
    for line in manifest.lines() {
        let (sum, file) = line
            .split_once("  ")
            .unwrap_or_else(|| panic!("bad manifest line {line:?}"));
        let bytes = fs::read(golden().join(file)).unwrap_or_else(|err| panic!("{file}: {err}"));
        if sha256_hex(&bytes) != sum {
            wrong.push(file.to_owned());
        }
        listed.insert(file.to_owned());
    }
    let mut present = BTreeSet::new();
    fixtures(&golden(), "", &mut present);
    assert_eq!(
        listed, present,
        "MANIFEST.sha256 lists exactly the fixtures"
    );
    assert!(
        wrong.is_empty(),
        "changed since the manifest was written: {wrong:?} \
         (these files are synced from notes-core; run scripts/sync-golden.sh)"
    );
    let sorted: Vec<&str> = manifest.lines().map(|line| &line[66..]).collect();
    assert!(
        sorted.windows(2).all(|pair| pair[0] < pair[1]),
        "manifest is sorted"
    );
}

/// SHA-256 (FIPS 180-4), so the manifest needs no extra dependency.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut data = bytes.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for block in data.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *state = state.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[test]
fn sha256_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    assert_eq!(
        sha256_hex(&[b'a'; 1000]),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}
