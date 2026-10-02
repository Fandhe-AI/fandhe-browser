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
//! [`EngineKind`] の文字列表現（設定ファイルの `"v8"`/`"boa"` との相互変換・
//! 対応する Cargo feature 名）は TASK-91（91.2・`MS-3`・Issue #215）で確定し、
//! [`EngineKind::as_str`]・[`EngineKind::from_config_name`] 等として提供する。

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

impl EngineKind {
    /// 全種別を既定選択の優先順（V8 → Boa）で並べた配列。
    ///
    /// `js-engine.md`「(1) 省略」の「同梱エンジンから V8 → boa の優先順で選ぶ」を
    /// 実装する呼び出し元（core の `config`・TASK-91.2）が使う。
    pub const ALL: [EngineKind; 2] = [EngineKind::V8, EngineKind::Boa];

    /// 設定ファイル（`[js] engine`）での表記（`"v8"`/`"boa"`）を返す。
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::V8 => "v8",
            EngineKind::Boa => "boa",
        }
    }

    /// このエンジンを同梱する Cargo feature 名（`"js-v8"`/`"js-boa"`）を返す。
    /// 未同梱エンジン指定時のエラーメッセージで再ビルド手順を示すために使う。
    pub fn feature_name(self) -> &'static str {
        match self {
            EngineKind::V8 => "js-v8",
            EngineKind::Boa => "js-boa",
        }
    }

    /// 設定ファイルの表記から種別を得る。完全一致のみ受理し、大文字小文字の
    /// 揺れ・前後空白は `None` とする（fail-closed。黙って受理しない）。
    pub fn from_config_name(value: &str) -> Option<EngineKind> {
        EngineKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
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
/// `js-v8`・`js-boa` はどちらも子プロセス版エンジンへ配線済みで
/// （TASK-29.6.2・#548／TASK-32.2・#166）、[`create_engine`] はどちらも `Ok` を
/// 返す。boa は V8 と同じ子プロセス分離基盤の上で動き、実時間は親の期限 kill、
/// メモリは子の OS 上限と親の RSS 監視で強制する（`boa_worker` モジュール doc）。
/// [`create_engine`] はこの一覧を「同梱判定」の唯一の情報源として使う。
/// feature が無効なバリアントはコンパイル自体から除外されるため
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
/// であり、簡易実装（現在の制限: オブジェクト・配列等の複合値は表現できない）。
/// 親側 bind 済みオブジェクトは [`JsValue::ObjectHandle`] の参照としてのみ
/// 表す。必要になった時点（`TASK-29`/`TASK-32`・`MS-3`）で variant を追加する
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
    /// 親側で bind 済みのオブジェクトへの参照（`JS-1`・`TASK-29.5a`・
    /// Issue #524）。子プロセス側は実体を持たず、[`ObjectHandle`] の ID
    /// だけを保持する。
    ///
    /// 簡易実装（REPAIR-3）: 現時点では型とワイヤ表現の定義のみで、V8 の値
    /// との相互変換（プロキシオブジェクト化）は `TASK-29.5b` の後続作業
    /// で実装する（DOM 風オブジェクト自体の bind・dispatch は実装済み）。それまで `V8Engine` は本 variant を V8 値へ変換せず、
    /// JS 側で catch できる `Error` として表面化させる。
    ObjectHandle(ObjectHandle),
}

/// 親側で bind 済みの DOM 風オブジェクトを指す ID（`JS-1`・`TASK-29.5a`・
/// Issue #524）。[`JsValue::ObjectHandle`] が保持する。
///
/// - handle ID と `NativeCall` の ID（`worker_protocol` の
///   `NATIVE_CALL`/`REGISTER_GLOBAL_FUNCTION` が使う `u32`）は **別の名前
///   空間** であり、相互に流用しない
/// - 子から届いた handle ID は untrusted。親は自分の登録簿と照合し、未登録の
///   ID を拒否する契約とする（照合の実装は `TASK-29.5b` の後続作業）
/// - 生の `u32` ではなく型で区別する（REPAIR-4）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectHandle(u32);

