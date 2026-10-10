//! 簡約方式と生 DOM 方式の同一タスクセット比較実行ハーネス（TASK-22.2・`AISNAP-9`・Issue #125・`MS-2`）。
//!
//! 比較実験の「実行支援」と「回収検査」を担う。LLM は呼ばない（API 経由の自動実行は採らない。
//! #123 のオーナー判断）。回答者は独立したサブエージェントによるブラインドの手動実行で、
//! 本モジュールは次の 3 点を提供する。
//!
//! 1. 割付表（[`allocation`]）: タスクごとに「先に実行する方式」を入れ替える。
//! 2. 実行パケット（[`packets`]・[`write_packets`]）: 指示テンプレート + タスク文 + ページ表現
//!    を 1 実行（1 タスク × 1 方式）ごとの 1 ファイルにまとめる。回答者へはこの本文だけを渡す。
//! 3. 回答回収と完全性検査（[`collect`]）: 保存された回答を読み、25 件 × 2 方式が揃ったかを
//!    構造体で返す。結合 JSON は既存の採点入口（`AGENT_EVAL_ANSWERS` / `AGENT_EVAL_RAW_ANSWERS`）へ
//!    そのまま渡せる。
//!
//! # 呼び出し元と責務境界
//!
//! - 呼び出し元: `compare_tests.rs`。後続の #126（TASK-22.3）が同ファイルへ差分算出・レポート生成を
//!   追記する。2 方式の差・−5pt 判定・和集合分母の揃えは本モジュールでは行わない。
//! - 取り込み側ルートが `tasks`・`generate_reduced`・`score` を同名 mod で `#[path]` 宣言している前提で
//!   `crate::` 参照する。
//!
//! # 暫定事項（REPAIR-3）
//!
//! - 実回答の収集は手動。本モジュールの完全性検査が通ることは「25 × 2 の回答が揃った」ことしか
//!   意味せず、成功率の比較結果ではない。
//! - 指示テンプレートは日本語。エージェントへの入力データであり、プログラムの出力文字列ではない
//!   （タスク文 `prompt` が日本語であることに揃える）。
//!
//! # 成果物の置き場
//!
//! パケット・回答・結合 JSON はコミットしない。[`write_packets`] と [`write_merged`] は出力先が
//! リポジトリ配下なら拒否する（誤コミット防止。fail-closed）。

use crate::generate_reduced::{GenerateError, InputMode, task_inputs};
use crate::score::{
    Answer, MAX_INPUT_BYTES, RawAnswer, ScoreError, parse_answers, parse_raw_answers,
};
use crate::tasks::{Category, TASKS, Task};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// 回答ファイル 1 件の読み取り上限。25 件分を結合しても `score::MAX_INPUT_BYTES` に収まる値。
pub const MAX_ANSWER_FILE_BYTES: usize = 32 * 1024;

const _: () = assert!(MAX_ANSWER_FILE_BYTES * 25 <= MAX_INPUT_BYTES);

/// 比較ハーネスの失敗。
#[derive(Debug)]
pub enum CompareError {
    /// 入力生成（`generate_reduced`）の失敗。
    Generate(GenerateError),
    /// 出力先の作成・書き込みの失敗。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 出力先がリポジトリ配下・相対パス等で拒否された。
    OutDirRejected(String),
}

impl fmt::Display for CompareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generate(e) => write!(f, "input generation failed: {e}"),
            Self::Io { path, source } => write!(f, "cannot write {}: {source}", path.display()),
            Self::OutDirRejected(r) => write!(f, "output directory rejected: {r}"),
        }
    }
}

impl std::error::Error for CompareError {}

impl From<GenerateError> for CompareError {
    fn from(e: GenerateError) -> Self {
        Self::Generate(e)
    }
}

// ---- 割付表 ----

/// 割付表の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub id: &'static str,
    pub category: Category,
    pub page: &'static str,
    /// 先に実行する方式。
    pub first: InputMode,
    /// 50 実行の通し番号（先に実行する方式, 後に実行する方式）。1 始まり。
    pub run_order: [u32; 2],
}

impl Assignment {
    /// 後に実行する方式。
    pub fn second(&self) -> InputMode {
        other_mode(self.first)
    }
}

