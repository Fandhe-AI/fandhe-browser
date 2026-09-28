//! JS エンジン種別の列挙と、同梱エンジン一覧を返す入口（MS-3・TASK-28（28.2）・
//! ビヘイビア `JS-1`・Issue #148）。
//!
//! `docs/spec/04-behavior/js-engine.md`「JS エンジンの切替方式」決定 5 は、
//! js crate が feature の有無に関係なく常に定義するエンジン種別の列挙型
//! （V8・Boa）と、同梱エンジンの一覧を返す関数を公開する契約を定める。
//! 本モジュールはその契約の入口のみを提供する。
//!
//! 呼び出し元（将来）: `fandhe-browser-core` は TASK-30（Issue #143・
//! `JS-2`）で `js_stub::execute_js_stub` を実エンジン呼び出しに置換する際、
//! また TASK-91（設定ファイルの `[js] engine` 解析）は「同梱されている
//! エンジンは何か」を問い合わせる際に、それぞれ [`bundled_engines`] を
//! 経由する想定。
//!
//! 本モジュールは TASK-28.3（Issue #149）でさらに、エンジン抽象トレイト
//! 本体（[`JsEngine`]）・[`EngineKind`] からトレイトオブジェクトを生成する
//! 関数（[`create_engine`]。未同梱の種別への `Err` 返却を含む）を追加した。
//!
//! 両エンジン共通のコンフォーマンステスト（TASK-28.4・Issue #150）は
//! 本モジュールではなく、crate の `tests/conformance.rs`（結合テスト）に
//! ある。
//!
//! 簡易実装: [`EngineKind`] の文字列表現（設定ファイルの `"v8"`/`"boa"` との
//! 相互変換・未同梱時のエラーメッセージ整形）は `TASK-91`（91.2・`MS-3`・
//! Issue #215）で確定するため、本 Issue では先取りしない。

/// 本 crate が抽象化対象とする JS エンジンの種別。
///
/// `docs/spec/04-behavior/js-engine.md` 決定 5 は V8・Boa の 2 種に固定した
/// 設計であるため、`fandhe-browser-core::Error`（他モジュールが今後バリアント
/// を追加する前提）とは異なり、`#[non_exhaustive]` は付けない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// V8（`rusty_v8`）エンジン。既定エンジン（README「実装方針（要点）」）。
    V8,
    /// boa（`boa_engine`）エンジン。切替先エンジン。
    Boa,
}

/// この crate に「同梱」（`js-engine.md`「JS エンジンの切替方式」の用語。
/// エンジンを Cargo feature でバイナリへ組み込むことを指し、実行時に使う
/// エンジンを選ぶ「選択」とは区別される）されている、すなわち対応する
/// feature が有効化されているエンジンの固定配列（[`bundled_engines`] の
/// 実体）。
const BUNDLED: &[EngineKind] = &[
    #[cfg(feature = "js-v8")]
    EngineKind::V8,
    #[cfg(feature = "js-boa")]
    EngineKind::Boa,
];

/// 「同梱」（`js-v8`/`js-boa` feature が有効化）されているエンジンの一覧を、
/// V8 → Boa の固定順で返す（**契約**: 本関数は `js-engine.md` の「同梱」＝
/// feature 有効化の一覧であり、「そのエンジンの実装を今すぐ呼び出せるか」
/// を保証しない。両者が一致するのは、V8 の実装が入る TASK-29・`boa` の
/// 実装が入る TASK-32 の完了後）。
///
/// 順序は `js-engine.md`「(1) 省略」の「同梱エンジンから V8 → boa の優先順で
/// 選ぶ」という後続契約（TASK-91 が依存）に対応する。
///
/// `js-v8` は TASK-29.1（#152）で `dep:v8` と結合済みだが、`js-boa` は
/// 依然として `dep:` 接頭辞なしのプレースホルダ feature（TASK-28.1・#147。
/// `boa_engine` の追加は TASK-32・#165）である。どちらも feature を
/// 有効化するだけでは実際の V8/boa 実装が使えるわけではない
/// （実装済みを装わない。REPAIR-3）。[`create_engine`] はこの一覧を
/// 「同梱判定」の唯一の情報源として使うが、同梱されている（＝ `feature` が
/// 有効）ことは「具象実装が使える」ことを意味しない。具象実装が入るのは
/// TASK-29（V8）／TASK-32（`boa`）の完了後であり、それまでは
/// [`create_engine`] は同梱されている種別に対しても
/// `CreateEngineError::NotYetImplemented` を返す。呼び出し元はこの一覧の
/// 要素を、それまでの間「即座に生成・実行できるエンジン」として扱っては
/// ならない。feature が無効なバリアントはコンパイル自体から除外されるため
/// （`#[cfg(...)]` 付きの配列要素）、「同梱されていないのに一覧に載る」
/// 幽霊エントリが実行時分岐の書き間違いで混入する余地はない。
pub fn bundled_engines() -> &'static [EngineKind] {
    BUNDLED
}

