//! `wpt-subset.json`（WPT サブセット定義）のスキーマ契約テスト（`PLUG-10`・TASK-101.6・Issue #278）。
//!
//! ランナー（`runner::parse_subset_tsv`）と取得スクリプト（`fetch-wpt.sh`）が前提にする
//! 構造を、実ファイルに対して検証する。JSON パーサーは依存追加になるため使わず、整形済み JSON
//! （1 フィールド 1 行・2 スペースインデント）を行走査で読む（`src/report.rs` のテストと同じ流儀）。
//!
//! 検証範囲: サイズ・件数の上限、`schemaVersion`、`totalSelected` と `subset` 件数の一致、
//! `harnessBreakdown` との一致、`file` のパス規則・一意性（ランナーと同じ `parse_subset_tsv`）、
//! `dir` と `perDirectorySummary` の整合、`wptRevision` の形式。
//!
//! 行走査は JSON 構文を検証しないため、依存なしの最小 JSON 構文検証器（`validate_json_syntax`。
//! RFC 8259 の再帰下降・深さ上限付き）を別途かけ、カンマ・括弧・引用符の欠落や末尾ゴミを検出する。
//!
//! 未検証: 各フィールドの JSON 型の厳密検証、`mappedFeatures` 配列要素、未知キーの検出
//! （構文検証のみで値は構築しないため。JSON パーサー依存の導入はユーザー承認事項）。

use wpt_subset_runner::HarnessKind;
use wpt_subset_runner::runner::parse_subset_tsv;

const SUBSET_JSON: &str = include_str!("../wpt-subset.json");

/// README の「件数上限」に合わせた定義ファイルの上限。
const MAX_BYTES: usize = 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;

#[derive(Debug)]
struct SubsetItem {
    file: String,
    dir: String,
    harness: String,
}

#[derive(Debug, Default)]
struct Parsed {
    schema_version: Option<u64>,
    total_selected: Option<usize>,
    wpt_revision: Option<String>,
    subset: Vec<SubsetItem>,
    /// `perDirectorySummary` の (dir, picked)。
    per_dir: Vec<(String, usize)>,
    /// `harnessBreakdown` の (harness, 件数)。
    breakdown: Vec<(String, usize)>,
}

/// `"key": "value"`（末尾のカンマは任意）の行から文字列値を取り出す。
fn str_field(line: &str, key: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(&format!("\"{key}\": \""))?;
    let rest = rest
        .strip_suffix("\",")
        .or_else(|| rest.strip_suffix('"'))?;
    Some(rest.to_string())
}

/// `"key": 123`（末尾のカンマは任意）の行から非負整数を取り出す。
fn num_field(line: &str, key: &str) -> Option<u64> {
    let rest = line.trim().strip_prefix(&format!("\"{key}\": "))?;
    rest.strip_suffix(',').unwrap_or(rest).parse().ok()
}

#[derive(PartialEq, Clone, Copy)]
enum Section {
    Other,
    Source,
    PerDir,
    Subset,
    Breakdown,
}

fn parse(json: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut section = Section::Other;
    let mut file: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut per_dir_name: Option<String> = None;
    for line in json.lines() {
        // トップレベルのキー（インデント 2）でセクションを切り替える。
        if line.starts_with("  \"") {
            section = if line.starts_with("  \"source\"") {
                Section::Source
            } else if line.starts_with("  \"perDirectorySummary\"") {
                Section::PerDir
            } else if line.starts_with("  \"subset\"") {
                Section::Subset
            } else if line.starts_with("  \"harnessBreakdown\"") {
                Section::Breakdown
            } else {
                Section::Other
            };
            if let Some(v) = num_field(line, "schemaVersion") {
                out.schema_version = Some(v);
            }
            if let Some(v) = num_field(line, "totalSelected") {
                out.total_selected = usize::try_from(v).ok();
            }
            continue;
        }
        match section {
            Section::Source => {
                if let Some(v) = str_field(line, "wptRevision") {
                    out.wpt_revision = Some(v);
                }
            }
            Section::PerDir => {
                if let Some(v) = str_field(line, "dir") {
                    per_dir_name = Some(v);
                } else if let Some(v) = num_field(line, "picked")
                    && let Some(name) = per_dir_name.take()
                {
                    out.per_dir
                        .push((name, usize::try_from(v).expect("picked fits usize")));
                }
            }
            Section::Subset => {
                if let Some(v) = str_field(line, "file") {
                    file = Some(v);
                } else if let Some(v) = str_field(line, "dir") {
                    dir = Some(v);
                } else if let Some(h) = str_field(line, "harness")
                    && let Some(f) = file.take()
                    && let Some(d) = dir.take()
                {
                    out.subset.push(SubsetItem {
                        file: f,
                        dir: d,
                        harness: h,
                    });
                }
            }
            Section::Breakdown => {
                let t = line.trim();
                if let Some((k, v)) = t.split_once(": ")
                    && let Ok(n) = v.trim_end_matches(',').parse::<usize>()
                {
                    out.breakdown.push((k.trim_matches('"').to_string(), n));
                }
            }
            Section::Other => {}
        }
    }
    out
}

/// 依存なしの最小 JSON 構文検証器（値は構築しない）。深さ上限 64 で再帰を抑える。
struct Syntax<'a> {
    b: &'a [u8],
    i: usize,
}

