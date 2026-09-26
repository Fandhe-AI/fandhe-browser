# コントリビューションガイドライン: 未実装・簡易実装箇所のコメント運用

対応 `TASK-3`（3.1）/ `MS-7` / ビヘイビア `REPAIR-3`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的

未実装・簡易実装の箇所は「実装済みを装わない」ことが REPAIR-3 の要請である。コメントを
「済ませたふりの実装」の隠れ蓑にせず、AI（Claude を含む）が後日そのまま拾って実装に着手できる
「仕様書」として書く（PoC-10 設計ガイドライン 4）。本書は、その将来仕様コメントに何を・どう書くかを定める。

## 適用範囲

以下のいずれかに該当する箇所が対象。

- **未実装**: 型・関数・モジュールがまだ存在しない、または常にエラーを返すだけのスタブ
- **簡易実装**: 仕様の一部だけを実装済み、既定値固定、参考用の近似処理
- **Issue スコープ外として意図的に見送った機能**: 受け入れ基準外・後続 Issue へ切り出した挙動

## `code-comment-style.md` との役割分担

| 文書 | 扱う範囲 |
| ---- | -------- |
| [code-comment-style.md](../../.claude/rules/code-comment-style.md) | 全コメントに共通する一般規約（役割要約・呼び出し元/呼び出し先の文脈・他 crate との契約・`// SAFETY:`・日本語での記述） |
| 本ガイドライン | REPAIR-3 固有の「将来仕様の明記」に特化した必須項目・書式・配置・禁止事項 |

両者が重なる場合（例: スタブの役割要約）は一般規約を code-comment-style.md 側の書き方に従い、
将来仕様の詳細（目指す挙動・ビヘイビア ID・TASK-n・現在の制限）は本書の書式に従う。

## 必須記載項目

将来仕様コメントには、該当する範囲で以下を必ず含める。

1. **現状の明示**: スタブ／簡易実装／スコープ外のいずれであるか
2. **目指す挙動（将来仕様）の要約**: spec 本文の丸ごと引用ではなく、必要な範囲の要約に留める
3. **対応するビヘイビア ID**（`<PREFIX>-<N>`。spec-reference.md の表記に従いバッククォート付きで書く）
4. **実装予定の TASK-n**（分かれば Issue 番号も併記）
5. **簡易実装の場合は現在の制限**: 何が未対応か、どんな入力・条件で挙動が変わるか
6. **呼び出し元に返す挙動**: エラー variant や既定値など、呼び出し側が依存してよい契約

## 書式と配置

- **crate・モジュール単位の未実装**は crate 直下の `//!` に `# スタブについて` の見出しを立て、
  未実装項目を「項目（ビヘイビア ID、TASK-n）」の箇条書きで並べる
- **Issue スコープ外として意図的に見送った箇所**は `## 本 Issue（#N）の範囲外（将来仕様。REPAIR-3: 実装済みを装わない）`
  の見出しを立て、同様に箇条書きで示す
- **関数・型単位**の未実装・簡易実装は `///` に「スタブ」「簡易実装」と明記し、置換先の TASK-n を書く
- 見出し語（「スタブについて」「将来仕様」「REPAIR-3」）は固定して使う。既存コードへの適用漏れを
  横断的に洗い出す作業（TASK-3.2）で `grep` できるようにするため
- ID 表記は [spec-reference.md](../../.claude/rules/spec-reference.md) に従う（バッククォート付き `CORE-1`、`TASK-n`、`MS-n`）

## 具体例

### 例 1: crate 雛形のスタブ節（`//!` での箇条書き）

```rust
//! # スタブについて
//!
//! 本ファイルは crate の雛形（TASK-1（1.7）・MS-1・ビヘイビア `REPAIR-1`）であり、
//! 以下はいずれも未実装。実装済みを装う公開 API・ダミー実装は置かない
//! （`code-comment-style.md`・REPAIR-3）。
//!
//! - プロファイル削除処理（`PROF-5`、TASK-53）
//! - クロスプラットフォーム advisory lock（`PROF-1`、TASK-54）
```

### 例 2: 常にエラーを返すスタブ関数（`#[non_exhaustive]` の戻り値型）

戻り値は真偽値やフラットな `String` にせず、将来フィールドを追加できる構造にする（REPAIR-4）。