fn other_mode(m: InputMode) -> InputMode {
    match m {
        InputMode::Reduced => InputMode::RawDom,
        InputMode::RawDom => InputMode::Reduced,
    }
}

/// JSON・ファイル名で使う ASCII 識別子。
fn mode_key(m: InputMode) -> &'static str {
    match m {
        InputMode::Reduced => "reduced",
        InputMode::RawDom => "raw",
    }
}

fn mode_label(m: InputMode) -> &'static str {
    match m {
        InputMode::Reduced => "簡約",
        InputMode::RawDom => "生 DOM",
    }
}

/// 割付表を作る（`AISNAP-9`・設計書「割付表」）。golden は受け取らない。
///
/// id 昇順に並べ、全体の通し位置が偶数なら簡約先・奇数なら生 DOM 先とする。種別ごとに交互に
/// 割り当て、奇数個の種別の余りを次の種別の先頭から続ける規則と同値で、全体 13 対 12・
/// 種別内の偏りは最大 1 件になる。入力順には依存しない。
pub fn allocation(tasks: &[Task]) -> Vec<Assignment> {
    let mut sorted: Vec<&Task> = tasks.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(b.id));
    sorted
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let first = if i % 2 == 0 {
                InputMode::Reduced
            } else {
                InputMode::RawDom
            };
            let base = u32::try_from(i).unwrap_or(u32::MAX).saturating_mul(2);
            Assignment {
                id: t.id,
                category: t.category,
                page: t.page,
                first,
                run_order: [base.saturating_add(1), base.saturating_add(2)],
            }
        })
        .collect()
}

/// 割付表を JSON（英語キー）で返す。
pub fn render_allocation_json(rows: &[Assignment]) -> String {
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "category": r.category.as_str(),
                "page": r.page,
                "first": mode_key(r.first),
                "second": mode_key(r.second()),
                "run_order": r.run_order,
            })
        })
        .collect();
    let mut s = serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_owned());
    s.push('\n');
    s
}

/// 割付表を Markdown 表で返す（#126 のレポート付録へ転記できる形）。
pub fn render_allocation_markdown(rows: &[Assignment]) -> String {
    let mut s = String::from(
        "| タスク id | 種別 | ページ | 先に提示する方式 | 実行番号（先, 後） |\n\
         | --------- | ---- | ------ | ---------------- | ------------------ |\n",
    );
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {}, {} |\n",
            r.id,
            r.category.as_str(),
            r.page,
            mode_label(r.first),
            r.run_order[0],
            r.run_order[1]
        ));
    }
    s
}

// ---- 指示テンプレート ----

/// 生 DOM 方式で回答に使ってよいセレクタ構文の例（core `selector.rs` の対応サブセット）。
/// テンプレートの記載と core 実装の乖離をテストで検出するため、テンプレートはこの定数から組む。
pub const RAW_ALLOWED_SELECTOR_EXAMPLES: [&str; 10] = [
    "button",
    "#login",
    ".item",
    "[disabled]",
    "[type=submit]",
    "[type=\"submit\"]",
    "a[href^=\"/docs\"]",
    "form#login input",
    "ul > li",
    "h1, h2",
];

/// 使用できない構文の例（core が未対応で、回答すると採点上 `invalid_shape` になる）。
pub const RAW_FORBIDDEN_SELECTOR_EXAMPLES: [&str; 9] = [
    "*",
    "a:first-child",
    "li::before",
    "h1 + p",
    "h1 ~ p",
    "a[href$=\".pdf\"]",
    "a[href*=\"docs\"]",
    "a[class~=\"item\"]",
    "a[lang|=\"en\"]",
];

/// `select` の `value` の説明。両方式で同一文言にして、方式間のバイアスを避ける。
pub const SELECT_VALUE_NOTE: &str = "action が select のときの value は、選ぶ option 要素の value 属性値を書く（画面に表示されるラベルではない）。";