/// [`JsEngine`] のメソッド間でやり取りする、エンジン非依存の値表現
/// （TASK-28.3・`JS-1`）。
///
/// `v8::Local<Value>` や `boa_engine::JsValue` のようなエンジン固有の値型を
/// 上位 crate（core・cdp・ai 等）へ漏らさないための共通通貨
/// （coding-rust.md「JS エンジンはトレイト抽象越しに使い、V8 / boa の具象型
/// を上位 crate へ漏らさない」）。`docs/spec/03-poc/js-engine-comparison` の
/// PoC-3 で実測した「文字列 in/out・数値 out」の形状をカバーする最小構成
/// であり、簡易実装（現在の制限: オブジェクト・配列等の複合値は表現できない。
/// 必要になった時点（`TASK-29`/`TASK-32`・`MS-3`）で variant を追加する
/// （過剰設計を避ける。REPAIR-3）。将来の variant 追加が
/// 破壊的変更にならないよう `#[non_exhaustive]` を付ける（`JsEngineError`・
/// `CreateEngineError` と同じ理由づけ。`EngineKind` が spec で V8・Boa の
/// 2 種に固定されているのとは事情が異なる）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum JsValue {
    /// JS の `undefined` に対応する。
    Undefined,
    /// JS の `null` に対応する。
    Null,
    /// 真偽値。
    Bool(bool),
    /// 数値（JS の Number は倍精度浮動小数点のため `f64` で表現する）。
    Number(f64),
    /// 文字列。
    String(String),
}

/// [`NativeFn`] 呼び出し 1 回に紐づく実行コンテキスト（`TASK-29`・
/// Issue #526・codex レビュー指摘「`NativeFn` が戻らない場合に評価期限が
/// 機能しない」対応）。
///
/// 親（`super::process_engine::V8ProcessEngine`）は `NativeFn` を呼び出し
/// スレッド上で同期実行する（`NativeFn` が `Send` ではなく、V8 の
/// `Isolate`/`HandleScope` がスレッド固有であるため別スレッドへ移せない。
/// 下記 [`NativeFn`] のドキュメントコメント参照）。そのため親は実行中の
/// `NativeFn` を外部から中断する手段を持たず、期限切れを確認できるのは
/// `NativeFn` から戻った直後だけである（`super::process_engine`
/// モジュールドキュメントの「親側 `NativeCall` dispatch」節参照）。
///
/// 本構造体は、呼び出し元が計算済みの評価期限を `NativeFn` 自身へ伝え、
/// `NativeFn` が自発的に処理を打ち切れるようにするための協調的な手段
/// である（強制はできない。`NativeFn` がこれを無視して戻らない場合の
/// 挙動は変わらない）。`Instant` を直接引数にしないのは、将来
/// フィールドを追加できる構造にするため（coding-rust.md「戻り値は
/// 将来拡張できる構造を持つ型にする」と同じ考え方を、引数型にも
/// 適用する）。
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct NativeCallContext {
    /// この呼び出しが属する評価全体の期限。呼び出し元
    /// （`super::process_engine::send_evaluate_and_await`）が評価開始時に
    /// 1 度だけ計算した値であり、`NativeFn` の実行時間もこの期限に含まれる。
    /// `NativeFn` はこの時刻を超えないよう自身の処理を打ち切ることが
    /// 望ましい（強制ではない。上記構造体ドキュメント参照）。
    pub deadline: std::time::Instant,
}