impl ObjectHandle {
    /// 生の ID から handle を作る。ID の割り当て（親側の登録簿）は呼び出し側
    /// の責務で、本関数は値の妥当性を検証しない。
    pub const fn from_raw(id: u32) -> Self {
        Self(id)
    }

    /// handle の生の ID を返す（ワイヤ表現への変換用）。
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// 親プロセス側 `NativeCall` dispatch が [`super::process_engine::ParentNativeFn`]
/// へ渡す、呼び出し 1 回に紐づく実行コンテキスト（`TASK-29`・Issue #526）。
///
/// [`NativeFn`]（1 引数・従来の呼び出し規約のまま）とは別の型であり、
/// 既存の `NativeFn` 利用者には影響しない。親は `ParentNativeFn` を専用
/// スレッドで実行するが、実行中のスレッドを外部から強制終了する手段は
/// 無い。そのため本構造体は次の 2 つを **協調的** に伝える。
///
/// - `deadline` フィールド: 評価全体の期限
/// - `is_cancelled()`: 期限切れ・呼び出し放棄の通知。
///   親は期限内に戻らなかった呼び出しに対し待機を打ち切る際、このフラグを
///   立てる。`ParentNativeFn` は外部状態を更新する前・長い処理の区間ごとに
///   確認し、`true` なら以降の副作用を行わず直ちに戻ること（戻り値は
///   親に破棄される）。これが「期限後に処理が継続し、再試行時に旧処理と
///   新処理が重複実行される」ことを防ぐための契約である
///
/// フラグを無視する `ParentNativeFn` は止められない（`process_engine`
/// モジュールドキュメントの「既知の制限」参照）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NativeCallContext {
    /// この呼び出しが属する評価全体の期限。呼び出し元
    /// （`super::process_engine::send_evaluate_and_await`）が評価開始時に
    /// 1 度だけ計算した値で、実行時間もこの期限に含まれる。
    pub deadline: std::time::Instant,
    /// 期限切れ・放棄の通知フラグ。[`NativeCallContext::is_cancelled`] 参照。
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl NativeCallContext {
    /// 期限と取り消しフラグから文脈を作る（親側 dispatch が使用）。
    pub fn new(
        deadline: std::time::Instant,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            deadline,
            cancelled,
        }
    }

