//! `snapshot::Node::name` フィールドの算出ロジック（`AISNAP-1`・`TASK-11.4`・
//! `MS-2`）。
//!
//! 本ファイル（TASK-11.4.2・Issue #545）が実装するのは、accessible name の
//! 出所のうち **HTML ネイティブのラベル付け**（`alt`・`title`・`value`・
//! `placeholder`・submit/reset/image の既定ラベル・`label[for]`・label に
//! よる包含）のみである。以下は本 Issue のスコープ外とし、後続タスクへ
//! 引き継ぐ（実装済みを装わない。REPAIR-3）。
//!
//! - ARIA 属性による明示的な命名（`aria-labelledby`・`aria-label`）:
//!   TASK-11.4.1（Issue #544）
//! - 子孫テキストからの名前（`button`・`a`・見出しの内容）・
//!   `hidden`/`aria-hidden` 子孫の除外・埋め込みコントロールの値・
//!   全体の優先順位統合・文書ルートの `<title>` による命名:
//!   TASK-11.4.3（Issue #546）
//! - `fieldset`→`legend`・`table`→`caption`・`figure`→`figcaption`・SVG の
//!   `<title>`・`aria-describedby`: 担当 Issue 未確定（out-of-scope-tracking
//!   に従いユーザー承認を得てから追跡する）
//! - `id` 索引化による走査コスト削減・DOM から `Snapshot`/`Node` への
//!   ツリー構築配線: TASK-11.7（Issue #76）
//!
//! 本モジュールは #544（TASK-11.4.1）と同じ公開型の形（`AccessibleName`・
//! `NameSource`・`NameBuffer` の構造）に揃えてある。両 Issue は同一ファイル
//! （`name.rs`）を独立に新規追加するため、マージ時に add/add コンフリクトが
//! 起きる見込みである。解消は後からマージする側が rebase して行い、ARIA と
//! ネイティブの優先順位統合は TASK-11.4.3（Issue #546）が担う（本ファイルは
//! `NameSource::AriaLabel`/`AriaLabelledBy` を追加しない。#544 の成果を
//! 装わないため）。
//!
//! 呼び出し文脈: 現時点では呼び出し元がない。DOM から `Snapshot`/`Node` を
//! 構築する TASK-11.7（Issue #76）が、ツリー構築時に要素ごとへ
//! [`compute_name`] を呼ぶ想定である（実装済みを装わない。REPAIR-3）。
//!
//! # HTML-AAM による要素ごとの算出順序
//!
//! 出典: [HTML Accessibility API Mappings — Accessible Name Computations By
//! HTML Element](https://w3c.github.io/html-aam/#accessible-name-and-description-computation)
//! （ソース: <https://github.com/w3c/aria> の `html-aam/index.html`。各節の
//! 見出しを下表に併記する）。下表は ARIA（`aria-label`/`aria-labelledby`）の
//! 段を省いてある（TASK-11.4.1・#544 の担当）。
//!
//! | 要素 | 節見出し | 出所の順序（ARIA を除く） | 空・空白だけの属性値 |
//! | ---- | -------- | -------------------------- | -------------------- |
//! | `img` | `img` Element Accessible Name Computation | `alt` → （`alt` 属性が **無い場合のみ**）`title` → （どちらも無い場合の `figcaption` 経由は担当 Issue 未確定）→ 名前なし | **`alt` は値が空文字列（trim 後）でも確定して使う**（`title` へは落ちない）。仕様注記: 「An `img` with an `alt` attribute whose value ... is the empty string is mapped to the `presentation` role. It has no accessible name.」`title` へ落ちるのは `alt` 属性自体が無いときだけ |
//! | `area` | `area` Element Accessible Name Computation | `alt` → `title` → 名前なし | `img` と異なり "even if empty" の注記がないため、一般則（trim 後に空なら次点へ）に従う |
//! | `input` テキスト系（`text`/`password`/`search`/`tel`/`url`/`email`/`number`、`type` 省略・未知の値を含む） | `input type="text"`, ... `textarea` Elements Accessible Name Computation | label → `title` → `placeholder` → （`aria-placeholder` は ARIA・スコープ外）→ 名前なし | いずれも trim 後に空なら次点へ |
//! | `textarea` | 同上（`input` テキスト系と同じ節） | 同上 | 同上 |
//! | `input` `checkbox`/`radio`・`range`/`color`/`date`/`datetime-local`/`month`/`week`/`time`/`file`（text 系節に明示列挙されない他の type）・`select`・`meter`・`output`・`progress` | `Other Form Elements Accessible Name Computation`（`output` のみ専用節あり。同じ実質順序） | label → `title` → 名前なし（`placeholder` 段はない） | trim 後に空なら次点へ |
//! | `input` `button` | `input type="button"`, `input type="submit"` and `input type="reset"` Elements Accessible Name Computation | label → `value` → `title` → 名前なし（**`button` には既定ラベルの段がない**。仕様: 「For `input type=submit` and `type=reset`: ...」と `submit`/`reset` に限定） | `value` は trim 後に空でも「指定あり」として扱われ、次段（既定ラベル。`button` には無い）ではなく直接 `title` に進む |
//! | `input` `submit`/`reset` | 同上 | label → `value`（属性が**指定されている**場合。空文字列でも指定扱い） → （`value` 属性が**未指定のときだけ**）既定ラベル（`"Submit"`/`"Reset"`） → `title` → 名前なし | `value=""` は「指定あり」なので既定ラベル段を飛ばし `title` へ進む。`value` 属性が全く無いときだけ既定ラベルを使う |
//! | `input` `image` | `input type="image"` Element Accessible Name Computation | label → `alt`（trim 後に非空の場合のみ） → `title`（非空の場合のみ） → 既定ラベル `"Submit Query"` → 名前なし | `value` 属性は本節のアルゴリズムに含まれない（仕様のコメント注記は「もし規定されるなら」という将来の余地であり、現行の算出手順には無い。本実装も `value` を使わない） |
//! | `input` `hidden` | 本 spec に個別記載なし | 常に名前なし | `hidden` はアクセシビリティツリーへ公開されない要素のため、本実装の判断で常に空とする（W3C 未規定の部分） |
//! | `button`（`<button>` 要素） | `button` Element Accessible Name Computation | label → `title` → 名前なし（**子孫テキストへのフォールバックは #546 のスコープ**。本 Issue は実装しない） | trim 後に空なら次点へ |
//! | その他の要素 | 各種 Section/Grouping・Text-level 等の節（いずれも `title` のみ） | `title` → 名前なし | trim 後に空なら名前なし |
//!
//! ラベル関連付け（HTML Standard の labeled control 規則。上表の「label」段）
//! は `for` 属性（文書順で最初に一致する `id` を持つ要素が対象と同じ場合の
//! み）または label による包含（label の子孫のうち文書順で最初の
//! ラベル付け可能な要素が対象と同じ場合）で判定する。詳細は
//! [`label_name`] を参照。