/// [`JsEngine::inject_global_function`]・[`JsEngine::bind_dom_like_object`]
/// が受け取る Rust ネイティブ関数の型（TASK-28.3・`JS-1`）。
///
/// 引数・戻り値を [`JsValue`] に統一することで、V8/`boa` どちらの具象実装
/// からも同じシグネチャで呼び出せる（PoC-3 の `print`/`dom.setText`/
/// `dom.getText`/`dom.count` 相当）。呼び出し元（将来: `fandhe-browser-core`
/// の TASK-30）は、この関数へ渡す引数が外部入力（プラグイン入出力・ネット
/// ワーク取得データ由来）である場合、untrusted な入力として検証する責務を
/// 負う（security.md「プラグイン境界」）。第 2 引数の [`NativeCallContext`]
/// は呼び出し元が計算した評価期限を伝える（`TASK-29`・Issue #526）。
///
/// `Send`/`Sync` 境界は付けない。V8 の `Isolate`/`HandleScope` はスレッド
/// 固有であり、境界を付けると V8 実装（TASK-29）が満たせなくなるため
/// （`js-engine.md`「選択はプロセス起動時に1回」が定める、エンジンの実行が
/// プロセス内で単一スレッドに閉じる前提に対応する）。
pub type NativeFn =
    Box<dyn FnMut(&[JsValue], &NativeCallContext) -> Result<JsValue, JsEngineError>>;

/// [`JsEngine::evaluate_script`] の実行制御オプション（TASK-28.3・`JS-1`）。
///
/// 簡易実装（現在の制限: 現時点ではフィールドを持たず、タイムアウト等の
/// 無限ループ対策〔OWASP「不安全な設計」・A04〕を指定する手段がない。
/// 機能があるように見せない。REPAIR-3）。タイムアウトは
/// `TASK-29`／`TASK-30`（`MS-3`）で本構造体にフィールドを追加して実装する
/// 差し込み口として用意する。`#[non_exhaustive]` を付け、フィールド追加が
/// 破壊的変更にならないようにする。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct EvaluateOptions {}

/// [`JsEngine`] の各操作が失敗した際のエラー（TASK-28.3・`JS-1`）。
///
/// V8/`boa` それぞれの実装（TASK-29/32）が固有の失敗種別を追加する前提の
/// ため `#[non_exhaustive]` を付ける（[`EngineKind`] が非 `#[non_exhaustive]`
/// なのとは事情が異なる。`EngineKind` は spec が種別を V8・Boa の 2 つに
/// 固定しているのに対し、本型は失敗理由の分類がエンジン実装依存で今後
/// 増える）。
#[derive(Debug)]
#[non_exhaustive]
pub enum JsEngineError {
    /// スクリプト評価が失敗した（構文エラー・実行時例外・タイムアウトに
    /// よる打ち切り等）。
    EvaluationFailed(String),
    /// グローバル関数・DOM 風オブジェクトの登録に失敗した。
    BindingFailed(String),
    /// スクリプト評価がリソース上限（現状はヒープ上限）に到達したために
    /// 打ち切られた（`JS-1`・Issue #506・#508・#509・#503）。
    ///
    /// [`EvaluationFailed`](Self::EvaluationFailed)（構文エラー・実行時
    /// 例外）や [`Timeout`](Self::Timeout)（実行時間の上限）とは別の
    /// variant にすることで、呼び出し側（core・cdp 等）がヒープ上限到達
    /// 由来の打ち切りを判別できるようにする（呼び出し側がリトライ・
    /// エンジン再生成等、他の打ち切り理由とは異なる対応を選べるように
    /// するため。security.md「偽装・回避機能の禁止」──実際の打ち切り
    /// 理由をひとまとめにして隠さない）。
    ///
    /// # V8 実装（子プロセス分離後。Issue #503）における意味
    ///
    /// 以前（案 A・Issue #507）は、同一プロセス内でヒープ上限を一時的に
    /// 広げて打ち切りを検知し、上限を元に戻して**同じエンジンを使い
    /// 続けられる**ようにしていた。現在（案 X・Issue #503）は、V8 の
    /// Isolate を子プロセスの中でだけ生成し、ヒープ上限に達すると
    /// その子プロセスが V8 の既定の fatal OOM で終了する。親
    /// （`process_engine.rs`。`js-v8` feature 有効時のみ・非公開）は
    /// この終了を検出して本 variant へ変換するが、**そのときには子の
    /// 状態（Context・グローバル変数）は失われている**。呼び出し側は
    /// 次の評価が新しいコンテキストで行われることを前提にする（メッセージ
    /// 文言に "context was discarded" を含める。security.md「偽装・
    /// 回避機能の禁止」──状態が失われたことを隠さない）。「打ち切り後に
    /// エンジンが使い続けられるかどうかは実装依存」という `JsEngine`
    /// トレイトの契約自体は変わらないが、V8 実装での具体的な意味は
    /// 「同じプロセス内で状態が残る」から「新しい子プロセス・新しい
    /// コンテキストで再開する」へ変わった。
    ResourceLimitExceeded(String),
    /// スクリプト評価が実行時間の上限を超えたために打ち切られた
    /// （`JS-1`・Issue #503）。
    ///
    /// [`EvaluationFailed`](Self::EvaluationFailed)（構文エラー・実行時
    /// 例外）とは別の variant にすることで、呼び出し側がタイムアウトを
    /// 判別できるようにする。V8 実装（子プロセス分離後）では、打ち切り
    /// 理由によってエンジンの状態が変わる:
    ///
    /// - 子の監視スレッド（watchdog）による打ち切り（`v8_engine.rs` の
    ///   `SCRIPT_EXECUTION_TIMEOUT`）: 子プロセスは生き続け、Context も
    ///   残る（従来と同じ挙動）
    ///   - Context が残る場合、メッセージに "context was discarded" は
    ///     含まれない
    /// - 親が応答待ちの期限切れで子プロセスを `kill` した場合
    ///   （`process_engine.rs` の応答待ちタイムアウト）: 子プロセスごと
    ///   終了させるため、Context は失われる
    ///   - この場合はメッセージに "context was discarded" を含める
    Timeout(String),
    /// 子プロセスの異常終了・プロトコル違反・起動失敗など、エンジンが
    /// 一時的に利用できない状態になったことを表す（`JS-1`・Issue #503）。
    ///
    /// V8 実装（子プロセス分離後）でのみ発生しうる。子プロセスの
    /// クラッシュ・プロトコル違反を検出した場合、コンテキストは失われ、
    /// 次回の評価で新しい子プロセスが自動的に起動される（メッセージに
    /// "context was discarded" を含める）。
    EngineUnavailable(String),
}

