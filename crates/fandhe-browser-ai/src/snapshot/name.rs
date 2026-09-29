//! `snapshot::Node::name` フィールドの算出ロジック（`AISNAP-1`・`TASK-11.4`・
//! `MS-2`）。
//!
//! 本ファイルが実装する accessible name の出所は次の 3 系統である。
//!
//! - **ARIA 属性による明示的な命名**（TASK-11.4.1・Issue #544）:
//!   `aria-labelledby`（IDREF リストの参照先テキストをトークン順に連結）と
//!   `aria-label`。accname 1.2 の step 2B・2C に相当し、全要素で下記の
//!   ネイティブ規則より**前**に評価する
//! - **HTML ネイティブのラベル付け**（TASK-11.4.2・Issue #545）: `alt`・
//!   `title`・`value`・`placeholder`・submit/reset/image の既定ラベル・
//!   `label[for]`・label による包含
//! - **子孫テキストと文書ルート**（TASK-11.4.3・Issue #546）: name-from-content
//!   role（button・link・heading・cell 等）の子孫テキスト（accname 2F。
//!   `hidden`/`aria-hidden="true"` の除外・子孫 `aria-label`・`img` の `alt`・
//!   埋め込みコントロールの値を含む）と、文書ルートの最初の HTML `<title>`
//!
//! 全体の算出順序は `aria-labelledby` → `aria-label` → ネイティブ
//! （label 等 → 子孫テキスト → `title`。[`compute_name_with_index`]）。ただし `input[type=hidden]` は
//! アクセシビリティツリーへ公開されないため ARIA より前に常に名前なしとする。
//!
//! `aria-labelledby` の参照先の扱い（#544 の範囲）: 参照先自身の
//! `aria-label` があれば子孫テキストより優先し、参照先自身の
//! `aria-labelledby` は辿らない（連鎖・循環を構造的に避ける）。参照先が
//! `img` なら `alt` を使い、それ以外は子孫テキスト（テキストノードと
//! 子孫 `img` の `alt`。`script`/`style`/`noscript`/`template` は除く）を使う。
//!
//! 以下は未実装とし、後続へ引き継ぐ（実装済みを装わない。REPAIR-3）。
//!
//! - 再帰中の子孫 `aria-labelledby` の追跡・子孫の `title` フォールバック
//!   （accname 2I の再帰適用）・`aria-valuetext`/range の値・参照先の label/`title`
//!   の適用・CSS のブロック要素間の区切りと生成コンテンツ: 担当 Issue 未確定
//! - `option`・`summary`・`menuitem` 等の暗黙 role が role.rs で未対応のため、
//!   これらの要素は name from content の対象にならない（role.rs 側の課題）
//! - `fieldset`→`legend`・`table`→`caption`・`figure`→`figcaption`・SVG の
//!   `<title>`・`aria-describedby`: 担当 Issue 未確定（out-of-scope-tracking
//!   に従いユーザー承認を得てから追跡する）
//! - DOM から `Snapshot`/`Node` へのツリー構築配線: TASK-11.7（Issue #76）。
//!   `id`/`label` の索引化自体は本ファイルが [`NameIndex`] として提供する
//!   （PR #567 レビュー指摘: 要素ごとに文書全体を再走査すると計算量が
//!   二乗になるため）。`aria-labelledby` の IDREF 解決も同じ索引の `id`
//!   マップを使う。TASK-11.7 は文書ごとに [`NameIndex::build`] を
//!   **1 回だけ**呼び、要素ごとには [`compute_name_with_index`] を使う
//!   ことで、文書走査を索引構築の 1 回に抑える（[`compute_name`] は単発
//!   呼び出し向けの簡易版で、内部で毎回 `NameIndex` を構築するため複数
//!   要素へ連続して使うと同じ問題が再発する）。
//!
//! 走査量の上限: 子孫走査は 1 回あたり [`MAX_CONTENT_STEPS`] で頭打ちにし、
//! 打ち切りは `truncated` で伝える。上限は要素ごとに固定で、共有索引の
//! 呼び出し順に結果が依存しない（[`compute_name`] と
//! [`compute_name_with_index`] は同じ結果を返す）。文書全体の総量は
//! 最悪 `node_count * MAX_CONTENT_STEPS` で、`node_count` はパーサーの
//! ノード数上限（`ParseOptions::max_nodes`）で抑えられる。`input[type=password]` の `value` は名前へ取り込まない。
//!
//! 呼び出し文脈: 現時点では呼び出し元がない。DOM から `Snapshot`/`Node` を
//! 構築する TASK-11.7（Issue #76）が、文書ごとに [`NameIndex::build`] を
//! 1 回呼んだうえで、ツリー構築時に要素ごとへ [`compute_name_with_index`]
//! を呼ぶ想定である（実装済みを装わない。REPAIR-3）。
//!
//! # HTML-AAM による要素ごとの算出順序
//!
//! 出典: [HTML Accessibility API Mappings — Accessible Name Computations By
//! HTML Element](https://w3c.github.io/html-aam/#accessible-name-and-description-computation)
//! （ソース: <https://github.com/w3c/aria> の `html-aam/index.html`。各節の
//! 見出しを下表に併記する）。下表は ARIA（`aria-label`/`aria-labelledby`）の
//! 段を省いてある（全要素で ARIA の 2 段が下表より前に来る。
//! [`compute_name_with_index`] 参照）。
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
//! | `button`（`<button>` 要素） | `button` Element Accessible Name Computation | label → 子孫テキスト（`NameSource::Content`）→ `title` → 名前なし | trim 後に空なら次点へ |
//! | その他の要素 | 各種 Section/Grouping・Text-level 等の節 | name-from-content role（link・heading・cell 等）なら 子孫テキスト → `title`、それ以外は `title` のみ → 名前なし | trim 後に空なら名前なし |
//!
//! ラベル関連付け（HTML Standard の labeled control 規則。上表の「label」段）
//! は `for` 属性（文書順で最初に一致する `id` を持つ要素が対象と同じ場合の
//! み）または label による包含（label の子孫のうち文書順で最初の
//! ラベル付け可能な要素が対象と同じ場合）で判定する。詳細は
//! [`label_name`] を参照。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use fandhe_browser_core::dom::{Children, Document, NodeData, NodeId};

use super::state::is_html_element_named;

/// 組み立てる accessible name の文字数上限（`AISNAP-1`）。文字数
/// （`char`）で数え、バイト数では数えない（マルチバイト文字を含む名前を
/// 不当に短く切り詰めないため）。
///
/// 外部入力（HTML の属性値・テキスト）から無制限に文字列を組み立てない
/// ための上限（security.md「不安全な設計」対策）。
const MAX_NAME_CHARS: usize = 120;

/// 1 断片（label・`aria-labelledby` の参照先）から折り畳み後に集めるテキストの
/// 文字数上限。マルチバイト文字・複数断片分の余裕を見込み [`MAX_NAME_CHARS`] の
/// 数倍を確保する。これを超える分は集めない（[`MAX_NAME_CHARS`] < 本値のため、
/// 超過した断片は [`NameBuffer`] 側で必ず `truncated` になり、切り詰めは
/// 結果へ伝わる）。
const NORMALIZED_LABEL_TEXT_CHAR_LIMIT: usize = MAX_NAME_CHARS * 4;

/// [`NameIndex`] が 1 文書につき許す、キャッシュ不能な `aria-labelledby` 参照先
/// 部分木の走査回数の上限（`AISNAP-1`・TASK-11.4.1）。参照先が対象要素自身を
/// 含む場合、テキストが対象ごとに変わるためキャッシュできない。多数の対象が
/// 同じ大きな部分木に含まれ、かつそれを参照する構成で文書全体の走査量が二乗に
/// なるのを防ぐ（security.md「不安全な設計」対策）。超過時は該当参照先を
/// 寄与なしとして扱い `truncated` を立てる。
const MAX_UNCACHED_REFERENT_SCANS: usize = 256;

/// [`NameIndex`] が 1 文書につき許す、キャッシュ可能な `aria-labelledby` 参照先
/// 部分木の**初回**走査回数の上限（`AISNAP-1`・TASK-11.4.1）。キャッシュは同じ
/// 参照先の再走査を防ぐが、互いに異なる id を持つ参照先がそれぞれ深い部分木を
/// 指す入力では初回走査自体が参照先数に比例して積み上がり、文書サイズの二乗に
/// なる。文書全体の初回走査回数（＝走査量は高々 回数 × 文書サイズ）を頭打ちに
/// し、超過時は該当参照先を寄与なしとして扱い `truncated` を立てる
/// （security.md「不安全な設計」対策）。キャッシュヒットは消費しない。
const MAX_CACHEABLE_REFERENT_SCANS: usize = 256;

/// 1 つのコントロールに関連付ける `<label>` の数の上限（`AISNAP-1`）。
///
/// 外部入力の HTML に大量の `<label for="...">` を並べられても、文書走査を
/// 定数個で打ち切るための上限（security.md「不安全な設計」対策）。超えた
/// 分は切り捨て、`truncated` へ反映する（黙って捨てない）。
///
/// この上限は「名前に実際に寄与した（テキストが空でない）label の数」を
/// 数える（PR #567 レビュー指摘の P1 修正: 空・空白だけの label が上限枠を
/// 占めてしまうと、それより後にある名前入りの label が切り捨てられ、
/// 結果として空の名前が確定して `title` 等の次点へ誤ってフォールバック
/// する不具合があった。[`label_name`] を参照）。
const MAX_LABELS: usize = 16;

/// `label_name` が 1 回の呼び出しで実際に走査（[`collect_content_text`] を
/// 呼び出す）する label の総数の上限（`AISNAP-1`）。
///
/// [`MAX_LABELS`] は「名前に寄与した label の数」だけを数える上限であり、
/// 寄与しない（空・空白だけの）label はこの上限の対象外である。この
/// ため、1 つの対象コントロールに `for` 属性で大量の空・空白だけの
/// `<label>` を関連付けると、[`MAX_LABELS`] にはいつまでも達しないまま
/// `collect_content_text` の呼び出し（文書走査を伴う）が際限なく増え続け、
/// 処理量が外部入力（label の個数）に比例して無制限に増大する
/// （security.md「不安全な設計」対策。PR #567 レビュー指摘の P1 修正）。
///
/// 本定数は「寄与したかどうかに関わらず走査した label の総数」を打ち切る
/// ことで、この処理量を定数（`MAX_LABELS_SCANNED` 件分の
/// `collect_content_text` 呼び出し）で頭打ちにする。[`MAX_LABELS`] の
/// 何倍か（空・空白だけの label がある程度混ざっていても、後続の名前
/// 入り label まで届くだけの余裕を持たせる）に設定する。
///
/// 上限に達した時点で以降の label は未走査のまま切り捨てる。この時点で
/// 寄与する label を 1 つも見つけていなければ（[`MAX_LABELS_SCANNED`] 件
/// すべてが空・空白だけの label だった場合）、[`label_name`] は
/// `None`（「寄与する label が無い」＝次点の `title`/`placeholder` へ
/// フォールバックしてよい）ではなく、`truncated: true` を立てた空の
/// `AccessibleName`（`source: NameSource::None`）を返す。呼び出し元は
/// これを `Some` として受け取るため次点へフォールバックせず、実際には
/// 走査上限より後ろに名前入りの label が存在するかもしれない（＝
/// 「名前なし」と確定できない）状態を、誤って `title`/`placeholder` の
/// 値へすり替えずに呼び出し元へ伝える（`AISNAP-1`・PR #567 レビュー
/// 指摘の P1 再修正: 空・空白だけの label が [`MAX_LABELS_SCANNED`] 件
/// 先行すると、それより後ろの唯一の名前入り label に到達できないまま
/// `matched_labels == 0` で `None` が返り、誤って `title`/`placeholder`
/// へフォールバックしていた不具合の修正）。
const MAX_LABELS_SCANNED: usize = MAX_LABELS * 4;

/// 対象要素自身の子孫（name from content。TASK-11.4.3）を 1 回走査するときに
/// 訪問するノード数の上限（`AISNAP-1`）。
///
/// 行・セル・リンク・見出しなど content role の要素はすべて自分の部分木を
/// 走査するため、入れ子では走査量の総和が文書サイズの二乗になりうる。1 回あたりを
/// 定数で打ち切る（security.md「不安全な設計」対策）。上限は要素ごとに固定で、
/// 共有索引を使う呼び出しの順序に結果が依存しない（PR #574 レビュー指摘）。
/// 文書全体の総量は最悪 `node_count * 本値` で、パーサーのノード数上限で
/// 抑えられる。打ち切ったら `truncated` を立てる。
const MAX_CONTENT_STEPS: usize = 1024;

/// `aria-labelledby` で名前に**寄与した**（正規化後のテキストが空でない）
/// 参照先の数の上限（`AISNAP-1`・TASK-11.4.1）。
///
/// 超えた分は切り捨てて `truncated` に反映する（黙って捨てない）。存在しない
/// id・空の参照先はこの枠を消費しない（[`MAX_LABELS`] と同じ設計。空の
/// 参照先が枠を占めて後ろの名前入りを落とす不具合を避ける）。
const MAX_IDREFS: usize = 16;

/// `aria-labelledby` で寄与の有無にかかわらず調べるトークン総数の上限
/// （`AISNAP-1`・TASK-11.4.1）。
///
/// トークンごとに参照先の部分木を走査しうるため、外部入力（属性値の
/// トークン数）に比例する処理量をここで頭打ちにする（security.md「不安全な
/// 設計」対策）。寄与する参照先を 1 つも見つけないまま上限に達した場合は、
/// [`MAX_LABELS_SCANNED`] と同じく `None` ではなく `truncated: true` の空の
/// 名前を返し、`aria-label` 等の次点へ誤ってフォールバックしない。
const MAX_IDREFS_SCANNED: usize = MAX_IDREFS * 4;