fn answer_example(mode: InputMode, task: &Task) -> (String, String) {
    let target = match mode {
        InputMode::Reduced => "\"ref\": \"<ref>\"",
        InputMode::RawDom => "\"selector\": \"<selector>\", \"index\": 0",
    };
    let target_null = match mode {
        InputMode::Reduced => "\"ref\": null",
        InputMode::RawDom => "\"selector\": null, \"index\": null",
    };
    let id = task.id;
    match task.category {
        Category::Click | Category::Nav => (
            format!("{{\"id\": \"{id}\", {target}}}"),
            format!("{{\"id\": \"{id}\", {target_null}}}"),
        ),
        Category::Extract => (
            format!("{{\"id\": \"{id}\", \"value\": \"<抽出した値>\"}}"),
            format!("{{\"id\": \"{id}\", \"value\": null}}"),
        ),
        Category::Form => (
            format!(
                "{{\"id\": \"{id}\", \"steps\": [\n  {{\"action\": \"fill\", {target}, \"value\": \"<入力する値>\"}},\n  {{\"action\": \"select\", {target}, \"value\": \"<option の value 属性値>\"}},\n  {{\"action\": \"click\", {target}}}\n]}}"
            ),
            format!("{{\"id\": \"{id}\", \"steps\": null}}"),
        ),
    }
}

/// 方式別の指示テンプレートを返す（`AISNAP-9`。設計書「サブエージェントへの指示テンプレート」）。
///
/// golden・実験の意図（方式の比較であること・判定式）・他方式への言及は含めない。
pub fn instruction(mode: InputMode, task: &Task) -> String {
    let (repr, how) = match mode {
        InputMode::Reduced => (
            "ページの簡約表現（アクセシビリティツリー形式のテキスト）",
            "要素は、表現中の行末付近にある角括弧 [...] の中の文字列（ref）で指す。括弧は含めず、`-2` のような接尾辞も含めてそのまま書く。",
        ),
        InputMode::RawDom => (
            "ページの HTML（body の内容）",
            "要素は {\"selector\": CSS セレクタ, \"index\": 整数} で指す。index は、与えられた HTML 全体でそのセレクタに一致する要素を文書順に並べたときの 0 始まりの位置。",
        ),
    };
    let (example, null_example) = answer_example(mode, task);
    let mut s = String::new();
    s.push_str("あなたは Web ページ上の操作を判断するエージェントです。\n");
    s.push_str(&format!(
        "入力: 1 つの{repr}と、タスク文 1 件。\n出力: 下記の JSON オブジェクト 1 件のみ（前後の説明文・コードフェンスは付けない）。\n\n"
    ));
    s.push_str("規則:\n");
    s.push_str(
        "- 与えられた表現だけを根拠に答える。それ以外の資料・ファイル・ページを参照しない。\n",
    );
    s.push_str("- 表現から特定できない場合は推測せず、回答不能（null）と答える。\n");
    s.push_str(&format!("- {how}\n"));
    s.push_str("- クリックする要素・遷移先のリンクを問われたら要素 1 つを答える。値を問われたら value に文字列で答える。入力手順を問われたら steps に操作を順に並べる。\n");
    s.push_str("- steps の fill と select は value が必須。click は value を持たない。\n");
    s.push_str(&format!("- {SELECT_VALUE_NOTE}\n"));
    if mode == InputMode::RawDom {
        s.push_str("- 使用できるセレクタ構文: 型セレクタ・#id・.class・[属性]・[属性=値]（値は引用符あり/なし）・[属性^=値]・子孫結合子（空白）・子結合子 >・カンマ区切り。\n");
        s.push_str(&format!(
            "  使用できる例: {}\n",
            RAW_ALLOWED_SELECTOR_EXAMPLES
                .iter()
                .map(|e| format!("`{e}`"))
                .collect::<Vec<_>>()
                .join("、")
        ));
        s.push_str(&format!(
            "- 使用できない構文（回答が無効になる）: {}\n",
            RAW_FORBIDDEN_SELECTOR_EXAMPLES
                .iter()
                .map(|e| format!("`{e}`"))
                .collect::<Vec<_>>()
                .join("、")
        ));
        s.push_str("  疑似クラス・疑似要素・兄弟結合子・属性値の部分一致（$= *= ~= |=）・ワイルドカード・エスケープは使えない。\n");
    }
    s.push_str(&format!(
        "\n回答形式:\n```json\n{example}\n```\n\n回答不能の場合:\n```json\n{null_example}\n```\n"
    ));
    s
}