use fandhe_browser_core::dom::{Document, NodeData, NodeId};

use super::state::is_html_element_named;

/// 組み立てる accessible name の文字数上限（`AISNAP-1`）。文字数
/// （`char`）で数え、バイト数では数えない（マルチバイト文字を含む名前を
/// 不当に短く切り詰めないため）。
///
/// 外部入力（HTML の属性値・テキスト）から無制限に文字列を組み立てない
/// ための上限（security.md「不安全な設計」対策）。
const MAX_NAME_CHARS: usize = 120;

/// 1 つのコントロールに関連付ける `<label>` の数の上限（`AISNAP-1`）。
///
/// 外部入力の HTML に大量の `<label for="...">` を並べられても、文書走査を
/// 定数個で打ち切るための上限（security.md「不安全な設計」対策）。超えた
/// 分は切り捨て、`truncated` へ反映する（黙って捨てない）。
const MAX_LABELS: usize = 16;

/// accessible name の出所（`AISNAP-1`）。
///
/// バリアントは本 Issue（TASK-11.4.2）が実装する HTML ネイティブの出所の
/// みである。ARIA 属性由来（TASK-11.4.1・#544）・子孫テキスト由来
/// （TASK-11.4.3・#546）の出所は、後続タスクが非破壊で追加する
/// （`#[non_exhaustive]`。REPAIR-4）。
///
/// `bool` ではなく enum にする理由: 名前の有無だけでなく「どの規則で
/// 決まったか」を AI エージェント・デバッグ双方に伝える必要があるため
/// （coding-rust.md「公開 API」・REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum NameSource {
    /// 出所なし（accessible name が空、または未算出）。
    #[default]
    None,
    /// `label`（`for` 属性による関連付け、または label による包含）により
    /// 算出した。
    Label,
    /// `alt` 属性により算出した。
    Alt,
    /// `value` 属性により算出した。
    Value,
    /// submit/reset/image の既定ラベル（`"Submit"`・`"Reset"`・
    /// `"Submit Query"`）により算出した。
    DefaultButtonLabel,
    /// `title` 属性により算出した。
    Title,
    /// `placeholder` 属性により算出した。
    Placeholder,
}

/// 要素の accessible name（`AISNAP-1`）。[`super::Node::name`] の算出結果。
///
/// `#[non_exhaustive]` により、将来のフィールド追加が非破壊になる
/// （REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AccessibleName {
    /// 正規化済みの名前文字列。空文字列は「名前なし」を表す。
    pub text: String,
    /// 算出に使った出所。
    pub source: NameSource,
    /// [`MAX_NAME_CHARS`]（文字数上限）または [`MAX_LABELS`]（label 数上限）
    /// により、入力の一部を切り捨てたかどうか。
    pub truncated: bool,
}

impl AccessibleName {
    /// `text` が空文字列かどうかを返す（「名前なし」の判定）。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// `text` を設定した `AccessibleName` を返す（ビルダー）。
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// `source` を設定した `AccessibleName` を返す（ビルダー）。
    #[must_use]
    pub fn with_source(mut self, source: NameSource) -> Self {
        self.source = source;
        self
    }

    /// `truncated` を設定した `AccessibleName` を返す（ビルダー）。
    #[must_use]
    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

/// HTML ASCII 空白（WHATWG 用語）かどうかを返す。`char::is_whitespace`
/// （Unicode 全般の空白）ではなく、HTML の区切り正規化規則が対象とする
/// 5 文字（`\t` `\n` `\u{0C}` `\r` ` `）だけを対象にする。
fn is_ascii_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{0C}' | '\r' | ' ')
}

/// `s` が HTML ASCII 空白以外の文字を 1 つ以上含むかどうかを返す。
fn has_non_whitespace(s: &str) -> bool {
    s.chars().any(|c| !is_ascii_whitespace(c))
}

/// accessible name を組み立てる非公開バッファ（`AISNAP-1`）。
///
/// HTML ASCII 空白の連続を 1 個の区切りへ畳み、先頭・末尾には区切りを
/// 出力しない。[`MAX_NAME_CHARS`] を超える分は追記せず `truncated` を立てる。
/// 複数の label 等の断片を、部分バッファを作らずこの 1 つのバッファへ
/// 直接書き込むことで、「truncated の真実源が複数箇所に分散する」問題を
/// 構造的に避ける。
///
/// # 契約（境界条件）
///
/// - `truncated == true` になるのは、空白以外の文字を 1 文字以上捨てた
///   ときだけ（末尾の区切りだけを捨てて `truncated` にはしない、という
///   ことはない: 区切りの直後の 1 文字が入らない場合は区切りごと捨てて
///   `truncated` を立てる。下記参照）
/// - 保留中の区切り（[`NameBuffer::push_separator`]・語の途中の空白）を
///   実際に `text` へ出力するのは、区切りとその直後の非空白 1 文字の
///   **両方**が上限に収まるとき（`char_count + 2 <= MAX_NAME_CHARS`）
///   だけである。収まらない場合は両方を捨てて `truncated = true` にする。
///   これにより `text` が空白で終わることは絶対にない
struct NameBuffer {
    text: String,
    char_count: usize,
    pending_space: bool,
    truncated: bool,
}

impl NameBuffer {
    fn new() -> Self {
        Self {
            text: String::new(),
            char_count: 0,
            pending_space: false,
            truncated: false,
        }
    }

    /// 上限に達し、これ以上文字を追記できない状態かどうか。
    fn is_full(&self) -> bool {
        self.char_count >= MAX_NAME_CHARS
    }

    /// 後続に非空白文字が来たときだけ実体化する区切りを予約する
    /// （複数の label 等、別々の断片をつなぐときに呼ぶ）。`text` が空の
    /// ときは先頭の区切りになるため何もしない。
    fn push_separator(&mut self) {
        if !self.text.is_empty() {
            self.pending_space = true;
        }
    }