/// accessible name の出所（`AISNAP-1`）。
///
/// ARIA 属性由来（`aria-labelledby`・`aria-label`。TASK-11.4.1・#544）、
/// HTML ネイティブの出所（TASK-11.4.2・#545）、子孫テキストと文書ルートの
/// `<title>`（TASK-11.4.3・#546）を持つ。将来の出所追加は非破壊
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
    /// `aria-labelledby` の参照先テキストにより算出した。
    AriaLabelledBy,
    /// `aria-label` 属性により算出した。
    AriaLabel,
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
    /// 子孫のテキスト（name from content。TASK-11.4.3）により算出した。
    Content,
    /// 文書ルートの最初の HTML `<title>` 要素により算出した（TASK-11.4.3）。
    DocumentTitle,
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
    /// [`MAX_NAME_CHARS`]（文字数上限）・[`MAX_CONTENT_STEPS`]（子孫走査 1 回あたりの
    /// ステップ上限）・
    /// 子孫走査の折り畳み後文字数上限・[`MAX_LABELS`]（label 数上限）・
    /// [`MAX_LABELS_SCANNED`]（label 走査総数の上限）・[`MAX_IDREFS`]
    /// （`aria-labelledby` の寄与参照先数の上限）・[`MAX_IDREFS_SCANNED`]
    /// （同トークン走査総数の上限）。走査総数の上限は、寄与するものを
    /// 1 つも見つけられないまま打ち切られた場合に `text` が空のまま
    /// `true` になる。いずれかにより、入力の一部を切り捨てたかどうか。
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

/// `input` 要素 `id` の `type` 属性値を正規化して返す（ASCII の大文字小文字
/// を区別しない照合ができるよう小文字化するのみで、前後の空白は除去しない）。
/// 属性が無い場合は空文字列（HTML の既定値である text 系として扱う）。
///
/// HTML Standard の `input` type キーワード照合は ASCII 大文字小文字を
/// 区別しない完全一致であり、前後に空白を含む値（例: `type=" submit "`）は
/// どのキーワードにも一致しない無効値として扱われ、既定の text 状態へ
/// フォールバックする。前後の空白を除去してから照合すると
/// `type=" submit "` を `submit` 状態、`type=" hidden "` を `hidden` 状態と
/// 誤判定してしまう（PR #567 レビュー指摘の P1 修正）。
fn normalized_input_type(doc: &Document, id: NodeId) -> String {
    doc.attribute(id, "type")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default()
}

/// [`collect_content_text`] の走査条件（`AISNAP-1`・TASK-11.4.3）。
///
/// label・`aria-labelledby` の参照先・対象要素自身の子孫（name from content）の
/// 3 経路が同じ走査関数を共有するための差分だけを持つ非公開の引数。
#[derive(Debug, Clone, Copy)]
struct ContentWalk {
    /// 訪問するノード数の上限。超えたら走査を打ち切り [`ContentScan::cut`] を立てる。
    max_steps: usize,
    /// `true` なら `hidden`/`aria-hidden="true"` の子孫も取り込む
    /// （accname 2A: 直接参照された hidden な参照先の内側だけで使う）。
    include_hidden: bool,
}

/// [`collect_content_text`] の走査結果（`AISNAP-1`・TASK-11.4.3）。
#[derive(Debug, Clone, Copy, Default)]
struct ContentScan {
    /// 文字数上限（[`NORMALIZED_LABEL_TEXT_CHAR_LIMIT`]）またはステップ上限で
    /// 入力の一部を集めずに打ち切ったか。`out` の長さからは推論できない
    /// （ステップ上限で切れたときは文字数が少ないため）ので明示的に返す。
    cut: bool,
    /// 消費したステップ数（文書全体の予算の差し引きに使う）。
    steps_used: usize,
}

/// `script`・`style`・`noscript`・`template` は名前の計算に寄与しない。
const SKIPPED_SUBTREES: [&str; 4] = ["script", "style", "noscript", "template"];

/// 要素が `hidden` 属性を持つか、`aria-hidden` が（HTML 空白の trim・ASCII
/// 大文字小文字無視で）`"true"` かを返す（accname 2A。CSS による非表示は
/// スタイル未評価のため対象外）。
fn is_hidden_element(doc: &Document, id: NodeId) -> bool {
    if doc.attribute(id, "hidden").is_some() {
        return true;
    }
    doc.attribute(id, "aria-hidden").is_some_and(|value| {
        value
            .trim_matches(is_ascii_whitespace)
            .eq_ignore_ascii_case("true")
    })
}

/// 埋め込みコントロール（accname 2E）が名前へ寄与するテキストを返す
/// （`AISNAP-1`・TASK-11.4.3）。寄与しない要素は `None`（通常の子孫走査へ進む）。
///
/// - テキスト系 `input`（`type` 省略・未知を含む）→ `value`。`password` は
///   平文がスナップショットへ漏れないよう**取り込まない**
/// - `select` → `selected` 属性を持つ最初の `option`、無ければ最初の `option` の
///   テキスト（探索は `max_steps` の半分までを消費し、残りは候補のテキスト収集に
///   確保する。探索が上限に達したら最初の `option` を採用し `cut` を立てる）
///
/// `aria-valuetext` や range の値は未実装（担当 Issue 未確定）。
fn embedded_control_text(
    doc: &Document,
    id: NodeId,
    exclude: NodeId,
    max_steps: usize,
) -> Option<(String, ContentScan)> {
    if is_html_element_named(doc, id, "input") {
        if matches!(
            normalized_input_type(doc, id).as_str(),
            "hidden"
                | "checkbox"
                | "radio"
                | "range"
                | "color"
                | "date"
                | "datetime-local"
                | "month"
                | "week"
                | "time"
                | "file"
                | "button"
                | "submit"
                | "reset"
                | "image"
                | "password"
        ) {
            return None;
        }
        let mut out = String::new();
        if let Some(value) = doc.attribute(id, "value") {
            fold_attr_bounded(value, &mut out);
        }
        return Some((out, ContentScan::default()));
    }
    if is_html_element_named(doc, id, "select") {
        let mut scan = ContentScan::default();
        // 子ノードを事前に全件確保せず、遅延イテレータのスタックで辿る。
        // 確保は「積んだ階層数（各 1 イテレータ）」に比例し、階層は 1 ステップ
        // ごとにしか増えないため `scan_limit` で有界になる（入力の子数に比例しない）。
        let mut stack: Vec<Children<'_>> = vec![doc.children(id)];
        let mut first: Option<NodeId> = None;
        let mut chosen: Option<NodeId> = None;
        // 選択状態の探索へ予算の半分までを割り当て、残りは選択候補のテキスト収集へ
        // 確保する。後続の `option` が大量でも、確定済みの候補（最初の `option`）の
        // 名前が予算枯渇で失われないようにする（PR #574 レビュー指摘）。
        let scan_limit = max_steps - max_steps / 2;
        while let Some(top) = stack.last_mut() {
            let Some(current) = top.next() else {
                stack.pop();
                continue;
            };
            if scan.steps_used >= scan_limit {
                scan.cut = true;
                break;
            }
            scan.steps_used += 1;
            if is_html_element_named(doc, current, "option") {
                if doc.attribute(current, "selected").is_some() {
                    chosen = Some(current);
                    break;
                }
                first.get_or_insert(current);
            } else if doc.is_element(current) {
                stack.push(doc.children(current));
            }
        }
        let mut out = String::new();
        if let Some(option) = chosen.or(first) {
            let inner = collect_content_text(
                doc,
                option,
                exclude,
                ContentWalk {
                    max_steps: max_steps.saturating_sub(scan.steps_used),
                    include_hidden: false,
                },
                &mut out,
            );
            scan.steps_used += inner.steps_used;
            scan.cut |= inner.cut;
        }
        return Some((out, scan));
    }
    None
}

/// `start` の子孫のテキストを収集し、`out` へ HTML ASCII 空白の連続を
/// 1 個の区切りへ畳みながら追記する（`AISNAP-1`。TASK-11.4.2 の label 用走査を
/// TASK-11.4.3 で name from content・参照先にも共有できるよう拡張）。
///
/// 呼び出し元: [`label_name`]（label の子孫）・[`referent_text`]
/// （`aria-labelledby` の参照先）・[`content_name`]（対象要素自身の子孫）・
/// 文書ルートの `<title>`（[`compute_name_with_index`]）。
///
/// 取り込む内容: テキストノード・子孫 `img` の `alt`（accname 2D の代替テキスト。
/// 子へは降りない）・子孫の `aria-label`（空白以外を含むとき。accname 2C の
/// 再帰適用。その部分木へは降りない）・埋め込みコントロールの値
/// （[`embedded_control_text`]）。`script`・`style`・`noscript`・`template` と
/// `exclude`（名前を算出している対象自身）の部分木、および
/// [`ContentWalk::include_hidden`] が `false` のときの hidden 部分木は除外する。
///
/// 未実装（実装済みを装わない。REPAIR-3）: 再帰中の子孫 `aria-labelledby` の
/// 追跡・子孫の `title` フォールバック・CSS のブロック要素間の区切り。
///
/// 明示スタックで非再帰に走査し、訪問数を [`ContentWalk::max_steps`] で上限する
/// ため、深いネストでも必ず停止する（security.md「不安全な設計」対策）。
/// 打ち切ったかどうかは戻り値の [`ContentScan::cut`] で明示的に返す。
///
/// **前提**: `out` は呼び出し時点で空文字列であること。
///
/// 上限判定は**折り畳み後**の文字数（[`NORMALIZED_LABEL_TEXT_CHAR_LIMIT`]）で
/// 行う。生バイト数で打ち切ると、本体の前に大量の HTML 空白がある入力で本体が
/// 収集されず「空」と誤判定され次点へ誤ってフォールバックする
/// （PR #567 レビュー指摘の P1 修正）。
fn collect_content_text(
    doc: &Document,
    start: NodeId,
    exclude: NodeId,
    opts: ContentWalk,
    out: &mut String,
) -> ContentScan {
    debug_assert!(
        out.is_empty(),
        "collect_content_text は空の out を前提とする"
    );

    let mut scan = ContentScan::default();
    let mut stack: Vec<NodeId> = doc.children(start).rev().collect();
    let mut normalized_chars = 0usize;
    let mut pending_space = false;

    macro_rules! fold {
        ($chars:expr) => {
            if fold_chars_into(
                $chars,
                out,
                &mut normalized_chars,
                &mut pending_space,
                NORMALIZED_LABEL_TEXT_CHAR_LIMIT,
            ) {
                scan.cut = true;
                break;
            }
        };
    }

    while let Some(current) = stack.pop() {
        if scan.steps_used >= opts.max_steps {
            scan.cut = true;
            break;
        }
        scan.steps_used += 1;

        if current == exclude {
            continue;
        }

        match doc.node_data(current) {
            Some(NodeData::Text { contents }) => fold!(contents.chars()),
            Some(NodeData::Element { .. }) => {
                if SKIPPED_SUBTREES
                    .iter()
                    .any(|name| is_html_element_named(doc, current, name))
                {
                    continue;
                }
                if !opts.include_hidden && is_hidden_element(doc, current) {
                    continue;
                }
                // accname 2C: 再帰中の埋め込みコントロール（2E）では `aria-label` を無視して
                // コントロールの値を優先するため、`aria-label` より先に判定する。
                if let Some((text, inner)) = embedded_control_text(
                    doc,
                    current,
                    exclude,
                    opts.max_steps.saturating_sub(scan.steps_used),
                ) {
                    scan.steps_used += inner.steps_used;
                    scan.cut |= inner.cut;
                    fold!(text.chars());
                    if inner.cut {
                        break;
                    }
                    continue;
                }
                if let Some(label) = doc.attribute(current, "aria-label")
                    && has_non_whitespace(label)
                {
                    fold!(label.chars());
                    continue;
                }
                if is_html_element_named(doc, current, "img") {
                    // `img` は `alt` を子孫テキストの代わりに使う。子要素を
                    // 持たないため `stack` へは積まない。
                    if let Some(alt) = doc.attribute(current, "alt") {
                        fold!(alt.chars());
                    }
                    continue;
                }
                stack.push(doc.children(current));
            }
            _ => {}
        }
    }
    scan
}

/// [`collect_content_text`] のテキスト折り畳み本体（`AISNAP-1`・PR #567
/// レビュー指摘の P1 修正）。テキストノードの内容・`img` の `alt` 属性値の
/// 両方から同じ規則（HTML ASCII 空白の連続を 1 個へ畳む）で `out` へ
/// 追記できるよう共有する。`normalized_chars`・`pending_space` は呼び出し元
/// が同じバッファに対して連続する複数ノード分を通して保持する状態であり、
/// このヘルパーはその状態を更新するだけで新規に作らない。
///
/// 戻り値: `limit`（折り畳み後の文字数上限）に達したら `true`
/// （呼び出し元は `'walk` ループを打ち切る）。
fn fold_chars_into(
    chars: impl Iterator<Item = char>,
    out: &mut String,
    normalized_chars: &mut usize,
    pending_space: &mut bool,
    limit: usize,
) -> bool {
    for c in chars {
        if is_ascii_whitespace(c) {
            if !out.is_empty() {
                *pending_space = true;
            }
            continue;
        }
        if *pending_space {
            if *normalized_chars >= limit {
                return true;
            }
            out.push(' ');
            *normalized_chars += 1;
            *pending_space = false;
        }
        if *normalized_chars >= limit {
            return true;
        }
        out.push(c);
        *normalized_chars += 1;
    }
    false
}