// ---- 実行パケット ----

/// 1 実行（1 タスク × 1 方式）分の回答者向けパケット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub id: &'static str,
    pub mode: InputMode,
    /// 出力先直下のファイル名（`<id>-reduced.txt` / `<id>-raw.txt`）。
    pub file_name: String,
    /// 回答者へ渡す本文（指示 + タスク文 + ページ表現）。
    pub body: String,
}

fn packet_file_name(id: &str, mode: InputMode) -> String {
    format!("{id}-{}.txt", mode_key(mode))
}

fn answer_file_name(id: &str, mode: InputMode) -> String {
    format!("{id}-{}.json", mode_key(mode))
}

/// 2 方式 × 25 タスクのパケットを組み立てる（`TASKS` 順に Reduced 25 件、続けて RawDom 25 件）。
pub fn packets(fixtures_dir: &Path) -> Result<Vec<Packet>, CompareError> {
    let mut out = Vec::with_capacity(TASKS.len() * 2);
    for mode in [InputMode::Reduced, InputMode::RawDom] {
        let inputs = task_inputs(fixtures_dir, mode)?;
        for (t, inp) in TASKS.iter().zip(inputs) {
            let label = match mode {
                InputMode::Reduced => "ページ表現（簡約）",
                InputMode::RawDom => "ページ表現（HTML）",
            };
            let body = format!(
                "{}\n## タスク\n{}\n\n## {label}\n{}\n",
                instruction(mode, t),
                inp.prompt,
                inp.input
            );
            out.push(Packet {
                id: t.id,
                mode,
                file_name: packet_file_name(t.id, mode),
                body,
            });
        }
    }
    Ok(out)
}

/// リポジトリルート（`CARGO_MANIFEST_DIR` の 2 階層上）。
fn repo_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .and_then(|p| p.canonicalize().ok())
}

/// 存在する最も深い祖先を正規化し、残りを連結する（未作成の出力先でもシンボリックリンクを解決する）。
fn canonical_lossy(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut p = c;
            for r in rest.iter().rev() {
                p.push(r);
            }
            return p;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// 出力先の安全弁（純関数）。絶対パスで `..` を含まず、`repo_root` 配下でないことを要求する。
pub fn check_out_dir(out_dir: &Path, repo_root: &Path) -> Result<(), CompareError> {
    if !out_dir.is_absolute() {
        return Err(CompareError::OutDirRejected(
            "must be an absolute path".to_owned(),
        ));
    }
    if out_dir.components().any(|c| c == Component::ParentDir) {
        return Err(CompareError::OutDirRejected(
            "must not contain '..'".to_owned(),
        ));
    }
    // 両辺を同じ正規化（シンボリックリンク・8.3 短縮名・UNC 接頭辞の解決）に通してから比較する
    if canonical_lossy(out_dir).starts_with(canonical_lossy(repo_root)) {
        return Err(CompareError::OutDirRejected(
            "must be outside the repository (generated files and answers are never committed)"
                .to_owned(),
        ));
    }
    Ok(())
}

fn check_out_dir_here(out_dir: &Path) -> Result<(), CompareError> {
    // ルートを解決できないときは安全側（拒否）に倒す
    let Some(root) = repo_root() else {
        return Err(CompareError::OutDirRejected(
            "cannot resolve repository root".to_owned(),
        ));
    };
    check_out_dir(out_dir, &root)
}

/// 出力ファイルを書く。既存のシンボリックリンクは追跡せず拒否する（リポジトリ外制約の迂回防止）。
fn write_file(path: &Path, content: &str) -> Result<(), CompareError> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(CompareError::OutDirRejected(format!(
            "output path is a symbolic link: {}",
            path.display()
        )));
    }
    std::fs::write(path, content.as_bytes()).map_err(|source| CompareError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn create_dir(out_dir: &Path) -> Result<(), CompareError> {
    std::fs::create_dir_all(out_dir).map_err(|source| CompareError::Io {
        path: out_dir.to_path_buf(),
        source,
    })
}

/// パケット 50 件と割付表（`allocation.json`・`allocation.md`）を `out_dir` 直下へ書き、書いたパスを返す。
///
/// `out_dir` はリポジトリ外の絶対パスに限る（[`check_out_dir`]）。割付表は回答者へ渡さない。
pub fn write_packets(fixtures_dir: &Path, out_dir: &Path) -> Result<Vec<PathBuf>, CompareError> {
    check_out_dir_here(out_dir)?;
    let all = packets(fixtures_dir)?;
    let rows = allocation(&TASKS);
    create_dir(out_dir)?;
    let mut paths = Vec::with_capacity(all.len() + 2);
    for p in &all {
        let path = out_dir.join(&p.file_name);
        write_file(&path, &p.body)?;
        paths.push(path);
    }
    for (name, content) in [
        ("allocation.json", render_allocation_json(&rows)),
        ("allocation.md", render_allocation_markdown(&rows)),
    ] {
        let path = out_dir.join(name);
        write_file(&path, &content)?;
        paths.push(path);
    }
    Ok(paths)
}

// ---- 回答の回収と完全性検査 ----

/// 読めない・JSON 不正・id 不一致などで受理しなかった回答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub id: String,
    pub reason: String,
}