impl std::fmt::Display for JsEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EvaluationFailed(msg) => write!(f, "script evaluation failed: {msg}"),
            Self::BindingFailed(msg) => write!(f, "binding registration failed: {msg}"),
            Self::ResourceLimitExceeded(msg) => {
                write!(f, "script evaluation exceeded a resource limit: {msg}")
            }
            Self::Timeout(msg) => write!(f, "script evaluation timed out: {msg}"),
            Self::EngineUnavailable(msg) => {
                write!(f, "js engine is temporarily unavailable: {msg}")
            }
        }
    }
}

impl std::error::Error for JsEngineError {}

/// JS エンジンをトレイト越しに扱うための抽象境界（TASK-28.3・`JS-1`）。
///
/// `docs/spec/04-behavior/js-engine.md`「JS エンジンの切替方式」決定 5 が
/// 定める、V8（TASK-29）・`boa`（TASK-32）を差し替え可能にする契約の中核。
/// 呼び出し元（将来）: `fandhe-browser-core`（TASK-30）が
/// `js_stub::execute_js_stub` を実エンジン呼び出しに置換する際、本トレイト
/// 越しにスクリプト評価・関数注入・オブジェクトバインディングを行う。
///
/// `Box<dyn JsEngine>` として扱えるよう object-safe に保つ（ジェネリック
/// メソッド・`impl Trait` 戻り値を持たない）。本 crate の型のみに依存し、
/// `fandhe-browser-core` の型（実 DOM 型等）を一切参照しない（依存方向は
/// js → core ではなく core → js。self-repair-design.md「crate 間の依存
/// 方向」）。[`bind_dom_like_object`](Self::bind_dom_like_object) の
/// 「DOM 風」は「複数のネイティブメソッドを持つ名前付きオブジェクト」と
/// いう形状の抽象に過ぎず、core の実 DOM 型を渡すものではない（core 側の
/// 橋渡しは TASK-30 で実装する）。
///
/// スレッド安全性: 実装は単一スレッドでの利用を前提としてよい（本トレイト
/// に `Send`/`Sync` 境界は付けない。理由は [`NativeFn`] のドキュメントを
/// 参照）。
pub trait JsEngine {
    /// スクリプトを評価し、結果を [`JsValue`] で返す（`JS-1`「スクリプト
    /// 評価」）。
    fn evaluate_script(
        &mut self,
        script: &str,
        options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError>;

    /// グローバルスコープに Rust ネイティブ関数を 1 つ注入する（`JS-1`
    /// 「グローバル関数注入」。PoC-3 の `print` 相当）。
    fn inject_global_function(&mut self, name: &str, func: NativeFn) -> Result<(), JsEngineError>;

    /// 名前付きの DOM 風オブジェクト（複数のネイティブメソッドを持つ）を
    /// グローバルスコープにバインドする（`JS-1`「DOM 風オブジェクトへの
    /// バインディング」。PoC-3 の `dom.setText`/`dom.getText`/`dom.count`
    /// 相当）。
    fn bind_dom_like_object(
        &mut self,
        name: &str,
        methods: Vec<(String, NativeFn)>,
    ) -> Result<(), JsEngineError>;
}

/// [`create_engine`] が失敗した際のエラー（TASK-28.3・`JS-1`）。
#[derive(Debug)]
#[non_exhaustive]
pub enum CreateEngineError {
    /// 指定した種別に対応する Cargo feature が有効化されていない
    /// （[`bundled_engines`] に含まれない）。
    NotBundled {
        /// 呼び出し元が要求した種別。
        requested: EngineKind,
        /// このバイナリに実際に同梱されている種別の一覧。
        bundled: &'static [EngineKind],
    },
    /// 指定した種別は同梱されている（feature は有効）が、具象実装がまだ
    /// 存在しない（V8: `TASK-29` / boa: `TASK-32`（いずれも `MS-3`）の
    /// 完了待ち）。実装済みを装わない（REPAIR-3）ための一時的なバリアント
    /// であり、`TASK-29`/`TASK-32`（`MS-3`）完了後は該当する種別について
    /// このバリアントを返さなくなる。
    NotYetImplemented {
        /// 呼び出し元が要求した種別。
        requested: EngineKind,
    },
}

impl std::fmt::Display for CreateEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // 簡易実装: EngineKind の文字列表現（設定ファイルの "v8"/"boa" との
            // 相互変換）は TASK-91（91.2・MS-3・Issue #215）で確定するため、
            // ここでは Debug 表現（{:?}）を暫定的に使う。
            Self::NotBundled { requested, bundled } => {
                write!(
                    f,
                    "js engine {requested:?} is not bundled into this binary (bundled: {bundled:?})"
                )
            }
            Self::NotYetImplemented { requested } => {
                write!(
                    f,
                    "js engine {requested:?} is bundled but not yet implemented"
                )
            }
        }
    }
}