/// `doc` 全体で 1 回だけ構築する、accessible name 算出用の索引
/// （`AISNAP-1`・PR #567 レビュー指摘の P1 修正）。
///
/// `label_name` が対象要素ごとに文書全体を再走査すると、TASK-11.7（#76）
/// でツリー構築時に要素ごと [`compute_name_with_index`] を呼んだ場合に
/// 計算量が文書サイズの二乗になる。本構造体は `id` 索引と
/// `label`→対象コントロールの関連付けを文書につき 1 回の走査で構築し、
/// 以降の `label_name` 呼び出しをハッシュマップの参照だけに抑える。
///
/// 構築は [`build`](Self::build) で行う。`labels_by_target` フィールドは
/// 非公開（本ファイル内の実装詳細）。`id` 索引（属性値 → 文書順で最初に
/// 一致した要素）は `label`→対象コントロールの関連付けの解決に使い、
/// 構築後も `aria-labelledby` の IDREF 解決のために `ids` として保持する
/// （TASK-11.4.1。参照ごとに文書を再走査しない）。
///
/// `doc` フィールドは構築元の文書への参照を保持する。[`NodeId`] は文書内の
/// 番号（arena インデックス）に過ぎず文書をまたいだ一意性を持たないため、
/// 構築元と異なる文書の `NodeId` を渡すと `labels_by_target` が無関係の
/// 要素に一致し、誤ったラベルを適用しかねない（PR #567 レビュー指摘の P2
/// 修正）。ライフタイム `'doc` で `NameIndex` に構築元の文書を型として
/// 結び付け、[`compute_name_with_index`] 側で `doc` 引数との同一性
/// （`std::ptr::eq`）を検証することで、この誤用を実行時に検出する。
pub struct NameIndex<'doc> {
    /// 構築元の文書（[`compute_name_with_index`] が渡された `doc` と
    /// 同一かどうかの検証に使う）。
    doc: &'doc Document,
    /// ラベル付け可能な対象コントロール → 関連付く `<label>` の文書順
    /// リスト（`for` 属性による関連付け・label による包含の両方を含む）。
    labels_by_target: HashMap<NodeId, Vec<NodeId>>,
    /// `id` 属性値 → 文書順で最初に一致した要素（空の値は登録しない。
    /// 大文字小文字を区別して照合する）。`aria-labelledby` の IDREF 解決用。
    ids: HashMap<&'doc str, NodeId>,
    /// `aria-labelledby` の参照先 → 折り畳み済みテキストのキャッシュ。
    /// 対象要素を含まない参照先だけを登録する（部分木の走査を参照先ごとに
    /// 文書につき 1 回へ抑える。各値は [`NORMALIZED_LABEL_TEXT_CHAR_LIMIT`]
    /// 文字以内）。
    referent_cache: RefCell<HashMap<NodeId, (String, bool)>>,
    /// 各ノードの入退場番号（DFS の enter/exit 時計。`(enter, exit)`）。
    /// 祖先判定を参照ごとの親リンク走査（最悪 O(深さ)）ではなく定数時間で
    /// 行うための索引（`aria-labelledby` を持つ深い入れ子要素が多い入力での
    /// 二乗の処理量を防ぐ。AGENTS.md のリソース上限）。
    spans: HashMap<NodeId, (usize, usize)>,
    /// キャッシュ不能な参照先走査の残り回数（[`MAX_UNCACHED_REFERENT_SCANS`]）。
    uncached_scans_left: Cell<usize>,
    /// キャッシュ可能な参照先の初回走査の残り回数（[`MAX_CACHEABLE_REFERENT_SCANS`]）。
    first_scans_left: Cell<usize>,
    /// 文書順で最初の HTML 名前空間の `<title>` 要素（文書ルートの名前用。
    /// SVG の `<title>` は含めない）。構築時の単一走査で記録する。
    first_title: Option<NodeId>,
}

impl<'doc> NameIndex<'doc> {
    /// `ancestor` が `node` 自身またはその祖先かどうかを、構築時に記録した
    /// 入退場番号の区間包含で定数時間に判定する。索引に載らないノード
    /// （走査上限で打ち切られた分）は「含まない」として扱う。
    fn is_ancestor_or_self(&self, ancestor: NodeId, node: NodeId) -> bool {
        match (self.spans.get(&ancestor), self.spans.get(&node)) {
            (Some(&(a_in, a_out)), Some(&(n_in, n_out))) => a_in <= n_in && n_out <= a_out,
            _ => false,
        }
    }

    /// `doc` を 1 回走査して [`NameIndex`] を構築する。
    ///
    /// 呼び出し文脈: 単発の [`compute_name`] は毎回これを内部で構築する
    /// （簡易版）。文書中の複数要素へ繰り返し名前を算出する呼び出し元
    /// （TASK-11.7 のツリー構築等）は、文書ごとに本関数を 1 回だけ呼び、
    /// 結果を [`compute_name_with_index`] へ使い回すこと。
    pub fn build(doc: &'doc Document) -> Self {
        // 文書順で辿った `<label>` 要素の一覧（`for` 属性の有無を問わない）。
        // 各 label を最終的に `labels_by_target` へ積む順序を、対象の
        // 解決方式（`for` 属性か包含か）によらず文書順のまま保つために使う
        // （包含由来の解決は下の単一走査中に即座に行い、`for` 属性由来の
        // 解決は走査完了後に `ids_by_value` を使って行うため、解決の
        // タイミングが異なる 2 種類を後から正しい順序でマージする必要が
        // ある）。
        let mut label_order: Vec<NodeId> = Vec::new();
        // `for` 属性を持つ label（値が空文字列でないもの）。対象は
        // `ids_by_value` が完成してから解決する（対象の `id` が文書中で
        // label より後に現れる場合があるため）。
        let mut for_labels: Vec<(NodeId, String)> = Vec::new();
        // 包含（wrapping）による解決結果。単一走査中に確定する。
        let mut wrap_targets: HashMap<NodeId, NodeId> = HashMap::new();
        let mut ids_by_value: HashMap<&'doc str, NodeId> = HashMap::new();

        // 単一走査で「id 索引」「label の一覧・`for` 値」「包含による
        // label→対象の解決」を同時に行う（`AISNAP-1`・PR #567 レビュー
        // 指摘の P1 修正）。
        //
        // 包含の解決は、祖先方向に開いたまま（まだ最初のラベル付け可能な
        // 子孫が見つかっていない）`<label>` をスタック（`open_wrapping`）
        // で追跡し、ラベル付け可能な要素へ入った時点でスタック中の未解決
        // label を**まとめて**解決する（1 つの要素が複数の入れ子 label に
        // とって同時に「最初のラベル付け可能な子孫」になり得るため）。
        // 各 label は解決された時点でスタックから外れ、二度と更新されない
        // ため、この解決処理の総コストは label 数に比例する（1 label
        // あたり高々 1 回の解決）。旧実装は label ごとに
        // `doc.descendants(label)` で子孫を再走査しており、入れ子の
        // `<label>` が多い文書で文書サイズに対し二乗の計算量になっていた
        // （PR #567 レビュー指摘の P1 修正）。
        //
        // 走査自体は列挙型 `Visit::{Enter, Exit}` を明示スタックに積む
        // 反復的な行きがけ／帰りがけ走査で行う（`Exit` により「この
        // ノードの子孫を辿り終えた」タイミングを検出できる。`open_wrapping`
        // の末尾がその `Exit` のノードと一致すれば、対象が見つからないまま
        // 子孫を辿り終えた label としてスタックから外し、関連付けなし
        // （`None`）のまま捨てる）。
        enum Visit {
            Enter(NodeId),
            Exit(NodeId),
        }

        let mut stack: Vec<Visit> = vec![Visit::Enter(doc.root())];
        let mut open_wrapping: Vec<NodeId> = Vec::new();
        let mut remaining_steps = doc.node_count();
        let mut spans: HashMap<NodeId, (usize, usize)> = HashMap::new();
        let mut clock = 0usize;
        let mut first_title: Option<NodeId> = None;

        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Exit(node) => {
                    if let Some(span) = spans.get_mut(&node) {
                        span.1 = clock;
                    }
                    clock += 1;
                    if open_wrapping.last() == Some(&node) {
                        // 子孫を辿り終えてもラベル付け可能な対象が見つから
                        // なかった包含 label。関連付けなしとして捨てる。
                        open_wrapping.pop();
                    }
                    continue;
                }
                Visit::Enter(node) => {
                    if remaining_steps == 0 {
                        break;
                    }
                    remaining_steps -= 1;
                    spans.insert(node, (clock, usize::MAX));
                    clock += 1;

                    if let Some(id_value) = doc.attribute(node, "id")
                        && !id_value.is_empty()
                    {
                        // 文書順で先に見つかったものだけを残す（重複 id は先頭優先）。
                        ids_by_value.entry(id_value).or_insert(node);
                    }

                    // 文書タイトルは `head` 直下の `title` に限る（body 内の `title` や
                    // 他要素配下の `title` は文書タイトルにしない。HTML Standard の
                    // `document.title`。PR #574 レビュー指摘）。
                    if first_title.is_none()
                        && is_html_element_named(doc, node, "title")
                        && doc
                            .parent(node)
                            .is_some_and(|parent| is_html_element_named(doc, parent, "head"))
                    {
                        first_title = Some(node);
                    }

                    if is_html_element_named(doc, node, "label") {
                        label_order.push(node);
                        // `for` 属性が指定されている場合（値が空文字列でも）
                        // は、対象をその id 一致でのみ判定し、包含
                        // （wrapping）へはフォールバックしない（HTML
                        // Standard の labeled control 規則。`for=""` は
                        // 「どの要素にも関連付かない」ことを意味する）。
                        match doc.attribute(node, "for") {
                            Some(for_value) if !for_value.is_empty() => {
                                for_labels.push((node, for_value.to_string()));
                            }
                            Some(_) => {}
                            None => open_wrapping.push(node),
                        }
                    } else if is_labelable(doc, node) && !open_wrapping.is_empty() {
                        // 現在開いている（未解決の）包含 label は、すべて
                        // この要素を「文書順で最初のラベル付け可能な子孫」
                        // として同時に解決する。
                        for &label in &open_wrapping {
                            wrap_targets.insert(label, node);
                        }
                        open_wrapping.clear();
                    }

                    stack.push(Visit::Exit(node));
                    stack.extend(doc.children(node).rev().map(Visit::Enter));
                }
            }
        }

        let mut labels_by_target: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let for_targets: HashMap<NodeId, Option<NodeId>> = for_labels
            .into_iter()
            .map(|(label, for_value)| (label, ids_by_value.get(for_value.as_str()).copied()))
            .collect();

        // `label_order`（文書順）を 1 回だけ辿り、解決方式によらず正しい
        // 文書順で `labels_by_target` へ積む。
        for label in label_order {
            let target = match for_targets.get(&label) {
                Some(target) => *target,
                None => wrap_targets.get(&label).copied(),
            };
            if let Some(target) = target {
                labels_by_target.entry(target).or_default().push(label);
            }
        }

        Self {
            doc,
            labels_by_target,
            ids: ids_by_value,
            spans,
            referent_cache: RefCell::new(HashMap::new()),
            uncached_scans_left: Cell::new(MAX_UNCACHED_REFERENT_SCANS),
            first_scans_left: Cell::new(MAX_CACHEABLE_REFERENT_SCANS),
            first_title,
        }
    }
}

/// `target` に関連付く `<label>` 要素から accessible name を算出する
/// （HTML Standard の labeled control 規則。`AISNAP-1`）。
///
/// `index`（[`NameIndex`]）を使い、`target` に関連付く label の一覧を
/// ハッシュマップ参照 1 回で取得する（文書の再走査はしない。PR #567
/// レビュー指摘の P1 修正）。関連付けの判定規則自体は [`NameIndex::build`]
/// を参照。
///
/// 関連付いた label のうち、テキストが空・空白だけでなく実際に名前へ
/// 寄与するものだけを文書順に、区切りを挟んでつなげる。関連付ける label
/// の数は [`MAX_LABELS`] を上限とし、超えた分は切り捨てて `truncated` に
/// 反映する（**上限は寄与する label だけを数える**: 空・空白だけの label
/// が上限枠を占めて、後続の名前入り label を誤って切り捨てないようにする
/// ため。PR #567 レビュー指摘の P1 修正）。`target` がラベル付け不可なら
/// `None`。
///
/// 寄与する label が 1 つも見つからなかった場合の戻り値は 2 通りに分かれる
/// （PR #567 レビュー指摘の P1 再修正）。
/// - 関連付く label を [`MAX_LABELS_SCANNED`] に達する前にすべて走査し
///   終え、それでも寄与するものが無かった場合: `None`（「本当に名前が
///   無い」。呼び出し元は `title`/`placeholder` 等の次点へフォール
///   バックしてよい）
/// - [`MAX_LABELS_SCANNED`] に達して打ち切られ、寄与する label を 1 つも
///   見つけられないまま終わった場合: `Some` の空の `AccessibleName`
///   （`source: NameSource::None`・`truncated: true`）。走査しきれて
///   いない以上「本当に名前が無い」とは確定できないため、`None` とは
///   区別し、呼び出し元が次点へフォールバックしないようにする
///   （[`MAX_LABELS_SCANNED`] のドキュメンテーションコメント参照）。
fn label_name(doc: &Document, index: &NameIndex, target: NodeId) -> Option<AccessibleName> {
    if !is_labelable(doc, target) {
        return None;
    }

    let labels = index.labels_by_target.get(&target)?;

    let mut buf = NameBuffer::new();
    let mut matched_labels = 0usize;
    let mut labels_truncated = false;
    // `labels_scanned >= MAX_LABELS_SCANNED` により、寄与する label を
    // 1 つも見つけないまま走査を打ち切ったかどうか。`matched_labels == 0`
    // と併せて「本当に名前が無い」のか「走査しきれておらず未確定」なのか
    // を区別するために使う（[`MAX_LABELS_SCANNED`] のドキュメンテーション
    // コメント参照。PR #567 レビュー指摘の P1 再修正）。
    let mut scan_cut_before_match = false;

    for (labels_scanned, &label) in labels.iter().enumerate() {
        if labels_scanned >= MAX_LABELS_SCANNED {
            // 寄与したかどうかに関わらず、走査した label の総数がここで
            // 打ち切られる（[`MAX_LABELS_SCANNED`] 参照）。以降の label は
            // 未走査のまま切り捨てるため truncated を立てる。
            labels_truncated = true;
            scan_cut_before_match = matched_labels == 0;
            break;
        }

        let mut label_text = String::new();
        let scan = collect_content_text(
            doc,
            label,
            target,
            ContentWalk {
                max_steps: doc.node_count(),
                include_hidden: false,
            },
            &mut label_text,
        );
        // 文字数上限での打ち切りを伝える（黙って捨てない）。
        labels_truncated |= scan.cut;
        if !has_non_whitespace(&label_text) {
            if scan.cut {
                // 走査が打ち切られて文字を得られなかった label は、打ち切り
                // 位置より後ろに本文があるかもしれず「名前なし」と確定できない
                // （PR #574 レビュー指摘）。寄与する label が最後まで 0 件なら
                // 次点へフォールバックさせない。
                scan_cut_before_match = true;
            }
            // 名前に寄与しない label は MAX_LABELS の対象に数えない。
            continue;
        }

        if matched_labels >= MAX_LABELS {
            labels_truncated = true;
            continue;
        }
        matched_labels += 1;

        buf.push_separator();
        buf.push_str(&label_text);
    }

    if matched_labels == 0 {
        if scan_cut_before_match {
            // 走査上限に達したために寄与する label を 1 つも見つけられ
            // なかった。まだ走査していない label の中に名前入りのものが
            // あるかもしれず「本当に名前が無い」とは確定できないため、
            // `None`（次点の `title`/`placeholder` へのフォールバックを
            // 許す）ではなく `truncated: true` を立てた空の名前を返し、
            // 呼び出し元にフォールバックさせない。
            return Some(AccessibleName::default().with_truncated(true));
        }
        return None;
    }

    // 寄与する label を 1 つ以上数えた時点で `buf` には非空白文字が
    // 最低 1 文字は入っているため（`NameBuffer` は空でない先頭 1 文字を
    // 上限に関わらず必ず受け付ける）、`buf.finish` が空になることはない。
    let mut result = buf.finish(NameSource::Label);
    if labels_truncated {
        result.truncated = true;
    }
    Some(result)
}