/// 1 方式分の回収状況。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MethodStatus {
    /// 回答ファイルが無いタスク id。
    pub missing: Vec<String>,
    /// 受理しなかった回答（理由付き）。
    pub rejected: Vec<Rejected>,
    /// 明示的に回答不能（null）とされたタスク id。プロトコル上は正当な回答として数える。
    pub unanswered: Vec<String>,
    /// 形式不備（採点時 `invalid_shape`）の回答。回収は成立しているため完全性には影響しない。
    pub invalid: Vec<String>,
    /// 受理した件数。
    pub accepted: usize,
}

/// 両方式の回収状況。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollectStatus {
    pub reduced: MethodStatus,
    pub raw: MethodStatus,
}

impl CollectStatus {
    /// 両方式とも `missing`・`rejected` が 0（25 件 × 2 方式が揃った）か。
    pub fn is_complete(&self) -> bool {
        [&self.reduced, &self.raw]
            .iter()
            .all(|m| m.missing.is_empty() && m.rejected.is_empty())
    }
}

/// 回収結果。#126 が差分算出の入力として直接受け取る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
    pub reduced: BTreeMap<String, Answer>,
    pub raw: BTreeMap<String, RawAnswer>,
    /// 簡約方式の結合 JSON（`AGENT_EVAL_ANSWERS` へ渡せる）。
    pub reduced_json: String,
    /// 生 DOM 方式の結合 JSON（`AGENT_EVAL_RAW_ANSWERS` へ渡せる）。
    pub raw_json: String,
    pub status: CollectStatus,
}

/// 回答ファイルを上限付きで読む。`Ok(None)` はファイル無し。
fn read_limited(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot open: {e}")),
    };
    let mut buf = Vec::new();
    file.take(MAX_ANSWER_FILE_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read: {e}"))?;
    if buf.len() > MAX_ANSWER_FILE_BYTES {
        return Err("file exceeds size limit".to_owned());
    }
    Ok(Some(buf))
}

type ParseFn<T> = fn(&[Task], &str) -> Result<BTreeMap<String, T>, ScoreError>;