impl std::error::Error for CreateEngineError {}

/// [`EngineKind`] から [`JsEngine`] トレイトオブジェクトを生成する
/// （`js-engine.md` 決定5「種別からトレイトオブジェクトを生成する関数」。
/// TASK-28.3・`JS-1`）。
///
/// 呼び出し元（将来）: `fandhe-browser-core` が `TASK-30`（`MS-3`）で設定
/// （`[js] engine`）が選択した種別からトレイトオブジェクトを得る際に使う。
///
/// スタブ（現在の制限: 同梱している種別に対しても常にエラーを返し、実際の
/// エンジンは生成しない）。**契約**: 同梱していない種別（[`bundled_engines`]
/// に含まれない）には [`CreateEngineError::NotBundled`] を返す（本 Issue の
/// 受け入れ条件）。同梱していても具象実装がまだ無い間は
/// [`CreateEngineError::NotYetImplemented`] を返す（`TASK-29`/`TASK-32`
/// （`MS-3`）完了後に対応する分岐が `Ok` を返すよう置き換わる）。成功した
/// かのような値（ダミーの [`JsEngine`] 実装）を返す「成功を一律に返す
/// フォールバック」は行わない（security.md「偽装・回避機能の禁止」）。
///
/// 同梱判定は [`bundled_engines`] を再利用し、本関数内で独自の feature 分岐
/// を持たない。`match kind { .. }` に `#[cfg(feature = ...)]` を付けない
/// ことで、両 feature 同時有効時に `_` 分岐が `unreachable_patterns` になる
/// 事故を避ける（`TASK-29`/`TASK-32`（`MS-3`）でこの `match` の各アームを
/// 具象エンジン生成に置き換える際も同じ構造を保つ）。
pub fn create_engine(kind: EngineKind) -> Result<Box<dyn JsEngine>, CreateEngineError> {
    if !bundled_engines().contains(&kind) {
        return Err(CreateEngineError::NotBundled {
            requested: kind,
            bundled: bundled_engines(),
        });
    }
    match kind {
        EngineKind::V8 => Err(CreateEngineError::NotYetImplemented { requested: kind }),
        EngineKind::Boa => Err(CreateEngineError::NotYetImplemented { requested: kind }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JS-1: feature 無し（既定ビルド）では同梱エンジンが 0 件であること。
    #[test]
    #[cfg(not(any(feature = "js-v8", feature = "js-boa")))]
    fn js_1_bundled_engines_returns_empty_by_default() {
        assert_eq!(bundled_engines(), &[] as &[EngineKind]);
    }

    /// JS-1: `js-v8` のみ有効な場合、V8 のみを含む一覧を返すこと。
    #[test]
    #[cfg(all(feature = "js-v8", not(feature = "js-boa")))]
    fn js_1_bundled_engines_returns_v8_only() {
        assert_eq!(bundled_engines(), &[EngineKind::V8]);
    }

    /// JS-1: `js-boa` のみ有効な場合、Boa のみを含む一覧を返すこと。
    #[test]
    #[cfg(all(feature = "js-boa", not(feature = "js-v8")))]
    fn js_1_bundled_engines_returns_boa_only() {
        assert_eq!(bundled_engines(), &[EngineKind::Boa]);
    }

    /// JS-1: 両 feature が有効な場合、V8 → Boa の固定順で一覧を返すこと。
    #[test]
    #[cfg(all(feature = "js-v8", feature = "js-boa"))]
    fn js_1_bundled_engines_returns_v8_then_boa() {
        assert_eq!(bundled_engines(), &[EngineKind::V8, EngineKind::Boa]);
    }

    /// JS-1: 既定ビルド（feature 無し）では `create_engine` が V8・Boa
    /// いずれに対しても `NotBundled`（`bundled` は空スライス）を返すこと
    /// （受け入れ条件「生成関数が同梱していない種別に Err を返す」の本体）。
    #[test]
    #[cfg(not(any(feature = "js-v8", feature = "js-boa")))]
    fn js_1_create_engine_returns_not_bundled_err_by_default() {
        assert!(matches!(
            create_engine(EngineKind::V8),
            Err(CreateEngineError::NotBundled {
                requested: EngineKind::V8,
                bundled: []
            })
        ));
        assert!(matches!(
            create_engine(EngineKind::Boa),
            Err(CreateEngineError::NotBundled {
                requested: EngineKind::Boa,
                bundled: []
            })
        ));
    }

    /// JS-1: `js-v8` のみ有効な場合、未同梱の Boa を要求すると
    /// `NotBundled { requested: Boa, bundled: &[V8] }` を返すこと。
    #[test]
    #[cfg(all(feature = "js-v8", not(feature = "js-boa")))]
    fn js_1_create_engine_returns_not_bundled_err_for_boa_when_only_v8_is_bundled() {
        assert!(matches!(
            create_engine(EngineKind::Boa),
            Err(CreateEngineError::NotBundled {
                requested: EngineKind::Boa,
                bundled: [EngineKind::V8]
            })
        ));
    }

    /// JS-1: `js-boa` のみ有効な場合、未同梱の V8 を要求すると
    /// `NotBundled { requested: V8, bundled: &[Boa] }` を返すこと。
    #[test]
    #[cfg(all(feature = "js-boa", not(feature = "js-v8")))]
    fn js_1_create_engine_returns_not_bundled_err_for_v8_when_only_boa_is_bundled() {
        assert!(matches!(
            create_engine(EngineKind::V8),
            Err(CreateEngineError::NotBundled {
                requested: EngineKind::V8,
                bundled: [EngineKind::Boa]
            })
        ));
    }

    /// JS-1: 同梱されている種別を渡しても、具象実装がまだ無いため
    /// `NotYetImplemented` を返すこと（＝「同梱＝即利用可能」ではないことの
    /// 回帰テスト。28.2 のドキュメントコメントの契約を裏付ける）。
    #[test]
    #[cfg(any(feature = "js-v8", feature = "js-boa"))]
    fn js_1_create_engine_returns_not_yet_implemented_err_for_bundled_kind() {
        for kind in bundled_engines() {
            assert!(matches!(
                create_engine(*kind),
                Err(CreateEngineError::NotYetImplemented { requested }) if requested == *kind
            ));
        }
    }

    /// JS-1: `JsEngineError`・`CreateEngineError` が `std::error::Error` を
    /// 実装していること（コンパイル時アサート）。
    #[test]
    fn js_1_js_engine_error_and_create_engine_error_implement_std_error() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<JsEngineError>();
        assert_error::<CreateEngineError>();
    }
}