    /// 親が当該呼び出しを放棄した（期限切れ）場合に `true`。`true` の間は
    /// 外部状態への副作用を起こさず直ちに戻ること。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// [`JsEngine::inject_global_function`]・[`JsEngine::bind_dom_like_object`]
/// が受け取る Rust ネイティブ関数の型（TASK-28.3・`JS-1`）。
///
/// 引数・戻り値を [`JsValue`] に統一することで、V8/`boa` どちらの具象実装
/// からも同じシグネチャで呼び出せる（PoC-3 の `print`/`dom.setText`/
/// `dom.getText`/`dom.count` 相当）。呼び出し元（将来: `fandhe-browser-core`
/// の TASK-30）は、この関数へ渡す引数が外部入力（プラグイン入出力・ネット
/// ワーク取得データ由来）である場合、untrusted な入力として検証する責務を
/// 負う（security.md「プラグイン境界」）。
///
/// **panic 禁止（契約）**: 失敗は必ず `Err` で返す。release ビルドは
/// `panic = "abort"` のため、`catch_unwind` でもスレッド分離でも panic を
/// 封じ込められず、親プロセス内で実行される `NativeFn`（`process_engine` の
/// `NativeCall` dispatch。`TASK-29`・Issue #526）が panic するとホスト
/// プロセス全体が終了する。panic の封じ込めにはプロセス分離が必要で、
/// 現契約の範囲外（別途設計判断）。
///
/// **`Send` 境界（`TASK-29.6.2`・Issue #548）**: 子プロセス版エンジンは
/// 関数を専用スレッドで実行して期限を強制する（AGENTS.md「リソース上限」）ため
/// `Send` を要求する。`Rc` 等 `!Send` な状態は捕獲できない（`Arc<Mutex<_>>` 等を
/// 使う）。`Sync` は不要。V8 の `Isolate`/`HandleScope` 自体はスレッド固有だが、
/// 関数は `JsValue` のみを受け渡すため `Send` 境界と衝突しない。
pub type NativeFn = Box<dyn FnMut(&[JsValue]) -> Result<JsValue, JsEngineError> + Send>;

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
/// に `Send`/`Sync` 境界は付けない。V8 の `Isolate` がスレッド固有のため）。
/// 登録する [`NativeFn`] のみ `Send` を要求する（[`NativeFn`] 参照）。
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
    ///
    /// 子プロセス版の実装では、子が破棄されて起動し直された際にホストが
    /// 登録した分は新しい子へ登録し直される。スクリプトが作った状態は
    /// 失われる（`JS-1`・`TASK-29`・Issue #527）。
    ///
    /// 子プロセス版（`TASK-29.4`・Issue #155）は登録フレームで子へ即時に
    /// 登録し、子が登録を拒否した場合（`undefined` 等 non-configurable な名前・
    /// 重複・件数上限）は [`JsEngineError::BindingFailed`] を返す。
    ///
    /// 子プロセス版（`TASK-29.6.2`・Issue #548）は `func` を専用スレッドで
    /// 実行し、期限まで待つ。戻らない関数があっても `evaluate_script` は期限内に
    /// `Timeout` で戻る（放棄されたスレッドは戻るかプロセス終了まで残る。
    /// 生存数は上限あり）。期限・取り消しの通知（`NativeCallContext`）は
    /// `func` へ渡らない。unwind するビルドでは `func` 内の panic を捕捉して
    /// JS 側のエラーへ変換する（release は `panic = "abort"` でホストごと終了）。
    /// `func` へ渡る引数は JS 由来の untrusted な値である。
    fn inject_global_function(&mut self, name: &str, func: NativeFn) -> Result<(), JsEngineError>;

    /// 名前付きの DOM 風オブジェクト（複数のネイティブメソッドを持つ）を
    /// グローバルスコープにバインドする（`JS-1`「DOM 風オブジェクトへの
    /// バインディング」。PoC-3 の `dom.setText`/`dom.getText`/`dom.count`
    /// 相当）。
    ///
    /// 子プロセス版は子の再起動時に登録順どおり再登録する
    /// （`TASK-29.5b`）。全メソッドを [`inject_global_function`]
    /// （Self::inject_global_function）と同じく専用スレッドで期限付きで実行する
    /// （`TASK-29.6.2`・Issue #548）。`NativeCallContext` が渡らない・panic の
    /// 扱いも同じ。
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
    /// 指定した種別が現在の環境では利用できない。V8（TASK-29.6.2・#548）・boa
    /// （TASK-32.2・#166）とも配線済みだが、macOS の boa は確保時に効くメモリ
    /// 上限を OS で強制できないため無効にしており、この値を返す（成功を
    /// 装わない。REPAIR-3）。feature 無効側の防御的分岐でも使う。
    NotYetImplemented {
        /// 呼び出し元が要求した種別。
        requested: EngineKind,
    },
}