```rust
/// JS 実行結果を表す型。
///
/// 本スタブでは値を生成しないが、将来拡張できる構造にしてある（REPAIR-4）。
/// TASK-30（MS-3・#143）で実際の実行結果を保持するフィールドが追加される想定。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsExecutionOutput {
    pub value: String,
}

/// JS 実行のスタブ境界（CORE-1・TASK-24（24.9）・MS-1・Issue #43）。
///
/// 常にエラーを返す設計（`js-engine.md` JS-2 が明示的に名指しする関数）。
/// `fandhe-browser-js`（TASK-28・MS-3）の統合・TASK-30（MS-3・Issue #143）での
/// V8 実呼び出しへの置換まで、呼び出し元には「未実装」を明示するエラーを
/// 返し、成功を装わない。
pub fn execute_js_stub(script: &str) -> crate::Result<JsExecutionOutput> {
    let _ = script;
    Err(crate::Error::JsExecutionUnavailable {
        message: "JS execution is not implemented yet \
                   (js_stub boundary; replaced by fandhe-browser-js per JS-2/TASK-30)"
            .to_string(),
    })
}
```

### 例 3: 簡易実装の制限の明記（現在の制限を明示する）

```rust
/// レスポンス本文を UTF-8 として非可逆変換した文字列（不正なバイト列は
/// 置換文字に置き換える）。厳密な文字コード判定（`charset` ヘッダ・
/// meta タグ由来。ビヘイビア `CORE-5` (7)・TASK-25・MS-3）は範囲外の将来仕様であり、
/// これは参考用の簡易変換に留まる。
pub fn body_text_lossy(&self) -> String {
    String::from_utf8_lossy(&self.body).into_owned()
}
```

crate・モジュール単位でスコープ外の項目をまとめる場合は次のように書く。

```rust
//! ## 本 Issue（#36）の範囲外（将来仕様。REPAIR-3: 実装済みを装わない）
//!
//! - Cookie セッション維持（ビヘイビア `CORE-5` (8)・TASK-25・MS-3）
//! - 文字コード判定（ビヘイビア `CORE-5` (7)・TASK-25・MS-3）: 本文は常にバイト列で返す
```

### 例 4: 悪い例と良い例の対比

NG（成功を装うダミー実装。ID・将来仕様のない `TODO`）:

```rust
// TODO: あとで直す
pub fn evaluate(script: &str) -> bool {
    let _ = script;
    true // 常に成功として返してしまっている
}
```

OK（書き直し。エラーで未実装を明示し、ID・TASK-n・目指す挙動を書く）:

```rust
/// スクリプト評価のスタブ（`JS-2`・TASK-30・MS-3・Issue #143）。
///
/// 現状は常にエラーを返す。将来は `fandhe-browser-js` 経由で V8 に評価させ、
/// 評価結果を [`JsExecutionOutput`] として返す設計に置き換わる。
pub fn evaluate(script: &str) -> crate::Result<JsExecutionOutput> {
    let _ = script;
    Err(crate::Error::JsExecutionUnavailable {
        message: "not implemented yet".to_string(),
    })
}
```

## 禁止事項

- **成功を一律に返すダミー実装・フォールバック**: 未実装の CDP メソッド等で「成功を返す」実装は
  検出回避（anti-bot 回避）として作用しうるため置かない（[security.md](../../.claude/rules/security.md)）
- **ID・将来仕様を伴わない `TODO` / `FIXME` だけのコメント**: 後日の実装判断材料にならず、
  REPAIR-3 の要求（将来仕様の明記）を満たさない
- **`todo!()` / `unimplemented!()` を外部入力経路やライブラリの公開 API に置くこと**: panic させない
  原則に反する（[coding-rust.md](../../.claude/rules/coding-rust.md)「エラーハンドリング」節）。
  戻り値は `Result` のエラー variant で未実装を表す
- **spec 本文の長い引用**: ID を示して spec を参照させ、丸ごとコピーはしない
  ([spec-reference.md](../../.claude/rules/spec-reference.md))

## 実装時・レビュー時の運用

- 将来仕様コメントに書いた挙動を実装した PR では、該当コメント（`# スタブについて`・`## 本 Issue（#N）の範囲外` の
  箇条書き等）を削除または更新し、陳腐化させない
- レビューでは `AGENTS.md` の「スタブの明示」（P0 観点）で確認する
- 既存コードへの本ルールの適用（適用漏れの洗い出し・修正）は TASK-3.2（Issue #327）で行う。本書はルールの
  文書化のみを扱う

## 参照

- [code-comment-style.md](../../.claude/rules/code-comment-style.md)
- [coding-rust.md](../../.claude/rules/coding-rust.md)
- [security.md](../../.claude/rules/security.md)
- [spec-reference.md](../../.claude/rules/spec-reference.md)
- `AGENTS.md`（レビュー観点「スタブの明示」）
- spec: `04-behavior/self-repair-design.md`（`REPAIR-3`）・`05-tasks.md`（`TASK-3`）