/// 1 方式分を回収する。読むファイル名は `TASKS` の id と方式から作る定数由来のみ。
fn collect_mode<T: Clone>(
    answers_dir: &Path,
    mode: InputMode,
    parse: ParseFn<T>,
    is_unanswered: fn(&T) -> bool,
    is_invalid: fn(&T) -> bool,
) -> (BTreeMap<String, T>, String, MethodStatus) {
    let mut st = MethodStatus::default();
    let mut map = BTreeMap::new();
    let mut values = Vec::new();
    for t in &TASKS {
        let path = answers_dir.join(answer_file_name(t.id, mode));
        let reject = |st: &mut MethodStatus, reason: String| {
            st.rejected.push(Rejected {
                id: t.id.to_owned(),
                reason,
            })
        };
        let bytes = match read_limited(&path) {
            Ok(Some(b)) => b,
            Ok(None) => {
                st.missing.push(t.id.to_owned());
                continue;
            }
            Err(r) => {
                reject(&mut st, r);
                continue;
            }
        };
        let Ok(text) = String::from_utf8(bytes) else {
            reject(&mut st, "not valid UTF-8".to_owned());
            continue;
        };
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                reject(&mut st, format!("invalid JSON: {e}"));
                continue;
            }
        };
        let id_ok = value
            .as_object()
            .map(|o| o.get("id").and_then(|i| i.as_str()) == Some(t.id));
        match id_ok {
            None => {
                reject(&mut st, "top level must be an object".to_owned());
                continue;
            }
            Some(false) => {
                reject(&mut st, "id does not match the file name".to_owned());
                continue;
            }
            Some(true) => {}
        }
        // 既存の採点入口と同じ検証（上限・形式）を 1 件ずつ通す
        let single = serde_json::Value::Array(vec![value.clone()]).to_string();
        let parsed = match parse(&TASKS, &single) {
            Ok(p) => p,
            Err(e) => {
                reject(&mut st, e.to_string());
                continue;
            }
        };
        let Some(ans) = parsed.get(t.id) else {
            reject(&mut st, "answer not parsed".to_owned());
            continue;
        };
        if is_unanswered(ans) {
            st.unanswered.push(t.id.to_owned());
        }
        if is_invalid(ans) {
            st.invalid.push(t.id.to_owned());
        }
        st.accepted += 1;
        map.insert(t.id.to_owned(), ans.clone());
        values.push(value);
    }
    // 採点入口の MAX_INPUT_BYTES を超えないよう、整形せずコンパクトに直列化する
    let json = serde_json::Value::Array(values).to_string();
    (map, format!("{json}\n"), st)
}

/// `answers_dir` の `<id>-reduced.json` / `<id>-raw.json`（各 JSON オブジェクト 1 件）を回収する。
///
/// 欠落・不正は `status` に記録し、全体のエラーにはしない（`is_complete()` で判定する）。
/// 期待する 50 個のファイル名以外は読まない。
pub fn collect(answers_dir: &Path) -> Collected {
    let (reduced, reduced_json, rs) = collect_mode(
        answers_dir,
        InputMode::Reduced,
        parse_answers,
        |a| matches!(a, Answer::Unanswered),
        |a| matches!(a, Answer::Invalid),
    );
    let (raw, raw_json, ws) = collect_mode(
        answers_dir,
        InputMode::RawDom,
        parse_raw_answers,
        |a| matches!(a, RawAnswer::Unanswered),
        |a| matches!(a, RawAnswer::Invalid),
    );
    Collected {
        reduced,
        raw,
        reduced_json,
        raw_json,
        status: CollectStatus {
            reduced: rs,
            raw: ws,
        },
    }
}

/// 回収状況を JSON（英語キー）で返す。
pub fn render_status_json(status: &CollectStatus) -> String {
    let m = |s: &MethodStatus| {
        serde_json::json!({
            "accepted": s.accepted,
            "missing": s.missing,
            "rejected": s.rejected.iter().map(|r| serde_json::json!({"id": r.id, "reason": r.reason})).collect::<Vec<_>>(),
            "unanswered": s.unanswered,
            "invalid": s.invalid,
        })
    };
    let v = serde_json::json!({
        "complete": status.is_complete(),
        "reduced": m(&status.reduced),
        "raw": m(&status.raw),
    });
    let mut s = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_owned());
    s.push('\n');
    s
}

/// 結合 JSON 2 本（`answers-reduced.json`・`answers-raw.json`）を `out_dir` へ書く。
/// 出力先の制約は [`write_packets`] と同じ。
pub fn write_merged(c: &Collected, out_dir: &Path) -> Result<Vec<PathBuf>, CompareError> {
    check_out_dir_here(out_dir)?;
    create_dir(out_dir)?;
    let mut paths = Vec::new();
    for (name, content) in [
        ("answers-reduced.json", &c.reduced_json),
        ("answers-raw.json", &c.raw_json),
    ] {
        let path = out_dir.join(name);
        write_file(&path, content)?;
        paths.push(path);
    }
    Ok(paths)
}