    /// `s` 内の HTML ASCII 空白の連続を 1 個の区切りへ畳みながら追記する。
    ///
    /// 上限に収まらなくなった時点で追記を止め `truncated` を立てる
    /// （本構造体のドキュメンテーションコメント「契約」を参照）。
    fn push_str(&mut self, s: &str) {
        for c in s.chars() {
            if is_ascii_whitespace(c) {
                if !self.text.is_empty() {
                    self.pending_space = true;
                }
                continue;
            }
            if self.is_full() {
                self.truncated = true;
                return;
            }
            if self.pending_space && !self.text.is_empty() {
                // 区切りと直後の 1 文字の両方が入るときだけ区切りを実体化する。
                // 入らない場合は両方を捨てる（`text` が空白で終わらない）。
                if self.char_count + 2 > MAX_NAME_CHARS {
                    self.truncated = true;
                    self.pending_space = false;
                    return;
                }
                self.text.push(' ');
                self.char_count += 1;
            }
            self.pending_space = false;
            self.text.push(c);
            self.char_count += 1;
        }
    }

    /// バッファを確定し [`AccessibleName`] を返す。`text` が空なら
    /// `AccessibleName::default()`（`source` は `NameSource::None`）。
    fn finish(self, source: NameSource) -> AccessibleName {
        if self.text.is_empty() {
            return AccessibleName::default();
        }
        AccessibleName {
            text: self.text,
            source,
            truncated: self.truncated,
        }
    }
}