/// ARIA 1.2 で「Name from: contents」を許す role かどうかを返す
/// （`AISNAP-1`・TASK-11.4.3）。
///
/// role 判定は [`super::role::compute_role`] に一本化し、タグ名の独自リストは
/// 持たない（二重管理を避ける）。`option`・`summary`・`menuitem` 等は role.rs
/// 側が暗黙 role を未対応で `generic` を返すため、現状は content 命名されない
/// （role.rs 側の未対応。本ファイルでは回避しない。REPAIR-3）。
fn allows_name_from_content(doc: &Document, id: NodeId) -> bool {
    const CONTENT_ROLES: [&str; 18] = [
        "button",
        "cell",
        "checkbox",
        "columnheader",
        "gridcell",
        "heading",
        "link",
        "menuitem",
        "menuitemcheckbox",
        "menuitemradio",
        "option",
        "radio",
        "row",
        "rowheader",
        "switch",
        "tab",
        "tooltip",
        "treeitem",
    ];
    super::role::compute_role(doc, id).is_some_and(|role| CONTENT_ROLES.contains(&role.as_str()))
}

/// 対象要素自身の子孫テキストから accessible name を算出する（name from
/// content。accname 1.2 の 2F。`AISNAP-1`・TASK-11.4.3）。
///
/// 走査は 1 回あたり [`MAX_CONTENT_STEPS`] で頭打ちにする（要素ごとに固定で、
/// 呼び出し順に依存しない）。戻り値の契約は
/// [`label_name`] と同じ:
/// - テキストあり: `Some`（`source: Content`。打ち切りがあれば `truncated`）
/// - テキストなし・打ち切りなし: `None`（次点の `title` へフォールバックしてよい）
/// - テキストなし・打ち切りあり: `Some` の空の名前（`truncated: true`。走査しきれて
///   いない以上「名前なし」と確定できないため `title` へフォールバックさせない）
fn content_name(doc: &Document, id: NodeId) -> Option<AccessibleName> {
    let mut text = String::new();
    let scan = collect_content_text(
        doc,
        id,
        id,
        ContentWalk {
            max_steps: MAX_CONTENT_STEPS,
            include_hidden: false,
        },
        &mut text,
    );

    let mut buf = NameBuffer::new();
    buf.push_str(&text);
    let truncated = scan.cut || buf.truncated;
    if buf.text.is_empty() {
        return truncated.then(|| AccessibleName::default().with_truncated(true));
    }
    let mut result = buf.finish(NameSource::Content);
    result.truncated = truncated;
    Some(result)
}

/// `aria-label` 属性から accessible name を算出する（accname 1.2 step 2C。
/// `AISNAP-1`・TASK-11.4.1）。
///
/// 属性が無い、または空白しか含まない場合は `None`（次の出所へフォール
/// バックさせる）。値は [`NameBuffer`] で空白を折り畳み [`MAX_NAME_CHARS`]
/// で切り詰める。
fn aria_label_name(doc: &Document, id: NodeId) -> Option<AccessibleName> {
    attr_name(doc, id, "aria-label", NameSource::AriaLabel)
}

/// `aria-labelledby` の参照先 `referent` の名前の断片テキストを返す
/// （`AISNAP-1`・TASK-11.4.1）。
///
/// 優先順: 参照先自身の `aria-label`（空白以外を含む場合。accname 2C）→
/// 参照先が `img` の場合その `alt`（参照先自身は子孫走査の対象外で `alt` が
/// 失われるための補完）→ 子孫テキスト（[`collect_content_text`]）。参照先自身の
/// `aria-labelledby` は辿らない（連鎖・循環を構造的に避ける）。`target`
/// （名前を算出している要素）の部分木は子孫走査から除外する（参照先が
/// 対象を含む場合に対象の値・内容を取り込まないため）。
///
/// 属性値も含め、折り畳み後 [`NORMALIZED_LABEL_TEXT_CHAR_LIMIT`] 文字までしか
/// `String` へ取り込まない（属性長に比例したメモリ確保を避ける）。
///
/// 対象を含まない参照先は結果を [`NameIndex`] にキャッシュし、同じ参照先を
/// 多数の対象が参照しても部分木走査は 1 回で済ませる。対象を含む参照先は
/// キャッシュできないため [`MAX_UNCACHED_REFERENT_SCANS`] で総回数を頭打ちにする。
/// 対象を含まない参照先の初回走査も [`MAX_CACHEABLE_REFERENT_SCANS`] で文書全体の
/// 総回数を頭打ちにする（異なる id の深い部分木を大量に参照する入力対策）。
/// いずれも使い切った後は `None`（呼び出し元は寄与なし＋`truncated` として扱う）。
fn referent_text(
    doc: &Document,
    index: &NameIndex,
    referent: NodeId,
    target: NodeId,
) -> Option<(String, bool)> {
    let contains_target = index.is_ancestor_or_self(referent, target);

    // 対象を含まない参照先だけがキャッシュを共有できる。対象を含む参照先は
    // 対象の部分木を除外して走査する必要があり、結果が対象ごとに異なるため、
    // キャッシュ（他の対象で作った断片）を読まない（`compute_name` と一致させる）。
    if !contains_target && let Some(cached) = index.referent_cache.borrow().get(&referent) {
        return Some(cached.clone());
    }

    let mut out = String::new();
    // 対象を含む参照先（キャッシュ不能）と含まない参照先（初回走査）で
    // 別々の文書全体予算を消費する。使い切ったら `None`（寄与なし+truncated）。
    let budget = if contains_target {
        &index.uncached_scans_left
    } else {
        &index.first_scans_left
    };
    let consume_budget = || {
        let left = budget.get();
        if left == 0 {
            return false;
        }
        budget.set(left - 1);
        true
    };
    // 参照先自身の `aria-label` / `img` の `alt` / テキスト系 `input` の値は
    // 部分木を走査せず対象にも依存しないため、走査予算を消費しない。
    // 一方 `select` の option 探索は部分木を辿るため、1 回ごとに予算を消費し
    // （多数の `select` × 大量の `option` で総走査量が二乗になるのを防ぐ）、
    // 1 回あたりの走査も `MAX_CONTENT_STEPS` で頭打ちにする。
    if is_html_element_named(doc, referent, "select") && !consume_budget() {
        return None;
    }
    let cut = if let Some(own_cut) = scan_referent_own_text(doc, referent, &mut out) {
        // 参照先自身の値を読む経路（select の選択 option 探索など）が予算で
        // 打ち切られた場合も、不完全な値を確定値として返さないよう伝播する。
        own_cut
    } else {
        if !consume_budget() {
            return None;
        }
        // accname 2A: 直接参照された hidden な参照先は、その hidden な子孫も
        // 含めて寄与する（参照先の内側だけ `include_hidden`）。
        let scan = collect_content_text(
            doc,
            referent,
            target,
            ContentWalk {
                max_steps: doc.node_count(),
                include_hidden: is_hidden_element(doc, referent),
            },
            &mut out,
        );
        scan.cut
    };
    if !contains_target {
        index
            .referent_cache
            .borrow_mut()
            .insert(referent, (out.clone(), cut));
    }
    Some((out, cut))
}

/// [`referent_text`] の補助。参照先自身の属性（`aria-label`、`img` の `alt`）から
/// 名前の断片を `out` へ取り込めたら `Some(cut)`（部分木の走査は不要）。
/// `cut` は参照先自身の値の読み取りが走査上限で打ち切られたか
/// （`select` の option 探索）。取り込めなければ `None`。
/// 対象要素に依存しないためキャッシュ対象。走査予算は呼び出し元（[`referent_text`]）が
/// `select` に限り消費し、1 回あたりの走査は [`MAX_CONTENT_STEPS`] で上限する。
fn scan_referent_own_text(doc: &Document, referent: NodeId, out: &mut String) -> Option<bool> {
    // 参照先自身が埋め込みコントロール（accname 2E）なら、値を `aria-label` より
    // 優先する（子孫走査側の 2E と結果を一致させる。PR #574 レビュー指摘）。
    // 値が空で打ち切りも無い場合だけ `aria-label` / `alt` へ譲る。
    // 参照先の label・title は未実装（担当 Issue 未確定）。
    let embedded = embedded_control_text(doc, referent, referent, MAX_CONTENT_STEPS);
    if let Some((text, scan)) = &embedded
        && (has_non_whitespace(text) || scan.cut)
    {
        fold_attr_bounded(text, out);
        return Some(scan.cut);
    }
    if let Some(label) = doc.attribute(referent, "aria-label")
        && has_non_whitespace(label)
    {
        fold_attr_bounded(label, out);
        return Some(false);
    }
    if is_html_element_named(doc, referent, "img")
        && let Some(alt) = doc.attribute(referent, "alt")
    {
        fold_attr_bounded(alt, out);
        return Some(false);
    }
    if let Some((text, scan)) = embedded {
        fold_attr_bounded(&text, out);
        return Some(scan.cut);
    }
    None
}

/// 属性値 `value` を空白折り畳みしながら、折り畳み後
/// [`NORMALIZED_LABEL_TEXT_CHAR_LIMIT`] 文字までを `out`（空）へ取り込む。
fn fold_attr_bounded(value: &str, out: &mut String) {
    let mut normalized_chars = 0usize;
    let mut pending_space = false;
    fold_chars_into(
        value.chars(),
        out,
        &mut normalized_chars,
        &mut pending_space,
        NORMALIZED_LABEL_TEXT_CHAR_LIMIT,
    );
}

/// `aria-labelledby` から accessible name を算出する（accname 1.2 step 2B。
/// `AISNAP-1`・TASK-11.4.1）。
///
/// 属性値を HTML ASCII 空白（`split_ascii_whitespace`。`\x0B` は区切りに
/// 含まれない）でトークン化し、トークン順（文書順ではない）に参照先の
/// テキストを半角スペース 1 個で連結する。存在しない id・空の参照先は
/// 飛ばし、[`MAX_IDREFS`] の枠も消費しない。同じトークンの重複は
/// それぞれ 1 件として扱う。IDREF 解決は [`NameIndex`] のハッシュ参照
/// だけで行い、文書を再走査しない。
///
/// 戻り値（[`label_name`] と同じ契約）:
/// - 属性が無い、または寄与する参照先が 0 件で打ち切りも無い: `None`
///   （呼び出し元は `aria-label` 等の次点へフォールバックする）
/// - 寄与が 0 件のまま [`MAX_IDREFS_SCANNED`] で打ち切った: `Some` の空の
///   名前（`truncated: true`。フォールバックさせない）
/// - 寄与が 1 件以上: `source: AriaLabelledBy`。[`MAX_IDREFS`] 超過分・
///   走査上限超過があれば `truncated: true`
fn aria_labelledby_name(
    doc: &Document,
    index: &NameIndex,
    target: NodeId,
) -> Option<AccessibleName> {
    let value = doc.attribute(target, "aria-labelledby")?;

    let mut buf = NameBuffer::new();
    let mut matched = 0usize;
    let mut truncated = false;
    let mut scan_cut_before_match = false;

    for (scanned, token) in value.split_ascii_whitespace().enumerate() {
        if scanned >= MAX_IDREFS_SCANNED {
            truncated = true;
            scan_cut_before_match = matched == 0;
            break;
        }
        let Some(&referent) = index.ids.get(token) else {
            continue;
        };
        let Some((text, cut)) = referent_text(doc, index, referent, target) else {
            // 走査予算を使い切った参照先は内容を確定できないため、寄与なしと
            // して扱い切り詰めを伝える（`MAX_UNCACHED_REFERENT_SCANS`）。
            truncated = true;
            if matched == 0 {
                scan_cut_before_match = true;
            }
            continue;
        };
        truncated |= cut;
        if !has_non_whitespace(&text) {
            if cut {
                // 参照先の走査が打ち切られて文字を得られなかった場合は名前が
                // 未確定のため、寄与が 0 件のまま終われば次の命名規則へ
                // フォールバックさせない（PR #574 レビュー指摘）。
                scan_cut_before_match = true;
            }
            continue;
        }
        if matched >= MAX_IDREFS {
            truncated = true;
            continue;
        }
        matched += 1;
        buf.push_separator();
        buf.push_str(&text);
    }

    if matched == 0 {
        if scan_cut_before_match {
            return Some(AccessibleName::default().with_truncated(true));
        }
        return None;
    }

    let mut result = buf.finish(NameSource::AriaLabelledBy);
    if truncated {
        result.truncated = true;
    }
    Some(result)
}