impl std::fmt::Display for CreateEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBundled { requested, bundled } => {
                let list = if bundled.is_empty() {
                    "none".to_string()
                } else {
                    bundled
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                write!(
                    f,
                    "js engine \"{}\" is not bundled into this binary (bundled: {list})",
                    requested.as_str()
                )
            }
            Self::NotYetImplemented { requested } => {
                write!(
                    f,
                    "js engine \"{}\" is bundled but not yet implemented",
                    requested.as_str()
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
/// **契約**: 同梱していない種別（[`bundled_engines`] に含まれない）には
/// [`CreateEngineError::NotBundled`] を返し、別エンジンへのフォールバックは
/// しない。V8・boa とも子プロセス版エンジン（`process_engine::V8ProcessEngine`。
/// boa は `EngineKind::Boa` を指定して生成。`TASK-32.2`・#166）を `Ok` で返す。
/// boa には中断 API・ヒープ上限 API が無いため、プロセス境界で実時間（親の
/// 期限 kill）とメモリ（子の OS 上限・親の RSS 監視）を強制する。
///
/// # 起動タイミングと失敗の現れ方（`TASK-29.6.2`・#548。`PERF-6`・`PERF-7`・`CORE-3`）
///
/// V8 版・boa 版とも**遅延起動**で、本関数は I/O を行わず子プロセスを起動しない。子が
/// 起動するのは `evaluate_script`・`inject_global_function`・
/// `bind_dom_like_object` のうち最初に呼ばれたもので、起動・ハンドシェイクの
/// 失敗は `CreateEngineError` ではなく、その呼び出しが返す
/// [`JsEngineError::EngineUnavailable`] として現れる。
///
/// **ホストの義務**: 子は同じ実行ファイルの自己再実行なので、ホストの
/// バイナリは `main` の先頭で [`crate::run_js_worker_if_requested`] を呼ぶこと。
/// 呼ばないと、子として再実行されたホストが通常の `main` を実行してしまい、
/// ハンドシェイクの期限（5 秒）後に初回呼び出しが `EngineUnavailable` で
/// 失敗する。
///
/// 同梱判定は [`bundled_engines`] を再利用し、本関数内で独自の feature 分岐
/// を持たない。`match kind { .. }` に `#[cfg(feature = ...)]` を付けない
/// ことで、両 feature 同時有効時に `_` 分岐が `unreachable_patterns` になる
/// 事故を避ける（V8 の生成部分だけを `#[cfg]` 付きの関数へ出している）。
pub fn create_engine(kind: EngineKind) -> Result<Box<dyn JsEngine>, CreateEngineError> {
    if !bundled_engines().contains(&kind) {
        return Err(CreateEngineError::NotBundled {
            requested: kind,
            bundled: bundled_engines(),
        });
    }
    match kind {
        EngineKind::V8 => create_v8_engine(),
        EngineKind::Boa => create_boa_engine(),
    }
}

/// boa の子プロセス版エンジンを生成する（[`create_engine`] から呼ばれる。
/// 子は最初の操作まで起動しない）。
///
/// macOS では boa を無効にする（fail-closed。`JS-1`・`TASK-32.2`・Issue #166・
/// AGENTS.md「リソース上限」P0）。boa にはヒープ上限 API が無く、macOS の子
/// プロセスには確保時に効く OS 側のメモリ上限が無い（`resource_limits` の
/// モジュール doc。`setrlimit` が `EINVAL`）ため、親の事後 RSS 監視だけでは
/// 強制になっていないと判断した。確保時に効く上限を macOS でも掛けられる
/// ようになった時点で本分岐を外す。**実装済みを装わず**
/// [`CreateEngineError::NotYetImplemented`] を返す（REPAIR-3）。
#[cfg(all(feature = "js-boa", target_os = "macos"))]
fn create_boa_engine() -> Result<Box<dyn JsEngine>, CreateEngineError> {
    Err(CreateEngineError::NotYetImplemented {
        requested: EngineKind::Boa,
    })
}

#[cfg(all(feature = "js-boa", not(target_os = "macos")))]
fn create_boa_engine() -> Result<Box<dyn JsEngine>, CreateEngineError> {
    Ok(Box::new(
        crate::process_engine::V8ProcessEngine::with_engine_kind(EngineKind::Boa),
    ))
}

/// `js-boa` 無効時の boa 分岐。[`create_engine`] は同梱判定で先に
/// `NotBundled` を返すため到達しないが、成功を装わず `NotYetImplemented` を返す。
#[cfg(not(feature = "js-boa"))]
fn create_boa_engine() -> Result<Box<dyn JsEngine>, CreateEngineError> {
    Err(CreateEngineError::NotYetImplemented {
        requested: EngineKind::Boa,
    })
}

/// V8 の子プロセス版エンジンを生成する（[`create_engine`] から呼ばれる。
/// 子は起動しない）。
#[cfg(feature = "js-v8")]
fn create_v8_engine() -> Result<Box<dyn JsEngine>, CreateEngineError> {
    Ok(Box::new(crate::process_engine::V8ProcessEngine::new()))
}

/// `js-v8` 無効時の V8 分岐。[`create_engine`] は同梱判定で先に
/// [`CreateEngineError::NotBundled`] を返すため到達しない。到達した場合も
/// 成功を装わず `Err` を返す。
#[cfg(not(feature = "js-v8"))]
fn create_v8_engine() -> Result<Box<dyn JsEngine>, CreateEngineError> {
    Err(CreateEngineError::NotYetImplemented {
        requested: EngineKind::V8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TASK-91.2: 全種別の設定名・feature 名の対応表。
    #[test]
    fn task_91_2_engine_kind_names() {
        assert_eq!(EngineKind::V8.as_str(), "v8");
        assert_eq!(EngineKind::Boa.as_str(), "boa");
        assert_eq!(EngineKind::V8.feature_name(), "js-v8");
        assert_eq!(EngineKind::Boa.feature_name(), "js-boa");
        assert_eq!(EngineKind::ALL, [EngineKind::V8, EngineKind::Boa]);
    }

    /// TASK-91.2: 設定名は完全一致のみ受理する。
    #[test]
    fn task_91_2_from_config_name_is_exact_match() {
        assert_eq!(EngineKind::from_config_name("v8"), Some(EngineKind::V8));
        assert_eq!(EngineKind::from_config_name("boa"), Some(EngineKind::Boa));
        for bad in ["V8", "Boa", "quickjs", "", " v8", "v8 "] {
            assert_eq!(EngineKind::from_config_name(bad), None, "{bad:?}");
        }
        for kind in EngineKind::ALL {
            assert_eq!(EngineKind::from_config_name(kind.as_str()), Some(kind));
        }
    }

    /// TASK-91.2: `NotBundled` の Display は設定名と同梱一覧を含む。
    #[test]
    fn task_91_2_not_bundled_display() {
        let e = CreateEngineError::NotBundled {
            requested: EngineKind::V8,
            bundled: &[],
        };
        assert_eq!(
            e.to_string(),
            "js engine \"v8\" is not bundled into this binary (bundled: none)"
        );
        let e = CreateEngineError::NotBundled {
            requested: EngineKind::V8,
            bundled: &[EngineKind::Boa],
        };
        assert_eq!(
            e.to_string(),
            "js engine \"v8\" is not bundled into this binary (bundled: boa)"
        );
    }

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

    /// JS-1: V8 を同梱したビルドでは `create_engine(V8)` が子プロセス版を
    /// `Ok` で返すこと（遅延起動のため子は起動せず、評価もしない。TASK-29.6.2）。
    #[test]
    #[cfg(feature = "js-v8")]
    fn js_1_create_engine_returns_ok_for_bundled_v8() {
        assert!(create_engine(EngineKind::V8).is_ok());
    }

    /// JS-1: boa を同梱したビルドでは `create_engine(Boa)` が boa 版を `Ok` で
    /// 返すこと（コンテキストは遅延生成のため評価はしない。TASK-32.2）。
    #[test]
    #[cfg(feature = "js-boa")]
    fn js_1_create_engine_returns_ok_for_bundled_boa() {
        assert!(create_engine(EngineKind::Boa).is_ok());
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