/// `doc` の要素 `id` から属性 `attr` の値で accessible name を算出する
/// 汎用ヘルパー（`title`・`placeholder`・`value`・`area` の `alt` が使う）。
///
/// 属性が無い、または trim 後に空文字列なら `None`（次の出所へフォール
/// バックさせる。`img` の `alt` だけはこの一般則の例外であり、
/// [`img_alt_name`] が別に扱う）。
fn attr_name(doc: &Document, id: NodeId, attr: &str, source: NameSource) -> Option<AccessibleName> {
    let value = doc.attribute(id, attr)?;
    if !has_non_whitespace(value) {
        return None;
    }
    let mut buf = NameBuffer::new();
    buf.push_str(value);
    let result = buf.finish(source);
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn title_name(doc: &Document, id: NodeId) -> Option<AccessibleName> {
    attr_name(doc, id, "title", NameSource::Title)
}

fn placeholder_name(doc: &Document, id: NodeId) -> Option<AccessibleName> {
    attr_name(doc, id, "placeholder", NameSource::Placeholder)
}

fn value_attr_name(doc: &Document, id: NodeId) -> Option<AccessibleName> {
    attr_name(doc, id, "value", NameSource::Value)
}

/// `img` 要素専用の `alt` 算出（`AISNAP-1`）。
///
/// HTML-AAM の `img` Element Accessible Name Computation は、`alt`
/// 属性を **値が trim 後に空文字列でも確定して使う**（`title` へは
/// 落ちない）。「`alt` 属性が存在するかどうか」で分岐するため、
/// [`attr_name`] の一般則（値が空なら `None`）とは異なる専用実装にする。
///
/// `alt` 属性が存在しない場合のみ `None`（呼び出し側が `title` へ
/// フォールバックする）。
fn img_alt_name(doc: &Document, id: NodeId) -> Option<AccessibleName> {
    let value = doc.attribute(id, "alt")?;
    let mut buf = NameBuffer::new();
    buf.push_str(value);
    Some(buf.finish(NameSource::Alt))
}

/// `img` 要素の accessible name を算出する（HTML-AAM `img` Element
/// Accessible Name Computation。ARIA 段を除く）。
///
/// `figcaption` へのフォールバック（`alt`・`title` がいずれも無い場合）は
/// 本 Issue のスコープ外（担当 Issue 未確定）。
fn img_name(doc: &Document, id: NodeId) -> AccessibleName {
    if let Some(name) = img_alt_name(doc, id) {
        return name;
    }
    title_name(doc, id).unwrap_or_default()
}

/// `area` 要素の accessible name を算出する（HTML-AAM `area` Element
/// Accessible Name Computation。ARIA 段を除く）。`img` と異なり `alt` は
/// 一般則（trim 後に空なら次点へ）に従う。
fn area_name(doc: &Document, id: NodeId) -> AccessibleName {
    attr_name(doc, id, "alt", NameSource::Alt)
        .or_else(|| title_name(doc, id))
        .unwrap_or_default()
}

/// `id` が要素 `id` のラベル付け可能な要素かどうかを返す（HTML Standard の
/// labeled control 規則。`AISNAP-1`）。
///
/// `input[type=hidden]` を除く `input`・`button`・`select`・`textarea`・
/// `meter`・`output`・`progress` のみが対象。それ以外の要素は label の
/// 名前を受け取らない。
fn is_labelable(doc: &Document, id: NodeId) -> bool {
    if is_html_element_named(doc, id, "input") {
        return normalized_input_type(doc, id) != "hidden";
    }
    const OTHER_LABELABLE: [&str; 6] = [
        "button", "select", "textarea", "meter", "output", "progress",
    ];
    OTHER_LABELABLE
        .iter()
        .any(|name| is_html_element_named(doc, id, name))
}

/// `input` 要素 `id` の `type` 属性値を正規化して返す（前後の ASCII 空白を
/// 除き、ASCII の大文字小文字を区別しない照合ができるよう小文字化する）。
/// 属性が無い場合は空文字列（HTML の既定値である text 系として扱う）。
fn normalized_input_type(doc: &Document, id: NodeId) -> String {
    doc.attribute(id, "type")
        .map(|value| {
            value
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

/// `doc` 全体を文書順に走査し、`id_value` と `id` 属性が完全一致する
/// （大文字小文字を区別する）**最初**の要素を返す（重複 `id` は先頭を
/// 採用する HTML の規則）。
fn first_element_with_id(doc: &Document, id_value: &str) -> Option<NodeId> {
    doc.descendants(doc.root())
        .find(|&candidate| doc.attribute(candidate, "id") == Some(id_value))
}

/// `label` の子孫のうち、文書順で最初のラベル付け可能な要素（[`is_labelable`]）
/// を返す（label による包含の判定に使う）。
fn first_labelable_descendant(doc: &Document, label: NodeId) -> Option<NodeId> {
    doc.descendants(label).find(|&id| is_labelable(doc, id))
}

/// `label` の子孫のテキストを `buf` へ集める（`AISNAP-1`）。
///
/// `script`・`style`・`noscript`・`template` のサブツリーは除外する。
/// `exclude`（関連付け先のコントロール自身）のサブツリーも除外する
/// （埋め込みコントロールの値の扱いは TASK-11.4.3・#546 のスコープ）。
/// 明示スタックで非再帰走査し、処理ステップ数を [`Document::node_count`]
/// で上限することで、深いネスト・壊れたリンクがあっても必ず停止する
/// （security.md「不安全な設計」対策）。
///
/// #544 の `collect_text`（`aria-labelledby` の参照先向け）とは別名にする
/// （役割が異なる: こちらは label→コントロールの関連付け専用）。将来
/// 両者を統合できる余地があることを、後続タスク（#546）へ申し送る。
fn collect_label_text(doc: &Document, label: NodeId, exclude: NodeId, buf: &mut NameBuffer) {
    const SKIPPED_SUBTREES: [&str; 4] = ["script", "style", "noscript", "template"];

    let mut stack: Vec<NodeId> = doc.children(label).rev().collect();
    let mut remaining_steps = doc.node_count();

    while let Some(current) = stack.pop() {
        if remaining_steps == 0 {
            break;
        }
        remaining_steps -= 1;

        if current == exclude {
            continue;
        }

        match doc.node_data(current) {
            Some(NodeData::Text { contents }) => buf.push_str(contents),
            Some(NodeData::Element { .. }) => {
                if SKIPPED_SUBTREES
                    .iter()
                    .any(|name| is_html_element_named(doc, current, name))
                {
                    continue;
                }
                stack.extend(doc.children(current).rev());
            }
            _ => {}
        }
    }
}

/// `target` に関連付く `<label>` 要素から accessible name を算出する
/// （HTML Standard の labeled control 規則。`AISNAP-1`）。
///
/// `doc` を文書順に 1 回走査し、各 `<label>` について次のいずれかで
/// `target` との関連付けを判定する。
///
/// - `for` 属性がある場合: その値と、文書順で最初に一致する `id` を持つ
///   要素が `target` 自身であり、かつ `target` がラベル付け可能
///   （[`is_labelable`]）なときに関連付く（`for=""` や `target` が `id` を
///   持たない場合は関連付かない）
/// - `for` 属性が無い場合: label の子孫のうち文書順で最初のラベル付け
///   可能な要素（[`first_labelable_descendant`]）が `target` と同じときに
///   関連付く
///
/// 関連付いた label のテキストを文書順に、区切りを挟んでつなげる。
/// 関連付ける label の数は [`MAX_LABELS`] を上限とし、超えた分は切り捨てて
/// `truncated` に反映する。`target` がラベル付け不可、または関連付く
/// label が 1 つも無い、またはテキストが空なら `None`。
fn label_name(doc: &Document, target: NodeId) -> Option<AccessibleName> {
    if !is_labelable(doc, target) {
        return None;
    }

    let target_id = doc.attribute(target, "id").filter(|id| !id.is_empty());
    // 対象要素の id が文書で最初に現れる同じ id かどうかを 1 度だけ判定する
    // （label ごとに first_element_with_id を呼び直さない。§3.5 の要求）。
    let target_is_first_with_its_id =
        target_id.is_some_and(|id| first_element_with_id(doc, id) == Some(target));

    let mut buf = NameBuffer::new();
    let mut matched_labels = 0usize;
    let mut labels_truncated = false;

    for label in doc.descendants(doc.root()) {
        if !is_html_element_named(doc, label, "label") {
            continue;
        }

        // `for` 属性が指定されている場合（値が空文字列でも）は、対象を
        // その id 一致でのみ判定し、包含（wrapping）へはフォールバック
        // しない（HTML Standard の labeled control 規則。`for=""` は
        // 「どの要素にも関連付かない」ことを意味する）。
        let matches = match doc.attribute(label, "for") {
            Some(for_value) => target_is_first_with_its_id && target_id == Some(for_value),
            None => first_labelable_descendant(doc, label) == Some(target),
        };
        if !matches {
            continue;
        }

        if matched_labels >= MAX_LABELS {
            labels_truncated = true;
            continue;
        }
        matched_labels += 1;

        buf.push_separator();
        collect_label_text(doc, label, target, &mut buf);
    }

    if matched_labels == 0 {
        return None;
    }

    let mut result = buf.finish(NameSource::Label);
    if result.is_empty() {
        return None;
    }
    if labels_truncated {
        result.truncated = true;
    }
    Some(result)
}

/// `input` 要素（`hidden` を除く）の accessible name を算出する
/// （HTML-AAM。ARIA 段を除く。`AISNAP-1`）。
///
/// `type` ごとの節分けは本モジュール冒頭のドキュメンテーションコメント
/// 「HTML-AAM による要素ごとの算出順序」の表を参照。
fn input_name(doc: &Document, id: NodeId) -> AccessibleName {
    match normalized_input_type(doc, id).as_str() {
        "hidden" => AccessibleName::default(),
        // checkbox/radio に加え、text 系の節（本モジュール冒頭の表）に
        // 明示列挙されていない他の type（range・color・date・
        // datetime-local・month・week・time・file 等）も HTML-AAM の
        // "Other Form Elements" 節（label → title。`placeholder` 段は
        // ない）に従う。`type` 省略・未知の値だけは HTML の既定である
        // text 系として扱い、下の `_` 節（`placeholder` を含む）へ渡す。
        "checkbox" | "radio" | "range" | "color" | "date" | "datetime-local" | "month" | "week"
        | "time" | "file" => label_name(doc, id)
            .or_else(|| title_name(doc, id))
            .unwrap_or_default(),
        "button" => label_name(doc, id)
            .or_else(|| value_attr_name(doc, id))
            .or_else(|| title_name(doc, id))
            .unwrap_or_default(),
        ty @ ("submit" | "reset") => {
            if let Some(name) = label_name(doc, id) {
                return name;
            }
            if let Some(name) = value_attr_name(doc, id) {
                return name;
            }
            // `value` 属性が全く無い（未指定）ときだけ既定ラベルを使う。
            // `value=""`（指定はされている）は既定ラベルを飛ばして `title`
            // へ進む（本モジュール冒頭の表を参照）。
            if doc.attribute(id, "value").is_none() {
                let default_label = if ty == "submit" { "Submit" } else { "Reset" };
                let mut buf = NameBuffer::new();
                buf.push_str(default_label);
                return buf.finish(NameSource::DefaultButtonLabel);
            }
            title_name(doc, id).unwrap_or_default()
        }
        "image" => {
            if let Some(name) = label_name(doc, id) {
                return name;
            }
            if let Some(name) = attr_name(doc, id, "alt", NameSource::Alt) {
                return name;
            }
            if let Some(name) = title_name(doc, id) {
                return name;
            }
            let mut buf = NameBuffer::new();
            buf.push_str("Submit Query");
            buf.finish(NameSource::DefaultButtonLabel)
        }
        // text/password/search/tel/url/email/number、`type` 省略・未知の
        // 値は HTML の既定（text 系コントロール）として扱う
        // （state.rs の `compute_checked` と同じ既定値の扱い方）。
        _ => label_name(doc, id)
            .or_else(|| title_name(doc, id))
            .or_else(|| placeholder_name(doc, id))
            .unwrap_or_default(),
    }
}

/// `doc` の要素 `id` から、HTML ネイティブの出所のみで accessible name を
/// 算出する（`AISNAP-1`・TASK-11.4.2）。`compute_name` の主経路。
fn native_name(doc: &Document, id: NodeId) -> AccessibleName {
    if is_html_element_named(doc, id, "img") {
        return img_name(doc, id);
    }
    if is_html_element_named(doc, id, "area") {
        return area_name(doc, id);
    }
    if is_html_element_named(doc, id, "input") {
        return input_name(doc, id);
    }
    if is_html_element_named(doc, id, "textarea") {
        return label_name(doc, id)
            .or_else(|| title_name(doc, id))
            .or_else(|| placeholder_name(doc, id))
            .unwrap_or_default();
    }
    if is_html_element_named(doc, id, "button") {
        // ネイティブ <button> 要素自身の子孫テキストへのフォールバックは
        // TASK-11.4.3（#546）のスコープ（実装済みを装わない。REPAIR-3）。
        return label_name(doc, id)
            .or_else(|| title_name(doc, id))
            .unwrap_or_default();
    }
    if is_html_element_named(doc, id, "select")
        || is_html_element_named(doc, id, "meter")
        || is_html_element_named(doc, id, "output")
        || is_html_element_named(doc, id, "progress")
    {
        return label_name(doc, id)
            .or_else(|| title_name(doc, id))
            .unwrap_or_default();
    }
    title_name(doc, id).unwrap_or_default()
}

/// `doc` の要素 `id` から accessible name を算出する（`AISNAP-1`・
/// `TASK-11.4`）。
///
/// # スタブについて
///
/// 本 Issue（TASK-11.4.2）時点で実装しているのは HTML ネイティブの
/// ラベル付けのみである（実装済みを装わない。REPAIR-3）。本ファイル冒頭の
/// ドキュメンテーションコメントに、後続タスクが担う出所（ARIA・子孫
/// テキスト・優先順位統合・文書ルートの `<title>`）を列挙してある。
///
/// 要素以外（テキストノード等）・範囲外の `id` では `AccessibleName::default()`
/// を返す（`Result` にはしない。`core::dom` のアクセサ群・
/// [`super::state::compute_state`] と同じ「範囲外・対象外は `None`/既定値」の
/// 契約に合わせる）。
///
/// 呼び出し文脈: 現時点では呼び出し元がない。TASK-11.7（Issue #76）が
/// DOM から `Snapshot`/`Node` を構築する際、算出結果の `text` を
/// [`super::Node::name`] へ格納する想定である。
pub fn compute_name(doc: &Document, id: NodeId) -> AccessibleName {
    if !doc.is_element(id) {
        return AccessibleName::default();
    }
    native_name(doc, id)
}

#[cfg(test)]
mod tests {
    use super::{AccessibleName, MAX_LABELS, MAX_NAME_CHARS, NameSource, compute_name};
    use fandhe_browser_core::dom::{Document, NodeId};
    use fandhe_browser_core::parse::{ParseOptions, parse_document};
    use fandhe_browser_core::query::query_selector_str;

    /// テスト入力の HTML をパースし、CSS セレクタで対象要素を 1 つ特定する
    /// （`state.rs` の `tests` モジュールと同じヘルパー）。
    fn parse_and_select(html: &str, selector: &str) -> (Document, NodeId) {
        let parsed =
            parse_document(html, &ParseOptions::default()).expect("テスト入力は必ず成功する");
        let doc = parsed.document;
        let root = doc.root();
        let target = query_selector_str(&doc, root, selector)
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる");
        (doc, target)
    }

    fn name(html: &str, selector: &str) -> AccessibleName {
        let (doc, id) = parse_and_select(html, selector);
        compute_name(&doc, id)
    }

    // --- img ---

    /// AISNAP-1（TASK-11.4.2・#545）: `img` の `alt` が非空ならそれを使う。
    #[test]
    fn aisnap_1_img_alt_nonempty() {
        let result = name(r#"<img alt="ロゴ" src="x.png">"#, "img");
        assert_eq!(
            result,
            AccessibleName {
                text: "ロゴ".to_string(),
                source: NameSource::Alt,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `img` の `alt=""` は `title` へ
    /// 落ちず、名前なしのまま確定する（HTML-AAM の明示的な例外）。
    #[test]
    fn aisnap_1_img_empty_alt_does_not_fall_back_to_title() {
        let result = name(r#"<img alt="" title="装飾画像" src="x.png">"#, "img");
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `alt` 属性が無い `img` は `title`
    /// を使う。
    #[test]
    fn aisnap_1_img_no_alt_uses_title() {
        let result = name(r#"<img title="装飾画像" src="x.png">"#, "img");
        assert_eq!(
            result,
            AccessibleName {
                text: "装飾画像".to_string(),
                source: NameSource::Title,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `alt`・`title` がいずれも無い `img`
    /// は名前なし。
    #[test]
    fn aisnap_1_img_without_alt_or_title_is_empty() {
        let result = name("<img src=\"x.png\">", "img");
        assert_eq!(result, AccessibleName::default());
    }

    // --- area ---

    /// AISNAP-1（TASK-11.4.2・#545）: `area` の `alt` が非空ならそれを使う。
    #[test]
    fn aisnap_1_area_alt_nonempty() {
        let result = name(
            r##"<map><area alt="地図" href="#" shape="rect" coords="0,0,1,1"></map>"##,
            "area",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "地図".to_string(),
                source: NameSource::Alt,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `area` は `img` と異なり `alt=""`
    /// が `title` へ落ちる（一般則）。
    #[test]
    fn aisnap_1_area_empty_alt_falls_back_to_title() {
        let result = name(
            r##"<map><area alt="" title="地図" href="#" shape="rect" coords="0,0,1,1"></map>"##,
            "area",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "地図".to_string(),
                source: NameSource::Title,
                truncated: false,
            }
        );
    }

    // --- input submit/reset/button ---

    /// AISNAP-1（TASK-11.4.2・#545）: `value` を持つ submit ボタンは
    /// `value` を使う。
    #[test]
    fn aisnap_1_input_submit_value() {
        let result = name(r#"<input type="submit" value="送信">"#, "input");
        assert_eq!(
            result,
            AccessibleName {
                text: "送信".to_string(),
                source: NameSource::Value,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `value` 属性が無い submit ボタンは
    /// 既定ラベル `"Submit"` を使う。
    #[test]
    fn aisnap_1_input_submit_without_value_uses_default_label() {
        let result = name(r#"<input type="submit">"#, "input");
        assert_eq!(
            result,
            AccessibleName {
                text: "Submit".to_string(),
                source: NameSource::DefaultButtonLabel,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `value` 属性が無い reset ボタンは
    /// 既定ラベル `"Reset"` を使う。
    #[test]
    fn aisnap_1_input_reset_without_value_uses_default_label() {
        let result = name(r#"<input type="reset">"#, "input");
        assert_eq!(result.text, "Reset");
        assert_eq!(result.source, NameSource::DefaultButtonLabel);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `value=""`（指定はされている）は
    /// 既定ラベルを飛ばして `title` へ進む。
    #[test]
    fn aisnap_1_input_submit_empty_value_falls_back_to_title_not_default() {
        let result = name(
            r#"<input type="submit" value="" title="送信する">"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "送信する".to_string(),
                source: NameSource::Title,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `value=""` かつ `title` も無い
    /// submit ボタンは名前なし（既定ラベルは使わない）。
    #[test]
    fn aisnap_1_input_submit_empty_value_without_title_is_empty() {
        let result = name(r#"<input type="submit" value="">"#, "input");
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `input type=button` には既定ラベルの
    /// 段がない。`value` が無ければ `title` へ直接進む。
    #[test]
    fn aisnap_1_input_button_without_value_uses_title_not_default() {
        let result = name(r#"<input type="button" title="実行">"#, "input");
        assert_eq!(
            result,
            AccessibleName {
                text: "実行".to_string(),
                source: NameSource::Title,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `input type=button` で `value`・
    /// `title` がいずれも無ければ名前なし（`"Submit"` にはならない）。
    #[test]
    fn aisnap_1_input_button_without_value_or_title_is_empty() {
        let result = name(r#"<input type="button">"#, "input");
        assert_eq!(result, AccessibleName::default());
    }

    // --- input image ---

    /// AISNAP-1（TASK-11.4.2・#545）: `input type=image` の `alt` が
    /// 非空ならそれを使う。
    #[test]
    fn aisnap_1_input_image_alt_nonempty() {
        let result = name(r#"<input type="image" alt="検索" src="x.png">"#, "input");
        assert_eq!(
            result,
            AccessibleName {
                text: "検索".to_string(),
                source: NameSource::Alt,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `input type=image` は `img` と
    /// 異なり `alt=""` が `title` へ落ちる。
    #[test]
    fn aisnap_1_input_image_empty_alt_falls_back_to_title() {
        let result = name(
            r#"<input type="image" alt="" title="検索する" src="x.png">"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "検索する".to_string(),
                source: NameSource::Title,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `input type=image` の算出手順に
    /// `value` は含まれない。`value` があっても無視し、`alt`・`title` が
    /// 無ければ既定ラベル `"Submit Query"` を使う。
    #[test]
    fn aisnap_1_input_image_ignores_value_and_uses_default_label() {
        let result = name(
            r#"<input type="image" value="検索実行" src="x.png">"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "Submit Query".to_string(),
                source: NameSource::DefaultButtonLabel,
                truncated: false,
            }
        );
    }

    // --- label 関連付け ---

    /// AISNAP-1（TASK-11.4.2・#545）: `label[for]` により名前を取得する。
    #[test]
    fn aisnap_1_label_for_associates_name() {
        let result = name(
            r#"<label for="agree">同意する</label><input type="checkbox" id="agree">"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "同意する".to_string(),
                source: NameSource::Label,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `for` と `id` の大文字小文字が
    /// 異なる場合は関連付かない（値は大文字小文字を区別する）。
    #[test]
    fn aisnap_1_label_for_is_case_sensitive() {
        let result = name(
            r#"<label for="Agree">同意する</label><input type="checkbox" id="agree">"#,
            "input",
        );
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `for=""`（指定はされているが空）は
    /// 「どの要素にも関連付かない」ことを意味し、label による包含へは
    /// フォールバックしない（HTML Standard の labeled control 規則）。
    #[test]
    fn aisnap_1_empty_for_does_not_fall_back_to_wrapping() {
        let result = name(r#"<label for="">名前 <input id="a"></label>"#, "input");
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `id` が重複している場合、文書順で
    /// 2 番目の要素には `for` が関連付かない（先頭を優先する HTML の規則）。
    #[test]
    fn aisnap_1_duplicate_id_only_first_element_gets_label() {
        // `id` が重複する 2 要素を、文書順のインデックスで明示的に選ぶ
        // （CSS セレクタは重複 `id` の 2 番目を選べないため）。
        let (doc, root) = {
            let parsed = parse_document(
                r##"<label for="x">誤り</label>
                        <input type="checkbox" id="x" title="1番目">
                        <input type="checkbox" id="x" title="2番目">"##,
                &ParseOptions::default(),
            )
            .expect("テスト入力は必ず成功する");
            let doc = parsed.document;
            let root = doc.root();
            (doc, root)
        };
        let inputs: Vec<NodeId> = doc
            .descendants(root)
            .filter(|&id| doc.local_name(id) == Some("input"))
            .collect();
        assert_eq!(inputs.len(), 2);
        let first = compute_name(&doc, inputs[0]);
        let second = compute_name(&doc, inputs[1]);
        assert_eq!(first.source, NameSource::Label);
        assert_eq!(first.text, "誤り");
        assert_eq!(second.source, NameSource::Title);
        assert_eq!(second.text, "2番目");
    }

    /// AISNAP-1（TASK-11.4.2・#545）: label による包含（`for` なし）で
    /// 名前を取得し、コントロール自身のテキスト・`option` は除外する。
    #[test]
    fn aisnap_1_label_wrapping_control_excludes_control_subtree() {
        let result = name(
            r#"<label>言語 <select><option>日本語</option></select></label>"#,
            "select",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "言語".to_string(),
                source: NameSource::Label,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `for` の label と包含 label が
    /// 両方ある場合、文書順でつながる。
    #[test]
    fn aisnap_1_for_and_wrapping_labels_concatenate_in_document_order() {
        let result = name(
            r#"<label for="agree">前置き</label>
               <label>後置き <input type="checkbox" id="agree"></label>"#,
            "input",
        );
        assert_eq!(result.text, "前置き 後置き");
        assert_eq!(result.source, NameSource::Label);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: 包含する label の中で 2 番目の
    /// ラベル付け可能な要素には関連付かない（最初の 1 つだけが対象）。
    #[test]
    fn aisnap_1_wrapping_label_only_associates_first_labelable_descendant() {
        let result = name(
            r#"<label>名前 <input id="first"><input id="second"></label>"#,
            "input#second",
        );
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `for` を持つ label がコントロールを
    /// 包含していても、`for` の側だけで判定される（二重に加算されない）。
    #[test]
    fn aisnap_1_for_label_that_also_wraps_control_counts_once() {
        let result = name(
            r#"<label for="agree">同意する <input type="checkbox" id="agree"></label>"#,
            "input",
        );
        assert_eq!(result.text, "同意する");
        assert_eq!(result.source, NameSource::Label);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: ラベル付けできない要素（`div`）は
    /// `label` の名前を受け取らない。
    #[test]
    fn aisnap_1_non_labelable_element_ignores_label_for() {
        let result = name(
            r#"<label for="x">見出し</label><div id="x">text</div>"#,
            "div",
        );
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `input type=hidden` は label が
    /// あっても常に名前なし。
    #[test]
    fn aisnap_1_hidden_input_is_always_empty() {
        let result = name(
            r#"<label for="token">トークン</label><input type="hidden" id="token">"#,
            "input",
        );
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `type` の大文字小文字・前後の空白を
    /// 無視して正規化する。
    #[test]
    fn aisnap_1_input_type_is_normalized() {
        let result = name(
            r##"<label for="x"> CheckBox </label><input type=" CHECKBOX " id="x">"##,
            "input",
        );
        assert_eq!(result.text, "CheckBox");
        assert_eq!(result.source, NameSource::Label);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: label の `script`/`style` サブ
    /// ツリーは除外する。
    #[test]
    fn aisnap_1_label_excludes_script_and_style_subtrees() {
        let result = name(
            r#"<label for="x">名前<script>evil()</script><style>.a{}</style></label><input id="x">"#,
            "input",
        );
        assert_eq!(result.text, "名前");
    }

    // --- text 入力のフォールバック連鎖 ---

    /// AISNAP-1（TASK-11.4.2・#545）: text 入力は label → title →
    /// placeholder の順にフォールバックする。
    #[test]
    fn aisnap_1_text_input_falls_back_label_title_placeholder() {
        let with_label = name(
            r#"<label for="x">名前</label><input id="x" title="t" placeholder="p">"#,
            "input",
        );
        assert_eq!(with_label.source, NameSource::Label);

        let with_title = name(r#"<input title="t" placeholder="p">"#, "input");
        assert_eq!(
            with_title,
            AccessibleName {
                text: "t".to_string(),
                source: NameSource::Title,
                truncated: false,
            }
        );

        let with_placeholder = name(r#"<input placeholder="p">"#, "input");
        assert_eq!(
            with_placeholder,
            AccessibleName {
                text: "p".to_string(),
                source: NameSource::Placeholder,
                truncated: false,
            }
        );

        let with_none = name("<input>", "input");
        assert_eq!(with_none, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: text 系節に明示列挙されない
    /// `input type=range` は `placeholder` 段を持たない（"Other Form
    /// Elements" 節: label → title のみ）。
    #[test]
    fn aisnap_1_input_range_does_not_use_placeholder() {
        let result = name(r#"<input type="range" placeholder="x">"#, "input");
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `textarea` は `placeholder` を使う。
    #[test]
    fn aisnap_1_textarea_placeholder() {
        let result = name(
            r#"<textarea placeholder="ご意見をどうぞ"></textarea>"#,
            "textarea",
        );
        assert_eq!(result.text, "ご意見をどうぞ");
        assert_eq!(result.source, NameSource::Placeholder);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `select` は `title` を使う
    /// （`placeholder` 段はない）。
    #[test]
    fn aisnap_1_select_title() {
        let result = name(r#"<select title="選択してください"></select>"#, "select");
        assert_eq!(result.text, "選択してください");
        assert_eq!(result.source, NameSource::Title);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: その他の要素（`div`）は `title`
    /// のみを使う。
    #[test]
    fn aisnap_1_other_element_uses_title_only() {
        let result = name(r#"<div title="補足">text</div>"#, "div");
        assert_eq!(result.text, "補足");
        assert_eq!(result.source, NameSource::Title);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `button` 要素は label を使う。
    #[test]
    fn aisnap_1_button_element_label() {
        let result = name(
            r#"<label for="submit-btn">送信する</label><button id="submit-btn">送信</button>"#,
            "button",
        );
        assert_eq!(result.text, "送信する");
        assert_eq!(result.source, NameSource::Label);
    }

    // --- 範囲外・非要素 ---

    /// AISNAP-1（TASK-11.4.2・#545）: テキストノードは既定値になる。
    #[test]
    fn aisnap_1_text_node_is_default() {
        let parsed = parse_document("<p>text</p>", &ParseOptions::default())
            .expect("テスト入力は必ず成功する");
        let doc = parsed.document;
        let root = doc.root();
        let p = query_selector_str(&doc, root, "p")
            .expect("セレクタは解釈できる")
            .expect("p 要素が見つかる");
        let text_node = doc.first_child(p).expect("p の子にテキストノードがある");
        assert_eq!(compute_name(&doc, text_node), AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.2・#545）: 範囲外の `NodeId` は既定値になる。
    ///
    /// `NodeId` のコンストラクタは `core` crate 内限定（`pub(crate)`）で
    /// 本 crate からは呼べないため、ノード数の多い別ドキュメントから
    /// 取得した `NodeId` を、ノード数の少ないドキュメントへ渡すことで
    /// 範囲外アクセスを再現する。
    #[test]
    fn aisnap_1_out_of_range_node_id_is_default() {
        let small = parse_document("<p>text</p>", &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document;
        let large = parse_document(
            "<div><span>a</span><span>b</span><span>c</span><span>d</span></div>",
            &ParseOptions::default(),
        )
        .expect("テスト入力は必ず成功する")
        .document;
        let out_of_range = *large
            .descendants(large.root())
            .collect::<Vec<NodeId>>()
            .last()
            .expect("子孫が存在する");
        assert!(small.node(out_of_range).is_none());
        assert_eq!(
            compute_name(&small, out_of_range),
            AccessibleName::default()
        );
    }

    // --- 空白の正規化 ---

    /// AISNAP-1（TASK-11.4.2・#545）: 連続する空白・改行・先頭末尾の
    /// 空白を 1 個の区切りへ正規化する。
    #[test]
    fn aisnap_1_whitespace_is_normalized() {
        let result = name("<div title=\"  a\t\tb\n\nc  \">text</div>", "div");
        assert_eq!(result.text, "a b c");
    }

    // --- truncated の境界 ---

    /// AISNAP-1（TASK-11.4.2・#545）: `MAX_NAME_CHARS` ちょうどの長さは
    /// 切り詰めない。
    #[test]
    fn aisnap_1_truncation_boundary_exact_length_not_truncated() {
        let text = "あ".repeat(MAX_NAME_CHARS);
        let html = format!(r#"<div title="{text}">x</div>"#);
        let result = name(&html, "div");
        assert_eq!(result.text.chars().count(), MAX_NAME_CHARS);
        assert!(!result.truncated);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: `MAX_NAME_CHARS` を 1 文字超えると
    /// 切り詰められ `truncated` が立つ。
    #[test]
    fn aisnap_1_truncation_boundary_one_over_is_truncated() {
        let text = "あ".repeat(MAX_NAME_CHARS + 1);
        let html = format!(r#"<div title="{text}">x</div>"#);
        let result = name(&html, "div");
        assert_eq!(result.text.chars().count(), MAX_NAME_CHARS);
        assert!(result.truncated);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: 区切り（label の間の空白）の直後の
    /// 1 文字が上限に収まらない場合、区切りごと捨てて `text` が空白で
    /// 終わらない（#544 で指摘された 2 つのバグの回帰テスト）。
    #[test]
    fn aisnap_1_truncation_never_leaves_trailing_space() {
        // 1 つ目の label で char_count を MAX_NAME_CHARS - 1 にちょうど
        // 揃え、2 つ目の label の先頭 1 文字が入らない状況を作る。
        let first = "あ".repeat(MAX_NAME_CHARS - 1);
        let html =
            format!(r#"<label for="x">{first}</label><label for="x">続き</label><input id="x">"#);
        let result = name(&html, "input");
        assert_eq!(result.text.chars().count(), MAX_NAME_CHARS - 1);
        assert!(!result.text.ends_with(' '));
        assert!(result.truncated);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: 1 つ目の label がちょうど
    /// `MAX_NAME_CHARS` を使い切った状態で 2 つ目の label が続く場合でも
    /// `truncated` が立つ（#544 で指摘されたバグ (a) の回帰テスト:
    /// 「バッファがちょうど満杯のときに何も追記せず抜けたのに `truncated`
    /// が立たない」を防ぐ）。
    #[test]
    fn aisnap_1_truncation_when_buffer_exactly_full_before_second_label() {
        let first = "あ".repeat(MAX_NAME_CHARS);
        let html =
            format!(r#"<label for="x">{first}</label><label for="x">続き</label><input id="x">"#);
        let result = name(&html, "input");
        assert_eq!(result.text.chars().count(), MAX_NAME_CHARS);
        assert!(!result.text.ends_with(' '));
        assert!(result.truncated);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: 関連付ける label の数がちょうど
    /// `MAX_LABELS` なら `truncated` は立たない。
    #[test]
    fn aisnap_1_max_labels_boundary_exact_count_not_truncated() {
        let mut html = String::new();
        for _ in 0..MAX_LABELS {
            html.push_str(r#"<label for="x">a</label>"#);
        }
        html.push_str(r#"<input id="x">"#);
        let result = name(&html, "input");
        assert!(!result.truncated);
        assert_eq!(
            result.text.chars().filter(|&c| c == 'a').count(),
            MAX_LABELS
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545）: 関連付ける label の数が
    /// `MAX_LABELS` を超えると切り捨てられ `truncated` が立つ。
    #[test]
    fn aisnap_1_max_labels_boundary_over_count_is_truncated() {
        let mut html = String::new();
        for _ in 0..(MAX_LABELS + 1) {
            html.push_str(r#"<label for="x">a</label>"#);
        }
        html.push_str(r#"<input id="x">"#);
        let result = name(&html, "input");
        assert!(result.truncated);
        assert_eq!(
            result.text.chars().filter(|&c| c == 'a').count(),
            MAX_LABELS
        );
    }

    // --- 深いネスト ---

    /// AISNAP-1（TASK-11.4.2・#545）: 深くネストした label でもスタック
    /// オーバーフローせず完走する（非再帰の明示スタック走査の検証）。
    #[test]
    fn aisnap_1_deeply_nested_label_does_not_overflow_stack() {
        const DEPTH: usize = 2_000;
        let mut html = String::from(r#"<label for="x">"#);
        for _ in 0..DEPTH {
            html.push_str("<span>");
        }
        html.push_str("奥の名前");
        for _ in 0..DEPTH {
            html.push_str("</span>");
        }
        html.push_str("</label><input id=\"x\">");

        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let parsed = parse_document(&html, &options).expect("深いネストでも成功する");
        let doc = parsed.document;
        let root = doc.root();
        let input = query_selector_str(&doc, root, "input")
            .expect("セレクタは解釈できる")
            .expect("input 要素が見つかる");
        let result = compute_name(&doc, input);
        assert_eq!(result.text, "奥の名前");
        assert_eq!(result.source, NameSource::Label);
    }
}