impl Syntax<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn lit(&mut self, w: &str) -> Result<(), String> {
        if self.b.get(self.i..self.i + w.len()) == Some(w.as_bytes()) {
            self.i += w.len();
            Ok(())
        } else {
            Err(format!("bad literal at {}", self.i))
        }
    }

    fn string(&mut self) -> Result<(), String> {
        self.i += 1; // 開始の引用符
        loop {
            match self.b.get(self.i) {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(b'\\') => match self.b.get(self.i + 1) {
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 2,
                    Some(b'u')
                        if self
                            .b
                            .get(self.i + 2..self.i + 6)
                            .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) =>
                    {
                        self.i += 6
                    }
                    _ => return Err(format!("bad escape at {}", self.i)),
                },
                Some(c) if *c < 0x20 => return Err(format!("control char at {}", self.i)),
                Some(_) => self.i += 1,
            }
        }
    }

    fn digits(&mut self) -> bool {
        let st = self.i;
        while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i > st
    }

    fn number(&mut self) -> Result<(), String> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        if !self.digits() {
            return Err(format!("bad number at {start}"));
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !self.digits() {
                return Err(format!("bad fraction at {start}"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !self.digits() {
                return Err(format!("bad exponent at {start}"));
            }
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<(), String> {
        if depth > 64 {
            return Err("too deep".into());
        }
        self.ws();
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return Err(format!("expected key at {}", self.i));
                    }
                    self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return Err(format!("expected ':' at {}", self.i));
                    }
                    self.i += 1;
                    self.value(depth + 1)?;
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return Err(format!("expected ',' or '}}' at {}", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.value(depth + 1)?;
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return Err(format!("expected ',' or ']' at {}", self.i)),
                    }
                }
            }
            Some(b'"') => self.string(),
            Some(b't') => self.lit("true"),
            Some(b'f') => self.lit("false"),
            Some(b'n') => self.lit("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("unexpected token at {}", self.i)),
        }
    }
}

/// JSON 構文（括弧の対応・カンマ区切り・引用符・末尾ゴミ）を検証する。
fn validate_json_syntax(json: &str) -> Result<(), String> {
    let mut s = Syntax {
        b: json.as_bytes(),
        i: 0,
    };
    s.value(0)?;
    s.ws();
    if s.i == s.b.len() {
        Ok(())
    } else {
        Err(format!("trailing data at {}", s.i))
    }
}

#[test]
fn plug_10_subset_json_is_syntactically_valid() {
    validate_json_syntax(SUBSET_JSON).expect("wpt-subset.json is valid JSON");
}

#[test]
fn plug_10_syntax_validator_rejects_broken_json() {
    // 行走査だけなら素通りしうる破損（区切りカンマ・閉じ括弧の欠落）を検出できること。
    let missing_comma = SUBSET_JSON.replacen("\",\n", "\"\n", 1);
    assert_ne!(missing_comma, SUBSET_JSON);
    assert!(validate_json_syntax(&missing_comma).is_err());
    let missing_close = SUBSET_JSON.trim_end().trim_end_matches('}').to_string();
    assert!(validate_json_syntax(&missing_close).is_err());
    assert!(validate_json_syntax(&format!("{SUBSET_JSON} x")).is_err());
    assert!(validate_json_syntax("{\"a\": [1, 2,]}").is_err());
    assert!(validate_json_syntax("{\"a\": [1, {\"b\": null}], \"c\": -1.5e3}").is_ok());
}

#[test]
fn plug_10_subset_json_within_size_and_entry_limits() {
    assert!(
        SUBSET_JSON.len() <= MAX_BYTES,
        "{} bytes",
        SUBSET_JSON.len()
    );
    let parsed = parse(SUBSET_JSON);
    assert!(parsed.subset.len() <= MAX_ENTRIES);
}

#[test]
fn plug_10_subset_json_schema_version_and_revision() {
    let parsed = parse(SUBSET_JSON);
    assert_eq!(parsed.schema_version, Some(1));
    let rev = parsed.wpt_revision.expect("wptRevision present");
    assert_eq!(rev.len(), 40, "{rev}");
    assert!(
        rev.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "{rev}"
    );
}

#[test]
fn plug_10_subset_json_total_matches_entries_and_breakdown() {
    let parsed = parse(SUBSET_JSON);
    assert_eq!(parsed.total_selected, Some(257));
    assert_eq!(parsed.subset.len(), 257);
    let count = |h: &str| parsed.subset.iter().filter(|s| s.harness == h).count();
    assert_eq!(
        parsed.breakdown,
        [
            ("testharness".to_string(), 152),
            ("reftest".to_string(), 88),
            ("other".to_string(), 17)
        ]
    );
    for (h, n) in &parsed.breakdown {
        assert_eq!(count(h), *n, "{h}");
    }
}

#[test]
fn plug_10_subset_json_files_pass_runner_validation() {
    let parsed = parse(SUBSET_JSON);
    // 全エントリのハーネス種別がランナーの既知値であること。
    for s in &parsed.subset {
        assert!(HarnessKind::parse(&s.harness).is_some(), "{s:?}");
        assert!(!s.file.starts_with('/'), "{s:?}");
    }
    // ランナーと同じ実装で、文字種・`..`・重複・上限を検証する。
    let tsv: String = parsed
        .subset
        .iter()
        .map(|s| format!("{}\t{}\n", s.harness, s.file))
        .collect();
    let entries = parse_subset_tsv(&tsv).expect("runner accepts every subset entry");
    assert_eq!(entries.len(), 257);
}

#[test]
fn plug_10_subset_json_dirs_are_consistent() {
    let parsed = parse(SUBSET_JSON);
    for s in &parsed.subset {
        assert!(s.file.starts_with(&format!("{}/", s.dir)), "{s:?}");
        assert!(parsed.per_dir.iter().any(|(d, _)| *d == s.dir), "{s:?}");
    }
    let mut total = 0;
    for (d, picked) in &parsed.per_dir {
        let n = parsed.subset.iter().filter(|s| s.dir == *d).count();
        assert_eq!(n, *picked, "picked mismatch for {d}");
        total += picked;
    }
    assert_eq!(total, 257);
}