/// `input` 要素（`hidden` を除く）の accessible name を算出する
/// （HTML-AAM。ARIA 段を除く。`AISNAP-1`）。
///
/// `type` ごとの節分けは本モジュール冒頭のドキュメンテーションコメント
/// 「HTML-AAM による要素ごとの算出順序」の表を参照。
fn input_name(doc: &Document, index: &NameIndex, id: NodeId) -> AccessibleName {
    match normalized_input_type(doc, id).as_str() {
        "hidden" => AccessibleName::default(),
        // checkbox/radio に加え、text 系の節（本モジュール冒頭の表）に
        // 明示列挙されていない他の type（range・color・date・
        // datetime-local・month・week・time・file 等）も HTML-AAM の
        // "Other Form Elements" 節（label → title。`placeholder` 段は
        // ない）に従う。`type` 省略・未知の値だけは HTML の既定である
        // text 系として扱い、下の `_` 節（`placeholder` を含む）へ渡す。
        "checkbox" | "radio" | "range" | "color" | "date" | "datetime-local" | "month" | "week"
        | "time" | "file" => label_name(doc, index, id)
            .or_else(|| title_name(doc, id))
            .unwrap_or_default(),
        "button" => label_name(doc, index, id)
            .or_else(|| value_attr_name(doc, id))
            .or_else(|| title_name(doc, id))
            .unwrap_or_default(),
        ty @ ("submit" | "reset") => {
            if let Some(name) = label_name(doc, index, id) {
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
            if let Some(name) = label_name(doc, index, id) {
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
        _ => label_name(doc, index, id)
            .or_else(|| title_name(doc, id))
            .or_else(|| placeholder_name(doc, id))
            .unwrap_or_default(),
    }
}

/// `doc` の要素 `id` から、HTML ネイティブの出所のみで accessible name を
/// 算出する（`AISNAP-1`・TASK-11.4.2）。ARIA 属性の段は含まない
/// （[`compute_name_with_index`] が先に評価する）。
fn native_name(doc: &Document, index: &NameIndex, id: NodeId) -> AccessibleName {
    if is_html_element_named(doc, id, "img") {
        return img_name(doc, id);
    }
    if is_html_element_named(doc, id, "area") {
        return area_name(doc, id);
    }
    if is_html_element_named(doc, id, "input") {
        return input_name(doc, index, id);
    }
    if is_html_element_named(doc, id, "textarea") {
        return label_name(doc, index, id)
            .or_else(|| title_name(doc, id))
            .or_else(|| placeholder_name(doc, id))
            .unwrap_or_default();
    }
    if is_html_element_named(doc, id, "button") {
        // HTML-AAM の button 節: label → 子孫テキスト → title。
        return label_name(doc, index, id)
            .or_else(|| content_name(doc, id))
            .or_else(|| title_name(doc, id))
            .unwrap_or_default();
    }
    if is_html_element_named(doc, id, "select")
        || is_html_element_named(doc, id, "meter")
        || is_html_element_named(doc, id, "output")
        || is_html_element_named(doc, id, "progress")
    {
        return label_name(doc, index, id)
            .or_else(|| title_name(doc, id))
            .unwrap_or_default();
    }
    // name from content を許す role（link・heading・cell 等）は子孫テキスト →
    // title、それ以外は title のみ（accname 2F → 2I）。
    if allows_name_from_content(doc, id) {
        return content_name(doc, id)
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
/// 実装しているのは ARIA 属性（`aria-labelledby`・`aria-label`。
/// TASK-11.4.1）・HTML ネイティブのラベル付け（TASK-11.4.2）・子孫テキスト
/// と文書ルートの `<title>`（TASK-11.4.3）である。本ファイル冒頭の
/// ドキュメンテーションコメントに、未実装の項目を列挙してある
/// （実装済みを装わない。REPAIR-3）。
///
/// 要素以外（テキストノード等）・範囲外の `id` では `AccessibleName::default()`
/// を返す（`Result` にはしない。`core::dom` のアクセサ群・
/// [`super::state::compute_state`] と同じ「範囲外・対象外は `None`/既定値」の
/// 契約に合わせる）。
///
/// # 計算量についての注意（PR #567 レビュー指摘の P1 修正）
///
/// 本関数は呼び出しのたびに [`NameIndex::build`] で索引を新規構築する
/// **単発呼び出し向けの簡易版**である。1 つの文書の複数要素へ連続して
/// 名前を算出する呼び出し元（TASK-11.7 のツリー構築等）がこれを要素ごとに
/// 呼ぶと、索引構築が要素数だけ繰り返され計算量が文書サイズの二乗になる。
/// そのような呼び出し元は、文書ごとに [`NameIndex::build`] を 1 回だけ
/// 呼び、要素ごとには [`compute_name_with_index`] を使うこと。
///
/// 呼び出し文脈: 現時点では呼び出し元がない。TASK-11.7（Issue #76）が
/// DOM から `Snapshot`/`Node` を構築する際、算出結果の `text` を
/// [`super::Node::name`] へ格納する想定である。
pub fn compute_name(doc: &Document, id: NodeId) -> AccessibleName {
    let index = NameIndex::build(doc);
    compute_name_with_index(doc, &index, id)
}

/// 文書ルートの名前（最初の HTML `<title>` の子テキスト。`AISNAP-1`・
/// TASK-11.4.3）。`<title>` が無ければ既定値。
fn document_title_name(doc: &Document, index: &NameIndex) -> AccessibleName {
    let Some(title) = index.first_title else {
        return AccessibleName::default();
    };
    let mut text = String::new();
    let scan = collect_content_text(
        doc,
        title,
        title,
        ContentWalk {
            max_steps: doc.node_count(),
            include_hidden: true,
        },
        &mut text,
    );
    let mut buf = NameBuffer::new();
    buf.push_str(&text);
    let truncated = scan.cut || buf.truncated;
    let mut result = buf.finish(NameSource::DocumentTitle);
    result.truncated = truncated && !result.text.is_empty();
    result
}

/// [`compute_name`] の索引再利用版（`AISNAP-1`・PR #567 レビュー指摘の
/// P1 修正）。
///
/// `index` は呼び出し元が `doc` について [`NameIndex::build`] で 1 回だけ
/// 構築し、同じ文書の要素ごとに使い回すことを想定する（文書ごとに
/// 索引を再構築すると [`compute_name`] と同じ計算量に戻ってしまうため、
/// ループの外側で 1 回だけ構築すること）。`doc` と `index` は同じ文書から
/// 構築したものでなければならない。[`NodeId`] は文書内の番号に過ぎず
/// 別文書の `NodeId` と衝突しうるため、異なる文書の組み合わせを渡すと
/// `labels_by_target` が無関係の要素に一致し**誤ったラベルを適用しうる**
/// （PR #567 レビュー指摘の P2 修正・修正前の状態）。本関数はその誤用を
/// 実行時に検出し、`doc` と `index` の構築元が同一でなければ（`ptr::eq`
/// による同一性検証）名前なし（[`AccessibleName::default`]）を返す。
pub fn compute_name_with_index(doc: &Document, index: &NameIndex, id: NodeId) -> AccessibleName {
    if !std::ptr::eq(doc, index.doc) {
        return AccessibleName::default();
    }
    // 文書ルートは最初の HTML `<title>` の子テキストで命名する（AISNAP-1・
    // TASK-11.4.3）。要素判定より前に置かないとルートに届かない。
    if id == doc.root() {
        return document_title_name(doc, index);
    }
    if !doc.is_element(id) {
        return AccessibleName::default();
    }
    // `input[type=hidden]` はアクセシビリティツリーへ公開されないため、
    // ARIA 属性があっても常に名前なしとする（既存の契約を維持）。
    if is_html_element_named(doc, id, "input") && normalized_input_type(doc, id) == "hidden" {
        return AccessibleName::default();
    }
    // accname 1.2 の順序: 2B `aria-labelledby` → 2C `aria-label` → 2D ネイティブ。
    if let Some(name) = aria_labelledby_name(doc, index, id) {
        return name;
    }
    if let Some(name) = aria_label_name(doc, id) {
        return name;
    }
    native_name(doc, index, id)
}

#[cfg(test)]
mod tests {
    use super::{
        AccessibleName, MAX_CACHEABLE_REFERENT_SCANS, MAX_IDREFS, MAX_IDREFS_SCANNED, MAX_LABELS,
        MAX_LABELS_SCANNED, MAX_NAME_CHARS, MAX_UNCACHED_REFERENT_SCANS, NameIndex, NameSource,
        compute_name, compute_name_with_index,
    };
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

    /// AISNAP-1（TASK-11.4.2・#545・PR #567 レビュー指摘の P1 修正）:
    /// `label` 内が `img` だけの場合（`for` 関連付け）、`img` の `alt` が
    /// 名前として使われる（画像ラベルの入力名が失われない）。
    #[test]
    fn aisnap_1_label_for_img_alt_is_collected() {
        let result = name(
            r#"<label for="q"><img alt="検索"></label><input id="q">"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "検索".to_string(),
                source: NameSource::Label,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545・PR #567 レビュー指摘の P1 修正）:
    /// `label` 内が `img` だけの場合（包含）でも `alt` が名前として使われる。
    #[test]
    fn aisnap_1_label_wrapping_img_alt_is_collected() {
        let result = name(
            r#"<label><img alt="検索"><input type="checkbox"></label>"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "検索".to_string(),
                source: NameSource::Label,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545・PR #567 レビュー指摘の P1 修正）:
    /// `label` 内のテキストと `img` の `alt` が両方ある場合、文書順に
    /// テキストノードと同列に連結される（区切りの空白は挿入しない。
    /// 隣接するテキストノード同士を連結する扱いと同じ規則）。
    #[test]
    fn aisnap_1_label_text_and_img_alt_are_concatenated_in_order() {
        let result = name(
            r#"<label for="q">前<img alt="中">後</label><input id="q">"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "前中後".to_string(),
                source: NameSource::Label,
                truncated: false,
            }
        );
    }

    /// AISNAP-1（TASK-11.4.2・#545・PR #567 レビュー指摘の P1 修正）:
    /// `label` 内にテキストと `alt=""` の `img` が両方ある場合、`img` は
    /// 何も追加しない（末尾に余分な空白が残らない）。
    #[test]
    fn aisnap_1_label_text_with_empty_img_alt_has_no_trailing_space() {
        let result = name(
            r#"<label for="q">名前<img alt=""></label><input id="q">"#,
            "input",
        );
        assert_eq!(
            result,
            AccessibleName {
                text: "名前".to_string(),
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

    /// AISNAP-1（TASK-11.4.2・#545）: `type` の大文字小文字は区別せず
    /// 正規化する（前後空白は除去しない。PR #567 レビュー指摘の P1 修正）。
    #[test]
    fn aisnap_1_input_type_case_is_normalized() {
        let result = name(
            r##"<label for="x"> CheckBox </label><input type="CHECKBOX" id="x">"##,
            "input",
        );
        assert_eq!(result.text, "CheckBox");
        assert_eq!(result.source, NameSource::Label);
    }

    /// AISNAP-1（TASK-11.4.2・#545・PR #567 レビュー指摘の P1 修正）:
    /// `type` 前後に空白がある値（`type=" hidden "`）は HTML Standard の
    /// キーワード照合に一致しない無効値として text 状態にフォールバック
    /// する。前後空白を除去して `hidden` と誤判定すると label があっても
    /// 名前なしになってしまうが、正しくは label から名前を得られる。
    #[test]
    fn aisnap_1_input_type_with_surrounding_whitespace_is_invalid_value() {
        let result = name(
            r##"<label for="x">トークン</label><input type=" hidden " id="x">"##,
            "input",
        );
        assert_eq!(result.text, "トークン");
        assert_eq!(result.source, NameSource::Label);
    }

    /// AISNAP-1（TASK-11.4.2・#545・PR #567 レビュー指摘の P1 修正）:
    /// `type=" submit "` も前後空白を含むため無効値 → text 状態として
    /// 扱い、label が無ければ `submit` の既定ラベル "Submit" ではなく
    /// `placeholder` から名前を取る（text 系専用の折り込み先）。
    #[test]
    fn aisnap_1_input_type_submit_with_whitespace_falls_back_to_placeholder() {
        let result = name(r##"<input type=" submit " placeholder="送信">"##, "input");
        assert_eq!(result.text, "送信");
        assert_eq!(result.source, NameSource::Placeholder);
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

    /// AISNAP-1（TASK-11.4.2・#545）: `MAX_LABELS` を超える数の label が
    /// 関連付いていても、そのうち空・空白だけの label は上限枠を占めない。
    /// 空の label が `MAX_LABELS` 個続いた後に名前入りの label が続く場合、
    /// その名前を切り捨てず取得できる（PR #567 レビュー指摘の P1 回帰
    /// テスト: 従来は空の label が上限を使い切り、後続の実名が失われた
    /// うえ `title` 等へ誤ってフォールバックしていた）。
    #[test]
    fn aisnap_1_empty_labels_do_not_consume_max_labels_budget() {
        let mut html = String::new();
        for _ in 0..MAX_LABELS {
            html.push_str(r#"<label for="x">   </label>"#);
        }
        html.push_str(r#"<label for="x">本当の名前</label><input id="x" title="使われないはず">"#);
        let result = name(&html, "input");
        assert_eq!(result.text, "本当の名前");
        assert_eq!(result.source, NameSource::Label);
        assert!(!result.truncated);
    }

    /// AISNAP-1（TASK-11.4.2・#545）: 寄与する label が `MAX_LABELS` を
    /// 超えるときは、間に空・空白だけの label が挟まっていても正しく
    /// `truncated` が立つ（寄与する label だけを数える、という仕様の
    /// 反対側の境界）。
    #[test]
    fn aisnap_1_max_labels_counts_only_contributing_labels() {
        let mut html = String::new();
        for _ in 0..(MAX_LABELS + 1) {
            html.push_str(r#"<label for="x">a</label><label for="x">   </label>"#);
        }
        html.push_str(r#"<input id="x">"#);
        let result = name(&html, "input");
        assert!(result.truncated);
        assert_eq!(
            result.text.chars().filter(|&c| c == 'a').count(),
            MAX_LABELS
        );
    }

    // --- NameIndex（複数要素での索引再利用） ---

    /// AISNAP-1（TASK-11.4.2・#545）: `NameIndex::build` を 1 回だけ構築し
    /// `compute_name_with_index` を複数要素へ使い回しても、各要素は
    /// `compute_name`（単発版）と同じ結果になる（PR #567 レビュー指摘の
    /// P1 修正: 索引を使った経路が単発経路と食い違わないことの確認）。
    #[test]
    fn aisnap_1_compute_name_with_index_matches_single_shot_for_each_element() {
        use super::{NameIndex, compute_name_with_index};

        let html = r#"
            <label for="a">名前A</label><input type="checkbox" id="a">
            <label>名前B <input type="checkbox" id="b"></label>
            <input type="text" id="c" title="タイトルC">
            <span id="s1">参照A</span>
            <input type="text" id="d" aria-labelledby="s1 c" aria-label="無視される">
            <button id="e" aria-label="閉じる">x</button>
        "#;
        let parsed =
            parse_document(html, &ParseOptions::default()).expect("テスト入力は必ず成功する");
        let doc = parsed.document;
        let root = doc.root();
        let index = NameIndex::build(&doc);

        for selector in ["input#a", "input#b", "input#c", "input#d", "button#e"] {
            let target = query_selector_str(&doc, root, selector)
                .expect("セレクタは解釈できる")
                .expect("対象要素が見つかる");
            assert_eq!(
                compute_name_with_index(&doc, &index, target),
                compute_name(&doc, target),
                "selector={selector}"
            );
        }
    }

    /// AISNAP-1（TASK-11.4.2・#545・PR #567 レビュー指摘の P2 修正）:
    /// 構築元と異なる文書の `NodeId` を `compute_name_with_index` へ渡すと、
    /// `NodeId` が文書内の番号に過ぎず両文書で衝突しても、無関係な文書の
    /// ラベルが誤って適用されず名前なしになる（修正前は `labels_by_target`
    /// が一致し、`doc_b` の label テキストが `doc_a` の要素に誤って
    /// 適用されていた）。
    #[test]
    fn aisnap_1_compute_name_with_index_rejects_mismatched_document() {
        use super::{NameIndex, compute_name_with_index};

        // 構造を揃えることで、両文書で同じ `NodeId`（arena インデックス）が
        // 同じ構造上の位置を指すようにする（衝突を意図的に起こす）。
        let html_a = r#"<label for="x">A用の名前</label><input id="x">"#;
        let html_b = r#"<label for="x">B用の名前</label><input id="x">"#;
        let parsed_a =
            parse_document(html_a, &ParseOptions::default()).expect("テスト入力は必ず成功する");
        let parsed_b =
            parse_document(html_b, &ParseOptions::default()).expect("テスト入力は必ず成功する");
        let doc_a = parsed_a.document;
        let doc_b = parsed_b.document;
        let root_a = doc_a.root();
        let target_a = query_selector_str(&doc_a, root_a, "input")
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる");

        // 前提確認: `doc_a` 単体では期待どおり `A用の名前` が算出できる。
        assert_eq!(compute_name(&doc_a, target_a).text, "A用の名前");

        let index_b = NameIndex::build(&doc_b);
        let result = compute_name_with_index(&doc_a, &index_b, target_a);
        assert_eq!(
            result,
            AccessibleName::default(),
            "異なる文書から構築した index を渡すと名前なしになるべき（B の名前が誤って適用されてはならない）"
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

    // --- PR #567 レビュー指摘の P1 修正（回帰テスト） ---

    /// PR #567 レビュー指摘の P1 修正: label 本体の前に、折り畳み前の生
    /// バイト数で 480 バイトを超える HTML 空白があっても、本体
    /// （非空白テキスト）が失われず名前として使われる。旧実装は生バイト数
    /// で上限判定していたため、本体に到達する前に打ち切られ、`label` から
    /// 空の名前が確定して `title` へ誤ってフォールバックしていた。
    #[test]
    fn aisnap_1_label_long_leading_whitespace_does_not_lose_trailing_name() {
        let mut html = String::from(r#"<label for="x">"#);
        // タブ 1000 個（1 バイト/文字）。旧実装の折り畳み前バイト上限
        // （`MAX_NAME_CHARS * 4 = 480`）を大きく超える。
        html.push_str(&"\t".repeat(1000));
        html.push_str("実際の名前");
        html.push_str(r#"</label><input id="x" title="使われないはず">"#);

        let result = name(&html, "input");
        assert_eq!(result.text, "実際の名前");
        assert_eq!(result.source, NameSource::Label);
    }

    /// PR #567 レビュー指摘の P1 修正: `MAX_LABELS`（名前に寄与した label
    /// の数）には大量の空・空白だけの label を並べても達しないが、
    /// [`MAX_LABELS_SCANNED`] により走査自体（`collect_content_text` の呼び
    /// 出し回数）は定数で打ち切られる。走査上限より後ろに置いた名前入り
    /// label には到達できず、`truncated` が立つ（外部入力の label 数に
    /// 比例して処理量が無制限に増え続けないことの確認）。
    ///
    /// 走査上限より前に実際に寄与する label（`"先頭"`）を 1 つ置くことで、
    /// 打ち切りが発生しても `truncated` が呼び出し元まで伝わることを
    /// 確認する。寄与する label が 1 つも無いまま走査が打ち切られた場合
    /// （`truncated` が失われずに伝わることの確認）は、後続の
    /// [`aisnap_1_max_labels_scanned_before_any_match_does_not_fall_back`]
    /// を参照（PR #567 レビュー指摘の P1 再修正）。
    #[test]
    fn aisnap_1_max_labels_scanned_bounds_empty_label_scan() {
        let mut html = String::from(r#"<label for="x">先頭</label>"#);
        for _ in 0..MAX_LABELS_SCANNED {
            html.push_str(r#"<label for="x">   </label>"#);
        }
        html.push_str(r#"<label for="x">届かないはずの名前</label><input id="x">"#);

        let result = name(&html, "input");
        assert_eq!(result.text, "先頭");
        assert_eq!(result.source, NameSource::Label);
        assert!(result.truncated);
    }

    /// PR #567 レビュー指摘の P1 再修正（回帰テスト）: 寄与する label が
    /// 1 つも無いまま [`MAX_LABELS_SCANNED`] 件で走査が打ち切られた場合、
    /// それより後ろにある唯一の名前入り label へ到達できなくても
    /// `title` 属性へ誤ってフォールバックしない。旧実装は
    /// `matched_labels == 0` を「本当に名前が無い」と同一視して `None` を
    /// 返し、`input_name` の `.or_else(|| title_name(..))` が `title` の
    /// 値を確定してしまっていた。
    #[test]
    fn aisnap_1_max_labels_scanned_before_any_match_does_not_fall_back() {
        let mut html = String::new();
        for _ in 0..MAX_LABELS_SCANNED {
            html.push_str(r#"<label for="x">   </label>"#);
        }
        html.push_str(
            r#"<label for="x">届かないはずの名前</label><input id="x" title="使われないはず">"#,
        );

        let result = name(&html, "input");
        assert_ne!(
            result.text, "使われないはず",
            "走査打ち切りで名前不確定のまま title へフォールバックしてはならない"
        );
        assert_eq!(result.text, "");
        assert_eq!(result.source, NameSource::None);
        assert!(result.truncated);
    }

    /// PR #567 レビュー指摘の P1 再修正（回帰テスト・負例）: 走査上限
    /// （[`MAX_LABELS_SCANNED`]）未満の個数の空・空白だけの label しか
    /// 無く、名前入りの label も存在しない場合は、従来どおり `title` へ
    /// 正しくフォールバックする（打ち切り時の特別扱いが、通常の
    /// 「名前が本当に無い」ケースまで壊していないことの確認）。
    #[test]
    fn aisnap_1_few_empty_labels_without_scan_cutoff_still_falls_back_to_title() {
        let mut html = String::new();
        for _ in 0..3 {
            html.push_str(r#"<label for="x">   </label>"#);
        }
        html.push_str(r#"<input id="x" title="title の名前">"#);

        let result = name(&html, "input");
        assert_eq!(result.text, "title の名前");
        assert_eq!(result.source, NameSource::Title);
        assert!(!result.truncated);
    }

    /// PR #567 レビュー指摘の P1 修正: `for` 属性を持たない包含
    /// （wrapping）label が深く入れ子になっていても、`NameIndex::build`
    /// は文書サイズに対して線形時間で完了する（旧実装は label ごとに
    /// `doc.descendants` で子孫を再走査しており、入れ子数に対して二乗
    /// 時間になっていた）。
    #[test]
    fn aisnap_1_nested_wrapping_labels_build_is_linear_not_quadratic() {
        use std::time::Instant;

        use super::NameIndex;

        const DEPTH: usize = 4_000;
        let mut html = String::new();
        for _ in 0..DEPTH {
            html.push_str("<label>");
        }
        html.push_str(r#"<input id="x">"#);
        for _ in 0..DEPTH {
            html.push_str("</label>");
        }

        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let parsed = parse_document(&html, &options).expect("深いネストでも成功する");
        let doc = parsed.document;

        let started = Instant::now();
        let index = NameIndex::build(&doc);
        let elapsed = started.elapsed();
        // 二乗時間への退行なら DEPTH=4,000 は著しく遅くなる。線形なら
        // 十分速く終わるはずなので、余裕を持った上限で退行を検知する。
        assert!(
            elapsed.as_secs() < 5,
            "NameIndex::build が遅すぎる（{elapsed:?}）。O(n^2) への退行の疑い"
        );

        // 構築結果も使えること（クラッシュ・無限ループしないことに加えて
        // 妥当な結果を返すこと）を確認する。
        let root = doc.root();
        let input = query_selector_str(&doc, root, "input")
            .expect("セレクタは解釈できる")
            .expect("input 要素が見つかる");
        let result = super::compute_name_with_index(&doc, &index, input);
        // すべての label が空要素（テキストなし）のため名前には寄与しない。
        assert!(result.is_empty());
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 入退場番号による祖先判定が親リンク走査と
    /// 一致する（兄弟・子孫・自身・祖先で期待値どおり）。
    #[test]
    fn aisnap_1_enter_exit_span_ancestor_check_matches_tree_shape() {
        use super::NameIndex;
        let options = ParseOptions::default();
        let parsed = parse_document(
            r#"<div id="a"><p id="b"><span id="c">x</span></p><p id="d">y</p></div>"#,
            &options,
        )
        .expect("パースに成功する");
        let doc = parsed.document;
        let index = NameIndex::build(&doc);
        let find = |sel: &str| {
            query_selector_str(&doc, doc.root(), sel)
                .expect("セレクタは解釈できる")
                .expect("要素が見つかる")
        };
        let (a, b, c, d) = (find("#a"), find("#b"), find("#c"), find("#d"));
        assert!(index.is_ancestor_or_self(a, c));
        assert!(index.is_ancestor_or_self(b, c));
        assert!(index.is_ancestor_or_self(c, c));
        assert!(!index.is_ancestor_or_self(c, b));
        assert!(!index.is_ancestor_or_self(d, c));
        assert!(!index.is_ancestor_or_self(b, d));
    }

    // --- ARIA（TASK-11.4.1・#544） ---

    fn named(text: &str, source: NameSource, truncated: bool) -> AccessibleName {
        AccessibleName {
            text: text.to_string(),
            source,
            truncated,
        }
    }

    /// AISNAP-1（TASK-11.4.1・#544）: `aria-label` を指定した要素はその値が名前になる。
    #[test]
    fn aisnap_1_aria_label_basic() {
        let result = name(r#"<button aria-label="閉じる">×</button>"#, "button");
        assert_eq!(result, named("閉じる", NameSource::AriaLabel, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 空白だけの `aria-label` は無視され次点へ進む。
    #[test]
    fn aisnap_1_aria_label_blank_falls_back_to_native() {
        let result = name(r#"<input type="text" aria-label="  " title="題">"#, "input");
        assert_eq!(result, named("題", NameSource::Title, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: `aria-label` の空白は畳まれ前後は除去される。
    #[test]
    fn aisnap_1_aria_label_whitespace_normalized() {
        let result = name("<div aria-label=\"  a \t b\n\nc  \">x</div>", "div");
        assert_eq!(result, named("a b c", NameSource::AriaLabel, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: `aria-label` は `label[for]` と `img` の `alt`（空を含む）より優先。
    #[test]
    fn aisnap_1_aria_label_beats_native() {
        let result = name(
            r#"<label for="i">ラベル</label><input id="i" aria-label="ARIA">"#,
            "input",
        );
        assert_eq!(result, named("ARIA", NameSource::AriaLabel, false));
        let result = name(r#"<img alt="" aria-label="ロゴ">"#, "img");
        assert_eq!(result, named("ロゴ", NameSource::AriaLabel, false));
        let result = name(r#"<img alt="代替" aria-label="ロゴ">"#, "img");
        assert_eq!(result, named("ロゴ", NameSource::AriaLabel, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 上限超の `aria-label` は切り詰めて `truncated`。
    #[test]
    fn aisnap_1_aria_label_truncated() {
        let long = "あ".repeat(MAX_NAME_CHARS + 1);
        let result = name(&format!(r#"<div aria-label="{long}">x</div>"#), "div");
        assert_eq!(
            result,
            named(&"あ".repeat(MAX_NAME_CHARS), NameSource::AriaLabel, true)
        );
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 複数 IDREF はトークン順（文書順ではない）に連結し、存在しない id は飛ばす。
    #[test]
    fn aisnap_1_aria_labelledby_token_order() {
        let result = name(
            r#"<span id="a">一</span><span id="b">二</span><span id="c">三</span>
               <div id="t" aria-labelledby="c missing a b">x</div>"#,
            "div#t",
        );
        assert_eq!(result, named("三 一 二", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: タブ・改行区切りのトークンも分割される。
    #[test]
    fn aisnap_1_aria_labelledby_tab_newline_separators() {
        let result = name(
            "<span id=\"a\">一</span><span id=\"b\">二</span><div id=\"t\" aria-labelledby=\"a\tb\nb\">x</div>",
            "div#t",
        );
        assert_eq!(result, named("一 二 二", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: `aria-labelledby` は `aria-label` より優先。
    #[test]
    fn aisnap_1_aria_labelledby_beats_aria_label() {
        let result = name(
            r#"<span id="a">参照</span><div id="t" aria-labelledby="a" aria-label="直接">x</div>"#,
            "div#t",
        );
        assert_eq!(result, named("参照", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 全参照が無効なら `aria-label`、それも無ければネイティブへ。
    #[test]
    fn aisnap_1_aria_labelledby_empty_falls_back() {
        let result = name(
            r#"<span id="e"> </span><div id="t" aria-labelledby="nope e" aria-label="直接">x</div>"#,
            "div#t",
        );
        assert_eq!(result, named("直接", NameSource::AriaLabel, false));
        let result = name(
            r#"<input id="t" aria-labelledby="nope" title="題">"#,
            "input",
        );
        assert_eq!(result, named("題", NameSource::Title, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 参照先自身の `aria-label` が子孫テキストより優先される。
    #[test]
    fn aisnap_1_aria_labelledby_referent_aria_label_wins() {
        let result = name(
            r#"<div id="r" aria-label="ARIA側">子孫</div><p id="t" aria-labelledby="r">x</p>"#,
            "p#t",
        );
        assert_eq!(result, named("ARIA側", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 参照先の `aria-labelledby` は辿らない（連鎖しない）。
    #[test]
    fn aisnap_1_aria_labelledby_does_not_chain() {
        let result = name(
            r#"<span id="c">C</span><span id="b" aria-labelledby="c">B内容</span>
               <p id="a" aria-labelledby="b">x</p>"#,
            "p#a",
        );
        assert_eq!(result, named("B内容", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 自己参照は自分の子孫テキストを名前とする。
    #[test]
    fn aisnap_1_aria_labelledby_self_reference() {
        let result = name(
            r#"<button id="x" aria-labelledby="x">送信</button>"#,
            "button",
        );
        assert_eq!(result, named("送信", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 参照先が対象を含む場合、対象の部分木は除外される。
    #[test]
    fn aisnap_1_aria_labelledby_referent_containing_target() {
        let result = name(
            r#"<div id="l">名前 <input id="t" aria-labelledby="l" value="除外"> 続き</div>"#,
            "input",
        );
        assert_eq!(
            result,
            named("名前 続き", NameSource::AriaLabelledBy, false)
        );
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 参照先の `script`/`style` は除外、子孫 `img` の `alt` と参照先自身が `img` の `alt` は使う。
    #[test]
    fn aisnap_1_aria_labelledby_referent_content_rules() {
        let result = name(
            r#"<div id="r">本文<script>var x;</script><style>p{}</style><img alt="図"></div>
               <p id="t" aria-labelledby="r">x</p>"#,
            "p#t",
        );
        assert_eq!(result, named("本文図", NameSource::AriaLabelledBy, false));
        let result = name(
            r#"<img id="r" alt="画像名"><p id="t" aria-labelledby="r">x</p>"#,
            "p#t",
        );
        assert_eq!(result, named("画像名", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 重複 id は文書順で先頭、照合は大文字小文字を区別する。
    #[test]
    fn aisnap_1_aria_labelledby_duplicate_and_case_sensitive_ids() {
        let result = name(
            r#"<span id="a">先</span><span id="a">後</span><p id="t" aria-labelledby="a">x</p>"#,
            "p#t",
        );
        assert_eq!(result, named("先", NameSource::AriaLabelledBy, false));
        let result = name(
            r#"<span id="a">小</span><p id="t" aria-labelledby="A" title="題">x</p>"#,
            "p#t",
        );
        assert_eq!(result, named("題", NameSource::Title, false));
    }

    fn many_refs_html(referents: usize) -> String {
        let mut html = String::new();
        let mut ids = Vec::new();
        for i in 0..referents {
            html.push_str(&format!(r#"<span id="r{i}">r{i}</span>"#));
            ids.push(format!("r{i}"));
        }
        html.push_str(&format!(
            r#"<div id="t" aria-labelledby="{}">x</div>"#,
            ids.join(" ")
        ));
        html
    }

    /// AISNAP-1（TASK-11.4.1・#544）: ちょうど `MAX_IDREFS` 件なら全件を連結し `truncated` にならない。
    #[test]
    fn aisnap_1_aria_labelledby_at_limit_not_truncated() {
        let result = name(&many_refs_html(MAX_IDREFS), "div#t");
        let expected = (0..MAX_IDREFS)
            .map(|i| format!("r{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(result, named(&expected, NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: `MAX_IDREFS` を超えた分は切り捨て `truncated: true`。
    #[test]
    fn aisnap_1_aria_labelledby_over_limit_truncated() {
        let result = name(&many_refs_html(MAX_IDREFS + 1), "div#t");
        let expected = (0..MAX_IDREFS)
            .map(|i| format!("r{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(result, named(&expected, NameSource::AriaLabelledBy, true));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 空の参照先・存在しない id は寄与枠を消費しない。
    #[test]
    fn aisnap_1_aria_labelledby_empty_referents_do_not_consume_limit() {
        let mut html = String::from(r#"<span id="e0"></span><span id="e1"> </span>"#);
        let mut tokens = vec!["e0".to_string(), "missing".to_string(), "e1".to_string()];
        for i in 0..MAX_IDREFS {
            html.push_str(&format!(r#"<span id="r{i}">r{i}</span>"#));
            tokens.push(format!("r{i}"));
        }
        html.push_str(&format!(
            r#"<div id="t" aria-labelledby="{}">x</div>"#,
            tokens.join(" ")
        ));
        let result = name(&html, "div#t");
        let expected = (0..MAX_IDREFS)
            .map(|i| format!("r{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(result, named(&expected, NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 存在しないトークンが走査上限まで先行すると、空の名前 + `truncated: true` で確定し `aria-label` へ落ちない。
    #[test]
    fn aisnap_1_aria_labelledby_scan_cut_does_not_fall_back() {
        let missing = vec!["nope"; MAX_IDREFS_SCANNED].join(" ");
        let html = format!(
            r#"<span id="ok">実在</span><div id="t" aria-labelledby="{missing} ok" aria-label="直接">x</div>"#
        );
        let result = name(&html, "div#t");
        assert_eq!(result, named("", NameSource::None, true));
    }

    /// AISNAP-1（TASK-11.4.3・#546）: `aria-labelledby` の参照先が `select` で、選択 option の探索が走査上限で打ち切られたとき、最初の option を採用しつつ `truncated: true` を伝える（キャッシュ経由でも落とさない）。
    #[test]
    fn aisnap_1_aria_labelledby_select_referent_scan_cut_propagates_truncated() {
        let filler = "<option></option>".repeat(600);
        let html = format!(
            r#"<select id="s"><option>先頭</option>{filler}<option selected>選択</option></select><p id="t1" aria-labelledby="s">a</p><p id="t2" aria-labelledby="s">b</p>"#
        );
        assert_eq!(
            name(&html, "p#t1"),
            named("先頭", NameSource::AriaLabelledBy, true)
        );
        assert_eq!(
            name(&html, "p#t2"),
            named("先頭", NameSource::AriaLabelledBy, true)
        );
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 走査上限未満の欠落トークンが先行するだけなら後ろの実在参照から名前を得る。
    #[test]
    fn aisnap_1_aria_labelledby_few_missing_tokens_ok() {
        let missing = vec!["nope"; MAX_IDREFS_SCANNED - 1].join(" ");
        let html = format!(
            r#"<span id="ok">実在</span><div id="t" aria-labelledby="{missing} ok">x</div>"#
        );
        let result = name(&html, "div#t");
        assert_eq!(result, named("実在", NameSource::AriaLabelledBy, false));
    }

    /// AISNAP-1（TASK-11.4.1・#544）: `input[type=hidden]` は ARIA 属性があっても名前なし。
    #[test]
    fn aisnap_1_hidden_input_ignores_aria() {
        let result = name(
            r#"<span id="a">参照</span><input type="hidden" aria-label="x" aria-labelledby="a">"#,
            "input",
        );
        assert_eq!(result, AccessibleName::default());
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 参照先の巨大な `aria-label` は折り畳み後の上限で取り込みを止め、名前は `MAX_NAME_CHARS` で切り詰められ `truncated: true` になる。
    #[test]
    fn aisnap_1_aria_labelledby_huge_referent_aria_label_bounded() {
        let huge = "あ".repeat(100_000);
        let html =
            format!(r#"<div id="r" aria-label="{huge}"></div><p id="t" aria-labelledby="r">x</p>"#);
        let result = name(&html, "p#t");
        assert_eq!(
            result,
            named(
                &"あ".repeat(MAX_NAME_CHARS),
                NameSource::AriaLabelledBy,
                true
            )
        );
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 対象を含まない同一参照先を多数の対象が参照しても、共有索引で同じ結果を返す（参照先テキストはキャッシュされる）。
    #[test]
    fn aisnap_1_aria_labelledby_shared_referent_cached() {
        let mut html = String::from(r#"<div id="big">共有<b>見出し</b></div>"#);
        for _ in 0..2000 {
            html.push_str(r#"<p aria-labelledby="big">x</p>"#);
        }
        let parsed = parse_document(&html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let targets = fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), "p")
            .expect("セレクタは解釈できる");
        assert_eq!(targets.len(), 2000);
        let index = NameIndex::build(&doc);
        for id in targets {
            assert_eq!(
                compute_name_with_index(&doc, &index, id),
                named("共有見出し", NameSource::AriaLabelledBy, false)
            );
        }
        assert_eq!(index.referent_cache.borrow().len(), 1);
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 対象を含む参照先（キャッシュ不能）の走査は文書全体で `MAX_UNCACHED_REFERENT_SCANS` 回まで。超過後は空の名前 + `truncated: true`。
    #[test]
    fn aisnap_1_aria_labelledby_uncached_scan_budget() {
        let mut html = String::from(r#"<div id="big">見出し"#);
        for _ in 0..(MAX_UNCACHED_REFERENT_SCANS + 5) {
            html.push_str(r#"<input aria-labelledby="big">"#);
        }
        html.push_str("</div>");
        let parsed = parse_document(&html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let targets = fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), "input")
            .expect("セレクタは解釈できる");
        let index = NameIndex::build(&doc);
        let names: Vec<AccessibleName> = targets
            .iter()
            .map(|&id| compute_name_with_index(&doc, &index, id))
            .collect();
        assert_eq!(
            names.first(),
            Some(&named("見出し", NameSource::AriaLabelledBy, false))
        );
        assert_eq!(
            names.get(MAX_UNCACHED_REFERENT_SCANS - 1),
            Some(&named("見出し", NameSource::AriaLabelledBy, false))
        );
        assert_eq!(
            names.get(MAX_UNCACHED_REFERENT_SCANS),
            Some(&named("", NameSource::None, true))
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: `select` の参照先の option 探索も初回走査予算（`MAX_CACHEABLE_REFERENT_SCANS`）を消費する。超過後は空の名前 + `truncated: true`。
    #[test]
    fn aisnap_1_aria_labelledby_select_referent_consumes_first_scan_budget() {
        let n = MAX_CACHEABLE_REFERENT_SCANS + 5;
        let mut html = String::new();
        for i in 0..n {
            html.push_str(&format!(
                r#"<select id="s{i}"><option>値{i}</option></select><p id="t{i}" aria-labelledby="s{i}">x</p>"#
            ));
        }
        let parsed = parse_document(&html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let targets = fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), "p")
            .expect("セレクタは解釈できる");
        let index = NameIndex::build(&doc);
        let names: Vec<AccessibleName> = targets
            .iter()
            .map(|&id| compute_name_with_index(&doc, &index, id))
            .collect();
        assert_eq!(
            names.get(MAX_CACHEABLE_REFERENT_SCANS - 1),
            Some(&named(
                &format!("値{}", MAX_CACHEABLE_REFERENT_SCANS - 1),
                NameSource::AriaLabelledBy,
                false
            ))
        );
        assert_eq!(
            names.get(MAX_CACHEABLE_REFERENT_SCANS),
            Some(&named("", NameSource::None, true))
        );
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 互いに異なる id の参照先（キャッシュ可能）の初回走査も文書全体で `MAX_CACHEABLE_REFERENT_SCANS` 回まで。超過後は空の名前 + `truncated: true`、キャッシュ済みの参照先は引き続き解決できる。
    #[test]
    fn aisnap_1_aria_labelledby_first_scan_budget() {
        let n = MAX_CACHEABLE_REFERENT_SCANS + 5;
        let mut html = String::new();
        for i in 0..n {
            html.push_str(&format!(r#"<div id="r{i}"><p><b>見出し{i}</b></p></div>"#));
        }
        for i in 0..n {
            html.push_str(&format!(r#"<input aria-labelledby="r{i}">"#));
        }
        // 予算切れ後でも、既にキャッシュ済みの参照先は解決できる。
        html.push_str(r#"<input id="again" aria-labelledby="r0">"#);
        let parsed = parse_document(&html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let targets = fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), "input")
            .expect("セレクタは解釈できる");
        let index = NameIndex::build(&doc);
        let names: Vec<AccessibleName> = targets
            .iter()
            .map(|&id| compute_name_with_index(&doc, &index, id))
            .collect();
        assert_eq!(
            names.get(MAX_CACHEABLE_REFERENT_SCANS - 1),
            Some(&named(
                &format!("見出し{}", MAX_CACHEABLE_REFERENT_SCANS - 1),
                NameSource::AriaLabelledBy,
                false
            ))
        );
        assert_eq!(
            names.get(MAX_CACHEABLE_REFERENT_SCANS),
            Some(&named("", NameSource::None, true))
        );
        assert_eq!(
            names.get(n),
            Some(&named("見出し0", NameSource::AriaLabelledBy, false))
        );
        assert_eq!(
            index.referent_cache.borrow().len(),
            MAX_CACHEABLE_REFERENT_SCANS
        );
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 参照先の外側の要素が先に作ったキャッシュを、参照先の内側の要素が再利用しない（共有索引でも `compute_name` と一致する）。
    #[test]
    fn aisnap_1_aria_labelledby_cache_not_reused_for_inner_target() {
        let html = r#"<span id="o" aria-labelledby="r"></span><div id="r">名前<button id="b" aria-labelledby="r">送信</button></div>"#;
        let parsed = parse_document(html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let find = |sel: &str| {
            fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), sel)
                .expect("セレクタは解釈できる")
                .first()
                .copied()
                .expect("要素が存在する")
        };
        let (outer, button) = (find("span#o"), find("button#b"));
        let index = NameIndex::build(&doc);
        let _ = compute_name_with_index(&doc, &index, outer);
        assert_eq!(
            compute_name_with_index(&doc, &index, button),
            compute_name(&doc, button)
        );
        assert_eq!(
            compute_name_with_index(&doc, &index, button),
            named("名前", NameSource::AriaLabelledBy, false)
        );
    }

    /// AISNAP-1（TASK-11.4.1・#544）: 対象を含む参照先でも、参照先自身の `aria-label` を読むだけなら走査予算を消費しない。
    #[test]
    fn aisnap_1_aria_labelledby_own_label_does_not_consume_budget() {
        let mut html = String::from(r#"<div id="big" aria-label="共有">"#);
        for _ in 0..(MAX_UNCACHED_REFERENT_SCANS + 5) {
            html.push_str(r#"<input aria-labelledby="big">"#);
        }
        html.push_str("</div>");
        let parsed = parse_document(&html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let targets = fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), "input")
            .expect("セレクタは解釈できる");
        let index = NameIndex::build(&doc);
        for id in targets {
            assert_eq!(
                compute_name_with_index(&doc, &index, id),
                named("共有", NameSource::AriaLabelledBy, false)
            );
        }
    }

    // --- TASK-11.4.3（#546）: 子孫テキスト・文書ルート・優先順位の統合 ---

    use super::{MAX_CONTENT_STEPS, NORMALIZED_LABEL_TEXT_CHAR_LIMIT};

    /// AISNAP-1（TASK-11.4.3・#546）: 子孫テキストだけで決まる要素。
    #[test]
    fn aisnap_1_content_button_link_heading_role() {
        assert_eq!(
            name("<button>送信</button>", "button"),
            named("送信", NameSource::Content, false)
        );
        assert_eq!(
            name(r##"<a href="#">詳細</a>"##, "a"),
            named("詳細", NameSource::Content, false)
        );
        assert_eq!(
            name("<h1>  見出し\n テキスト </h1>", "h1"),
            named("見出し テキスト", NameSource::Content, false)
        );
        assert_eq!(
            name(r#"<div role="button">押す</div>"#, "div"),
            named("押す", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: 表のセルは content role。href の無い
    /// `a`（generic）は content を使わず title のみ。
    #[test]
    fn aisnap_1_content_table_cells_and_non_content_elements() {
        let html = "<table><tr><th>見出しセル</th><td>データ</td></tr></table>";
        assert_eq!(
            name(html, "th"),
            named("見出しセル", NameSource::Content, false)
        );
        assert_eq!(
            name(html, "td"),
            named("データ", NameSource::Content, false)
        );
        assert_eq!(
            name(r#"<a title="T">本文</a>"#, "a"),
            named("T", NameSource::Title, false)
        );
        assert_eq!(
            name(r#"<div title="補足">text</div>"#, "div"),
            named("補足", NameSource::Title, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: 入れ子の要素・画像 alt の連結。
    #[test]
    fn aisnap_1_content_nested_and_img_alt() {
        assert_eq!(
            name(r##"<a href="#"><img alt="ロゴ"> ホーム</a>"##, "a"),
            named("ロゴ ホーム", NameSource::Content, false)
        );
        assert_eq!(
            name("<button><span>保</span><b>存</b></button>", "button"),
            named("保存", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: accname 2A。hidden / aria-hidden の
    /// 子孫は除外する（aria-hidden は trim・大文字小文字無視）。script/style も除外。
    #[test]
    fn aisnap_1_content_excludes_hidden_and_script() {
        assert_eq!(
            name(
                r#"<button>送信<span hidden>隠し</span><span aria-hidden=" TRUE ">x</span><script>s()</script><style>b{}</style></button>"#,
                "button"
            ),
            named("送信", NameSource::Content, false)
        );
        assert_eq!(
            name(
                r#"<button>送信<span aria-hidden="false">見える</span></button>"#,
                "button"
            ),
            named("送信見える", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: accname 2C の再帰適用。子孫の
    /// `aria-label` を採用し、その部分木へは降りない。
    #[test]
    fn aisnap_1_content_descendant_aria_label() {
        assert_eq!(
            name(
                r##"<a href="#"><span aria-label="ホーム">🏠<b>無視</b></span></a>"##,
                "a"
            ),
            named("ホーム", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: 再帰中の埋め込みコントロールでは
    /// `aria-label` より値（accname 2E）を優先する。
    #[test]
    fn aisnap_1_content_embedded_control_value_beats_aria_label() {
        assert_eq!(
            name(
                r#"<button>数量 <input aria-label="数量" value="5"></button>"#,
                "button"
            ),
            named("数量 5", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: accname 2E。埋め込みコントロールの値。
    /// password の value は取り込まない（平文の漏えい防止）。
    #[test]
    fn aisnap_1_content_embedded_controls() {
        assert_eq!(
            name(
                "<button>数量 <select><option>1</option><option selected>2</option></select></button>",
                "button"
            ),
            named("数量 2", NameSource::Content, false)
        );
        assert_eq!(
            name(
                "<button>数量 <select><option>1</option><option>2</option></select></button>",
                "button"
            ),
            named("数量 1", NameSource::Content, false)
        );
        assert_eq!(
            name(r#"<button>名前 <input value="太郎"></button>"#, "button"),
            named("名前 太郎", NameSource::Content, false)
        );
        assert_eq!(
            name(
                r#"<button>鍵 <input type="password" value="secret123"></button>"#,
                "button"
            ),
            named("鍵", NameSource::Content, false)
        );
        assert_eq!(
            name(
                r#"<input id="v" value="参照値"><button aria-labelledby="v">x</button>"#,
                "button"
            ),
            named("参照値", NameSource::AriaLabelledBy, false)
        );
        // 参照先でも値を aria-label より優先する（子孫走査と一致）。
        assert_eq!(
            name(
                r#"<input id="v" value="5" aria-label="数量"><button aria-labelledby="v">x</button>"#,
                "button"
            ),
            named("5", NameSource::AriaLabelledBy, false)
        );
        // 値が空なら参照先の aria-label へ譲る。
        assert_eq!(
            name(
                r#"<input id="v" aria-label="数量"><button aria-labelledby="v">x</button>"#,
                "button"
            ),
            named("数量", NameSource::AriaLabelledBy, false)
        );
        assert_eq!(
            name(
                r#"<input id="v" type="password" value="secret123"><button aria-labelledby="v" title="T">x</button>"#,
                "button"
            ),
            named("x", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: 文字数上限の境界。ちょうど上限は
    /// truncated でなく、1 文字超過で 120 文字・truncated。上限の境界が
    /// 要素の境目に来ても正しい。
    #[test]
    fn aisnap_1_content_char_limit_boundary() {
        let exact = "あ".repeat(MAX_NAME_CHARS);
        let r = name(&format!("<button>{exact}</button>"), "button");
        assert_eq!(r, named(&exact, NameSource::Content, false));

        let over = "あ".repeat(MAX_NAME_CHARS + 1);
        let r = name(&format!("<button>{over}</button>"), "button");
        assert_eq!(r, named(&exact, NameSource::Content, true));

        let half = "い".repeat(MAX_NAME_CHARS / 2);
        let r = name(
            &format!("<button><span>{half}</span><span>{half}</span><span>末</span></button>"),
            "button",
        );
        assert_eq!(
            r,
            named(&"い".repeat(MAX_NAME_CHARS), NameSource::Content, true)
        );
        let r = name(
            &format!("<button><span>{half}</span><span>{half}</span></button>"),
            "button",
        );
        assert_eq!(r.text.chars().count(), MAX_NAME_CHARS);
        assert!(!r.truncated);
    }

    /// AISNAP-1（TASK-11.4.3・#546）: ステップ上限。テキストに届く前に
    /// 打ち切ったら title へフォールバックせず truncated の空の名前。
    #[test]
    fn aisnap_1_content_step_limit_does_not_fall_back_to_title() {
        let spans = "<span></span>".repeat(MAX_CONTENT_STEPS + 10);
        let r = name(
            &format!(r#"<button title="T">{spans}後ろ</button>"#),
            "button",
        );
        assert_eq!(r, named("", NameSource::None, true));

        let r = name(
            &format!(r#"<button title="T">先{spans}後ろ</button>"#),
            "button",
        );
        assert_eq!(r, named("先", NameSource::Content, true));
    }

    /// AISNAP-1（TASK-11.4.3・#546・PR #574 レビュー指摘）: 未選択の `select` で
    /// 後続の `option` が大量にあっても、最初の `option` のテキストは予算を
    /// 残して収集され、名前が空にならない（truncated は立つ）。
    #[test]
    fn aisnap_1_content_select_default_option_survives_many_options() {
        let options = "<option>x</option>".repeat(MAX_CONTENT_STEPS + 10);
        let html = format!("<button>数量 <select><option>初期</option>{options}</select></button>");
        assert_eq!(
            name(&html, "button"),
            named("数量 初期", NameSource::Content, true)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: 文書全体の予算。content role の深い
    /// 入れ子を共有索引で全要素算出しても有限時間で終わり、予算超過分は
    /// truncated になる。
    #[test]
    fn aisnap_1_content_document_budget_bounds_total_work() {
        const DEPTH: usize = 3_000;
        let mut html = String::new();
        for _ in 0..DEPTH {
            html.push_str(r#"<div role="button">"#);
        }
        html.push('x');
        for _ in 0..DEPTH {
            html.push_str("</div>");
        }
        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let parsed = parse_document(&html, &options).expect("成功する");
        let doc = parsed.document;
        let targets = fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), "div")
            .expect("セレクタは解釈できる");
        let index = NameIndex::build(&doc);
        let start = std::time::Instant::now();
        let results: Vec<AccessibleName> = targets
            .iter()
            .map(|&id| compute_name_with_index(&doc, &index, id))
            .collect();
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        // 最も外側はステップ上限で本文に届かず、truncated の空の名前。
        let first = results.first().expect("要素がある");
        assert_eq!(first, &named("", NameSource::None, true));
        // 最も内側は本文に届く（先に算出した要素に予算を奪われない）。
        let last = results.last().expect("要素がある");
        assert_eq!(last, &named("x", NameSource::Content, false));
    }

    /// AISNAP-1（TASK-11.4.3・#546・PR #574 レビュー指摘）: 共有索引での
    /// 名前は算出順に依存せず、単発の `compute_name` と一致する。
    #[test]
    fn aisnap_1_content_result_is_independent_of_call_order() {
        let mut html = String::new();
        for _ in 0..600 {
            html.push_str(r#"<div role="button">"#);
        }
        html.push_str("短い");
        for _ in 0..600 {
            html.push_str("</div>");
        }
        html.push_str(r#"<button title="T">押す</button>"#);
        let parsed = parse_document(&html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let ids =
            fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), "div, button")
                .expect("セレクタは解釈できる");
        let expected: Vec<AccessibleName> = ids.iter().map(|&id| compute_name(&doc, id)).collect();
        let index = NameIndex::build(&doc);
        let mut reversed: Vec<AccessibleName> = ids
            .iter()
            .rev()
            .map(|&id| compute_name_with_index(&doc, &index, id))
            .collect();
        reversed.reverse();
        assert_eq!(reversed, expected);
        assert_eq!(
            expected.last().expect("要素がある"),
            &named("押す", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: label 経由の子孫走査が文字数上限で
    /// 打ち切られたら truncated が伝わる。
    #[test]
    fn aisnap_1_label_cut_propagates_truncated() {
        let long = "あ".repeat(NORMALIZED_LABEL_TEXT_CHAR_LIMIT + 50);
        let r = name(
            &format!(r#"<label for="x">{long}</label><input id="x">"#),
            "input",
        );
        assert_eq!(r.text.chars().count(), MAX_NAME_CHARS);
        assert_eq!(r.source, NameSource::Label);
        assert!(r.truncated);
    }

    /// AISNAP-1（TASK-11.4.3・#546）: accname 2A。参照先自身が hidden なら
    /// その hidden な子孫も含める。参照先が hidden でなければ hidden な子孫は除外。
    #[test]
    fn aisnap_1_labelledby_hidden_referent() {
        assert_eq!(
            name(
                r#"<span id="h" hidden>隠し<span hidden>内側</span></span><button aria-labelledby="h">x</button>"#,
                "button"
            ),
            named("隠し内側", NameSource::AriaLabelledBy, false)
        );
        assert_eq!(
            name(
                r#"<span id="h">見え<span hidden>内側</span></span><button aria-labelledby="h">x</button>"#,
                "button"
            ),
            named("見え", NameSource::AriaLabelledBy, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: ARIA → label → 子孫テキスト → title の
    /// 優先順位の統合。
    #[test]
    fn aisnap_1_precedence_aria_native_content_title() {
        assert_eq!(
            name(
                r#"<span id="l">参照</span><button aria-labelledby="l" aria-label="A" title="T">本文</button>"#,
                "button"
            ),
            named("参照", NameSource::AriaLabelledBy, false)
        );
        assert_eq!(
            name(
                r#"<button aria-label="A" title="T">本文</button>"#,
                "button"
            ),
            named("A", NameSource::AriaLabel, false)
        );
        assert_eq!(
            name(
                r#"<label for="b">L</label><button id="b" title="T">本文</button>"#,
                "button"
            ),
            named("L", NameSource::Label, false)
        );
        assert_eq!(
            name(r#"<button title="T">本文</button>"#, "button"),
            named("本文", NameSource::Content, false)
        );
        assert_eq!(
            name(r#"<button title="T"></button>"#, "button"),
            named("T", NameSource::Title, false)
        );
        assert_eq!(
            name(r#"<h1 title="T">見出し</h1>"#, "h1"),
            named("見出し", NameSource::Content, false)
        );
    }

    /// AISNAP-1（TASK-11.4.3・#546）: 文書ルートは最初の HTML `<title>` で
    /// 命名する。SVG の `<title>` は対象外。
    #[test]
    fn aisnap_1_document_root_title() {
        let parse = |html: &str| {
            parse_document(html, &ParseOptions::default())
                .expect("成功する")
                .document
        };
        let doc = parse("<html><head><title>  Example\n Domain </title></head></html>");
        assert_eq!(
            compute_name(&doc, doc.root()),
            named("Example Domain", NameSource::DocumentTitle, false)
        );
        let doc = parse("<html><head></head><body>x</body></html>");
        assert_eq!(compute_name(&doc, doc.root()), AccessibleName::default());
        let doc = parse("<body><svg><title>図</title></svg></body>");
        assert_eq!(compute_name(&doc, doc.root()), AccessibleName::default());
        let doc = parse("<head><title>先</title></head><body><svg><title>図</title></svg></body>");
        assert_eq!(
            compute_name(&doc, doc.root()),
            named("先", NameSource::DocumentTitle, false)
        );
        // body 内の HTML `<title>` は文書タイトルにしない（head 直下に限る）。
        let doc = parse("<html><head></head><body><p><title>本文</title></p></body></html>");
        assert_eq!(compute_name(&doc, doc.root()), AccessibleName::default());
        let doc = parse(
            "<html><head><title>先頭</title></head><body><div><title>後</title></div></body></html>",
        );
        assert_eq!(
            compute_name(&doc, doc.root()),
            named("先頭", NameSource::DocumentTitle, false)
        );
        let long = "題".repeat(MAX_NAME_CHARS + 5);
        let doc = parse(&format!("<head><title>{long}</title></head>"));
        let r = compute_name(&doc, doc.root());
        assert_eq!(r.text.chars().count(), MAX_NAME_CHARS);
        assert!(r.truncated);
    }

    /// AISNAP-1（TASK-11.4.3・#546）: 共有索引版と単発版の結果が一致する。
    #[test]
    fn aisnap_1_content_index_matches_single_shot() {
        let html = r##"<html><head><title>T</title></head><body>
            <button>送信</button><a href="#">詳細</a><h1 title="x">見出し</h1>
            <table><tr><th>H</th><td>D</td></tr></table>
            <button>数量 <select><option selected>2</option></select></button>
        </body></html>"##;
        let parsed = parse_document(html, &ParseOptions::default()).expect("成功する");
        let doc = parsed.document;
        let index = NameIndex::build(&doc);
        let mut all = Vec::new();
        for tag in [
            "html", "head", "title", "body", "button", "a", "h1", "th", "td", "select",
        ] {
            all.extend(
                fandhe_browser_core::query::query_selector_all_str(&doc, doc.root(), tag)
                    .expect("セレクタは解釈できる"),
            );
        }
        assert!(!all.is_empty());
        for id in all.into_iter().chain([doc.root()]) {
            assert_eq!(
                compute_name_with_index(&doc, &index, id),
                compute_name(&doc, id)
            );
        }
    }
}
