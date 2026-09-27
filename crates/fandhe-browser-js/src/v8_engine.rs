//! V8（`rusty_v8` = crates.io の `v8` crate `=152.2.0`）の Platform 初期化と
//! Isolate 生成を受け持つ、feature `js-v8` 配下の非公開モジュール
//! （`JS-1`・TASK-29（29.2）・MS-3・Issue #153）。
//!
//! 呼び出し元（将来）: [`super::engine_trait::create_engine`] の
//! `EngineKind::V8` 分岐（TASK-29.6/29.7 で配線する。§スタブについて）。
//! 上位 crate（core・TASK-30）は [`super::engine_trait::JsEngine`]
//! トレイト越しにだけ使い、本モジュールの型を直接見ない
//! （`pub` を付けず crate 内に閉じる。AC-2）。
//!
//! # スタブについて
//!
//! 本モジュールは Platform/Isolate の初期化（29.2）に加え、[`V8Engine`] の
//! 永続 Context を使ったスクリプト評価（[`V8Engine::evaluate_script`]。
//! `TASK-29.3`・Issue #154）と、実行時間・入力サイズ・結果サイズの
//! リソース上限（AGENTS.md「リソース上限」P0・codex レビュー指摘 #154
//! 対応）を実装済みである。以下は未実装（実装済みを装わない。REPAIR-3）。
//!
//! - [`super::engine_trait::JsEngine`] トレイトへの集約（`TASK-29.6`）。
//!   本モジュールは同トレイトと同じシグネチャの inherent メソッドとして
//!   `evaluate_script` を提供するに留め、`impl JsEngine for V8Engine` は
//!   まだ書かない（29.4「グローバル関数注入」・29.5「DOM 風バインディング」
//!   と並行して進めるため、共有の impl ブロックを編集し合うコンフリクトを
//!   避ける）
//! - グローバル関数注入・DOM 風オブジェクトバインディング（`TASK-29.4`・
//!   `29.5`）
//! - `create_engine`（`engine_trait.rs`）への配線（`TASK-29.6`・`29.7`）。
//!   配線すると `tests/conformance.rs` の同梱エンジン向けコンフォーマンス
//!   テスト（注入・バインディングの契約検証を含む）が V8 に対しても走る
//!   ため、29.4・29.5 の完了を待つ
//! - `v8` 由来のエラーを共通エラー型へ変換する仕組み（`TASK-29.6`）
//! - ヒープ上限到達時の穏当な `Err` 化（`near_heap_limit_callback` に
//!   よるグレースフルな終了）。現状はヒープ上限（[`MAX_ISOLATE_HEAP_BYTES`]）
//!   に到達すると V8 の既定の OOM 処理によりプロセスが終了する既知の制約
//!   がある（コールバック型が `unsafe extern "C" fn` であり、本モジュールで
//!   新規に `unsafe` を追加する承認をまだ得ていないため見送り）。詳細・
//!   トレードオフは [`MAX_ISOLATE_HEAP_BYTES`] のドキュメントコメントを
//!   参照。穏当な `Err` 化は Issue #506 で扱う
//! - 実行時間・入力サイズ・結果サイズの上限値を呼び出し側から調整する
//!   経路（[`super::engine_trait::EvaluateOptions`] の拡張。`TASK-30`
//!   （`MS-3`）で扱う。現状は本モジュールの定数で固定値を使う）
//!
//! # プロセス全体の初期化について
//!
//! V8 の初期化（[`ensure_v8_initialized`]）はプロセスの生存期間中ずっと
//! 有効なままにし、`v8::V8::dispose()` / `v8::V8::dispose_platform()` は
//! 呼ばない。理由:
//!
//! - `v8::V8::dispose()` は `unsafe fn` であり、生きている [`v8::Isolate`]
//!   があれば未定義動作になりうる。呼び出しタイミングを安全に保証する
//!   仕組みがこのモジュールには無い
//! - dispose 後は再初期化できない（v8 152.2.0 の制約）ため、テストや
//!   将来の複数回利用（cli の再起動なしの再初期化等）と相性が悪い
//!
//! `unsafe` は本モジュールでは使わない（`dispose` に触れないため）。
//! 新たに `unsafe` が必要になった場合は実装を止めてユーザーへ報告する
//! （security.md「`unsafe` の新規追加はユーザー承認を得る」）。
//!
//! # スレッド安全性
//!
//! [`V8Engine`] は内部に `v8::OwnedIsolate` を持つため `!Send` である。
//! Isolate は生成したスレッドの中だけで使う。v8 152.2.0 の
//! `OwnedIsolate::Drop` は、同じスレッドに 2 つ以上の `OwnedIsolate` が
//! 存在する場合に「生成と逆の順序」以外で drop されると `assert!` で
//! panic する（呼び出し側が破棄順序を守ることを前提にした挙動）。
//! ライブラリコードは呼び出し側の作法に依存して panic してはならない
//! （[coding-rust.md](../../../.claude/rules/coding-rust.md)）ため、
//! 本モジュールは [`V8_ISOLATE_ACTIVE`] というスレッドローカルな
//! フラグで「同一スレッドで `V8Engine` を同時に 2 つ以上生成できない」
//! ことを構造的に強制する。同時に 1 つしか存在し得なければ、逆順制約が
//! 問題になる状況自体が発生しない。既に 1 つ存在するスレッドで
//! [`V8Engine::new`] を呼んだ場合は panic ではなく
//! [`IsolateAlreadyActiveOnThread`] エラーを返す。

use std::cell::Cell;
use std::sync::Once;
use std::sync::mpsc;
use std::time::Duration;

use super::engine_trait::{EvaluateOptions, JsEngineError, JsValue};

/// [`V8Engine::evaluate_script`] が生成する例外メッセージの上限文字数
/// （UTF-8 文字数。`chars().count()` で判定する）。
///
/// V8 の例外メッセージには JS 側が任意の長さの文字列を投げられる
/// （例: `throw 'a'.repeat(1e7)`）。上限を設けずに `JsEngineError` へ
/// 詰めるとエラー値・ログが際限なく肥大化しうるため切り詰める
/// （OWASP A04「不安全な設計」・security.md）。
const MAX_ERROR_MESSAGE_CHARS: usize = 1024;

/// [`V8Engine::evaluate_script`] が例外メッセージを V8 から取り出す際に
/// 使う一時バッファの最大バイト数（`JS-1`・codex レビュー指摘 #154 P1
/// 対応）。
///
/// 従来は `to_rust_string_lossy` で V8 文字列全体を Rust の `String` へ
/// 複製してから [`truncate_error_message`] で切り詰めていたため、巨大な
/// 文字列を throw された場合に切り詰め前の一時的なメモリ消費を防げな
/// かった。[`v8_string_prefix_lossy`] が `v8::String::write_utf8_v2` で
/// この上限までしか V8 側の文字列を読み出さないようにすることで、
/// 取り出しの段階から上限を掛ける。UTF-8 は 1 文字最大 4 バイトのため
/// [`MAX_ERROR_MESSAGE_CHARS`]（文字数）に対して余裕を持たせたバイト数
/// にする。
const MAX_ERROR_MESSAGE_EXTRACT_BYTES: usize = MAX_ERROR_MESSAGE_CHARS * 4;

/// [`V8Engine::evaluate_script`] に渡すスクリプト文字列に許容する最大
/// バイト数（`JS-1`・codex レビュー指摘 #154 P0 対応）。
///
/// 以前は `v8::String::MAX_LENGTH`（V8 が UTF-16 文字列として内部的に
/// 表現できる上限であり、アプリケーション側の入力サイズ上限ではない）と
/// 比較していたため、実質的に無制限のスクリプトを受け付けてしまって
/// いた。本定数はアプリケーション側で明示的に定めた実効的な上限
/// （OWASP A04「不安全な設計」対策。security.md）。
const MAX_SCRIPT_SOURCE_BYTES: usize = 1_048_576; // 1 MiB

/// [`V8Engine::new`] が生成する Isolate に設定するヒープサイズの上限
/// （バイト。`JS-1`・codex レビュー指摘 #154 P0 対応）。
///
/// `v8::CreateParams::heap_limits` で V8 に伝える実効的な上限。これにより
/// 1 つの Isolate が確保できるヒープが際限なく増え続けることはなくなる
/// （OWASP A04「不安全な設計」対策）。
///
/// # 既知の制限（実装済みを装わない。REPAIR-3。PR #503 レビュー指摘 P0）
///
/// 上限到達時に評価を穏当に `Err` として終了させるには
/// `v8::Isolate::add_near_heap_limit_callback` にコールバックを登録する
/// 必要があるが、そのコールバック型 `NearHeapLimitCallback` は
/// `unsafe extern "C" fn` であり、登録すると本モジュールに新たに
/// `unsafe` を追加することになる。`unsafe` の新規追加はユーザー承認を
/// 要する（[coding-rust.md](../../../.claude/rules/coding-rust.md)・
/// security.md）ため、本 Issue（自動修正）の時点ではこの承認を得られず
/// 見送る。
///
/// **したがって、この上限に到達した場合、現状は `Err` を返さずプロセス
/// 全体が終了する**（V8 の既定の OOM ハンドラの挙動。Isolate 単位での
/// グレースフルな終了ではない）。呼び出し側（core・cdp 等）は、この
/// 既知の制約を踏まえて JS 評価をプロセス分離するか、上限到達の影響範囲
/// を許容できる形で運用する必要がある。少なくとも 1 Isolate あたりの
/// ヒープ使用量には上限が付くため、無制限なメモリ枯渇（DoS）は防げるが、
/// 上限到達時のプロセス終了自体は防げない。`near_heap_limit_callback`
/// によるグレースフルな `Err` 化、またはプロセス隔離による緩和は
/// Issue #506（ユーザー承認事項: 新規 `unsafe` の追加可否）で扱う。
const MAX_ISOLATE_HEAP_BYTES: usize = 128 * 1024 * 1024; // 128 MiB

/// [`value_to_js_value`] が文字列型の評価結果を [`JsValue::String`] へ
/// 変換する際に許容する最大文字数（UTF-16 コード単位。`v8::String::length`
/// の単位。`JS-1`・codex レビュー指摘 #154 P1 対応）。
///
/// `to_rust_string_lossy` は変換前に V8 側の文字列全体を複製するため、
/// JS が巨大な文字列を返すとその複製コストを入力サイズ検査では防げない。
/// 変換前に `v8::String::length`（文字列を複製しない問い合わせ）で長さを
/// 確認し、上限超過時は変換せず `Err` を返す。結果を無断で切り詰めて
/// 返すと呼び出し元へ実際とは異なる値を渡すことになる（偽装として
/// 作用しうる）ため、切り詰めではなく `Err` にする
/// （security.md「偽装・回避機能の禁止」）。
const MAX_RESULT_STRING_UTF16_UNITS: usize = 1_048_576; // 約 1M 文字（2 MiB 相当）

/// スクリプト評価（コンパイル・実行の両方）に許容する時間の上限
/// （`JS-1`・codex レビュー指摘 #154 P0 対応・PR #503 レビュー指摘 P1
/// 対応）。
///
/// [`V8Engine::evaluate_script`] は `v8::Script::compile` を呼ぶ**前**に
/// 監視用スレッドを起動し、この時間内にコンパイル・実行が完了しなければ
/// `v8::IsolateHandle::terminate_execution`（他スレッドから安全に呼べる
/// API）で強制終了する。`while (true) {}` のような無限ループを渡されても
/// 呼び出しスレッドが戻らなくなることを防ぐ（AGENTS.md「リソース上限」
/// P0・OWASP A04「不安全な設計」対策）。
///
/// # コンパイル自体は打ち切られない場合がある（実装済みを装わない。REPAIR-3）
///
/// V8 の `TerminateExecution` は「スタックガードの割り込み要求」であり、
/// JS の実行（バイトコード実行）中のチェックポイントで効く。パーサは
/// 別の上限（割り込みで書き換わらない `real_climit`）を参照するため、
/// `v8::Script::compile` 自体はこの割り込みでは中断されない可能性が高い。
///
/// 本 crate の手元検証（`v8_engine::tests::js_1_v8_pending_termination_before_compile_is_handled`・
/// `js_1_v8_pending_termination_before_compile_of_max_sized_script_is_handled`）
/// では、監視スレッドとの実時間のレースに頼る代わりに、
/// `v8::Script::compile` を呼ぶ**前**に同期的に `terminate_execution` を
/// 呼んでおき（＝「コンパイル開始時点で既に終了要求が保留されている」
/// 状況を決定的に再現し）、[`MAX_SCRIPT_SOURCE_BYTES`] 相当（1 MiB）まで
/// の大きさのスクリプトで検証した。結果は一貫して、コンパイルは打ち切
/// られずに完了し、保留されていた終了要求はその後の `compiled.run` の
/// 開始時点で効いた（[`V8Engine::evaluate_script`] が返すエラーメッセー
/// ジが "execution exceeded ... timeout" になり、"compilation exceeded"
/// にはならなかった）。「コンパイル処理そのものに極端に時間がかかる
/// 入力」（構文解析自体が長時間かかるスクリプト）は本 crate では作れて
/// おらず、そのような入力に対する実時間ベースでの打ち切りは未検証。
///
/// したがって本定数が保証するのは「監視の起点をコンパイル開始前に
/// 早めたことで、コンパイルに要する時間もタイムアウト計測の対象時間に
/// 含まれる」ことであり、「コンパイル処理自体を強制的に打ち切れる」こと
/// ではない。[`MAX_SCRIPT_SOURCE_BYTES`] による入力サイズ上限が、
/// コンパイル時間そのものに対する現状の主な緩和策である。
const SCRIPT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(2);

/// プロセス内で V8 の Platform 初期化を 1 回だけ実行するためのフラグ。
static V8_INIT: Once = Once::new();

thread_local! {
    /// このスレッド上に生存中の [`V8Engine`]（＝`v8::OwnedIsolate`）が
    /// 既にあるかどうか。[`V8Engine::new`] で確保し [`V8Engine`] の
    /// `Drop` で解放することで、同一スレッドでの `OwnedIsolate` の
    /// 同時複数保持を防ぎ、破棄順序の `assert!` panic を型・構造レベルで
    /// 起こり得なくする（本モジュール冒頭「スレッド安全性」節）。
    static V8_ISOLATE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// V8 の Platform 初期化を冪等に行う（AC-1: 何度呼んでも panic しない）。
///
/// `std::sync::Once` により、同一プロセス内での 2 回目以降の呼び出しは
/// 何もしない。複数スレッドから同時に呼ばれても `Once` が直列化するため、
/// 競合状態にはならない。
///
/// 戻り値は `()` とする。`v8::V8::initialize_platform` /
/// `v8::V8::initialize` には失敗を表す戻り値が無く（呼び出し順序を誤ると
/// panic するのみ）、本関数は「1 回だけ・正しい順序で」呼ぶことを `Once`
/// で構造的に保証するため、`Result` で装う必要がない。
///
/// `call_once` のクロージャ内で v8 初期化 API が panic すると `Once` は
/// poison されるが、本関数はここでしか呼ばれず、かつ呼び出し順序
/// （`initialize_platform` → `initialize`）を守っているため、通常の
/// 実行経路では panic しない。
///
/// # Platform の選択（`new_unprotected_default_platform` を使う理由）
///
/// v8 152.2.0 の `V8::initialize()` のドキュメントによれば、PKU
/// （メモリ保護キー）を持つ x86-64 Linux では、`V8::initialize()` より
/// **前に**作られたスレッドが Isolate に入ると `SIGSEGV`
/// （`SEGV_PKUERR`）になりうる。この制約は `new_default_platform`
/// （protected 版）が持つ。
///
/// 本 crate 側ではスレッドの生成順を保証できない。`cargo test` の
/// テストハーネスはテスト本体より前にテストスレッドを作り、結合テスト
/// （`tests/conformance.rs`）は `cfg(test)` を付けずに lib をビルドする。
/// 本番でも core/cdp の非同期ランタイムのワーカースレッドが V8 初期化より
/// 先に作られる可能性がある。
///
/// protected 版を使うと実行時に `SIGSEGV` で落ちる経路ができてしまうため、
/// 可用性を優先して `new_unprotected_default_platform` を採用する
/// （trade-off: thread-isolated allocation によるセキュリティ強化が
/// 無効になる）。将来、cli の `main`（`TASK-41`）が他のスレッドを作る前に
/// 本関数を呼ぶ契約を確立できれば、`TASK-30`（core 統合）と合わせて
/// protected 版への切替を再検討する。
pub(crate) fn ensure_v8_initialized() {
    V8_INIT.call_once(|| {
        let platform = v8::new_unprotected_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

/// V8 の Isolate を 1 つ保持する（`JS-1`・TASK-29.2）。
///
/// `evaluate_script` 等の inherent メソッドは `TASK-29.3`〜`29.5` で順次
/// 追加するが、[`super::engine_trait::JsEngine`] トレイトへの集約
/// （`impl JsEngine for V8Engine`）は `TASK-29.6` で行う（モジュール冒頭
/// 「スタブについて」）ため、本 Issue の時点では lib 本体から生成されない
/// （`create_engine` 未配線。§スコープ境界）。テストからのみ使われるので、
/// `cfg(not(test))` の場合に `dead_code` の期待を宣言する。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "TASK-29.6/29.7 で create_engine から配線されるまで lib 本体からは未使用（REPAIR-3）"
    )
)]
pub(crate) struct V8Engine {
    /// 29.4（グローバル関数注入）・29.5（DOM 風バインディング）・本 Issue の
    /// 評価呼び出しが共有する、永続的な単一の V8 Context（`JS-1`。
    /// `js-engine.md`「文字列 in/out・数値 out」の PoC-3 相当を、呼び出しの
    /// たびに使い捨てる Context ではなく 1 つの Context の中で行うことで、
    /// [`V8Engine::evaluate_script`] を繰り返し呼んでもグローバル変数や
    /// 注入した関数が引き継がれる（29.7 に向けた前提）。
    ///
    /// **フィールド宣言順が重要**: `isolate` より前に置くこと。Rust は
    /// フィールドを宣言順に drop するため、`context`（`isolate` を指す
    /// `v8::Global`）を先に破棄してから `isolate` を破棄する必要がある。
    /// 逆順にすると、`Global::drop` が既に破棄済みの Isolate に触れる
    /// ことになり不正な状態になる。
    context: v8::Global<v8::Context>,
    /// v8 の実行コンテキストを保持する Isolate 本体。[`V8_ISOLATE_ACTIVE`]
    /// により同一スレッドでは常に高々 1 つしか生存しないため、
    /// `OwnedIsolate` の drop 順制約（生成と逆順であること）が問題になる
    /// 状況自体が起こらない。
    isolate: v8::OwnedIsolate,
}

/// 同一スレッド上に既に別の [`V8Engine`]（`OwnedIsolate`）が存在するため、
/// 新しい [`V8Engine`] を生成できないことを表すエラー。
///
/// v8 152.2.0 の `OwnedIsolate::Drop` は、同一スレッドに複数の
/// `OwnedIsolate` が存在する状態で破棄順序（生成と逆順）を誤ると
/// `assert!` で panic する。本モジュールはそもそも同一スレッドでの
/// 複数同時生成を許さない構造にすることでこれを避けており、本エラーは
/// その構造上の制約を呼び出し側へ `Result` として明示的に伝える
/// （ライブラリコードは panic させない。[coding-rust.md] 参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IsolateAlreadyActiveOnThread;

impl std::fmt::Display for IsolateAlreadyActiveOnThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "a V8Engine (v8::OwnedIsolate) is already active on this thread; \
             drop it before creating another one",
        )
    }
}

impl std::error::Error for IsolateAlreadyActiveOnThread {}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "TASK-29.6/29.7 で create_engine から配線されるまで lib 本体からは未使用（REPAIR-3）"
    )
)]
impl V8Engine {
    /// V8 の初期化（[`ensure_v8_initialized`]）を確実に行ったうえで、
    /// 新しい Isolate を生成する。
    ///
    /// `v8::Isolate::new` には失敗を返す経路が無い（内部で V8 が
    /// 未初期化だと panic するのみ）ため、本関数を経由すれば
    /// 呼び出し前に必ず初期化が済んでいる。エンジン生成時のエラー変換
    /// （[`super::engine_trait::CreateEngineError`] との対応）は
    /// `create_engine` への配線（`TASK-29.6`）で扱う。
    ///
    /// 同一スレッドで既に別の `V8Engine` が生存している場合は
    /// [`IsolateAlreadyActiveOnThread`] を返す（`OwnedIsolate` の
    /// drop 順序制約による panic を防ぐため。本モジュール冒頭
    /// 「スレッド安全性」節）。
    ///
    /// Isolate の生成に続けて、[`evaluate_script`](Self::evaluate_script)
    /// 等が使い回す永続 Context を 1 つ生成し、`v8::Global` へ昇格して
    /// 保持する（`V8Engine::context` のドキュメントコメント参照）。
    ///
    /// Isolate には [`MAX_ISOLATE_HEAP_BYTES`] をヒープ上限として設定する
    /// （`JS-1`・codex レビュー指摘 #154 P0 対応。制限事項は同定数の
    /// ドキュメントコメントを参照）。
    pub(crate) fn new() -> Result<Self, IsolateAlreadyActiveOnThread> {
        ensure_v8_initialized();
        let acquired = V8_ISOLATE_ACTIVE.with(|active| {
            if active.get() {
                false
            } else {
                active.set(true);
                true
            }
        });
        if !acquired {
            return Err(IsolateAlreadyActiveOnThread);
        }
        let create_params = v8::CreateParams::default().heap_limits(0, MAX_ISOLATE_HEAP_BYTES);
        let mut isolate = v8::Isolate::new(create_params);
        let context = {
            let scope = std::pin::pin!(v8::HandleScope::new(&mut isolate));
            let scope = scope.init();
            let context = v8::Context::new(&scope, Default::default());
            v8::Global::new(&scope, context)
        };
        Ok(Self { context, isolate })
    }

    /// この Isolate のヒープ統計から総ヒープサイズ（バイト）を返す。
    ///
    /// Isolate が実際に生成され、動作していることをテストで具体値
    /// （`> 0`）で確認するための最小限の窓口。`JsEngine` トレイトの
    /// 一部ではない（本 Issue のスコープ外。`TASK-29.3`〜）。
    #[cfg(test)]
    pub(crate) fn total_heap_size(&mut self) -> usize {
        self.isolate.get_heap_statistics().total_heap_size()
    }

    /// この Isolate に対して他スレッドから安全に呼べる
    /// `v8::IsolateHandle` を返す（テスト専用の窓口。PR #503 レビュー指摘
    /// P1 の回帰テスト用）。
    ///
    /// テストからこれを使って [`evaluate_script`](Self::evaluate_script) の
    /// 呼び出し**前**に `terminate_execution` を呼んでおくことで、
    /// 「コンパイル開始時点で既に終了要求が保留されている」状況を、
    /// 監視スレッドの起動タイミングに依存しない決定的な形で再現できる
    /// （バックグラウンドスレッドとのレース待ちに頼ると、`1 + 1` のような
    /// 極小スクリプトは監視スレッドがスケジュールされるより先に完了し
    /// うるため、タイムアウトを再現できずテストがフレークする）。
    #[cfg(test)]
    pub(crate) fn thread_safe_handle_for_test(&self) -> v8::IsolateHandle {
        self.isolate.thread_safe_handle()
    }

    /// スクリプトを評価し、結果を [`JsValue`] で返す（`JS-1`「スクリプト
    /// 評価」・`TASK-29.3`・Issue #154）。
    ///
    /// [`super::engine_trait::JsEngine::evaluate_script`] と同じシグネチャの
    /// inherent メソッドとして実装する（モジュール冒頭「スタブについて」。
    /// トレイト実装への集約は `TASK-29.6`）。呼び出しのたびに
    /// [`V8Engine::context`]（永続 Context）へ入り直すため、`var`/グローバル
    /// 変数などのスクリプト間状態は評価をまたいで引き継がれる。
    ///
    /// タイムアウトは [`SCRIPT_EXECUTION_TIMEOUT`]（固定値）を使う。
    ///
    /// # リソース上限（AGENTS.md「リソース上限」P0。codex レビュー指摘
    /// #154 対応）
    ///
    /// - 入力サイズ: [`MAX_SCRIPT_SOURCE_BYTES`] を超えるスクリプト文字列
    ///   は評価前に `Err` を返す
    /// - ヒープサイズ: Isolate 生成時（[`V8Engine::new`]）に
    ///   [`MAX_ISOLATE_HEAP_BYTES`] を上限として設定する。上限到達時の
    ///   挙動の制限事項は同定数のドキュメントコメントを参照
    /// - 実行時間: 監視は `v8::Script::compile` の**前**から始まり、
    ///   [`SCRIPT_EXECUTION_TIMEOUT`] を超えると
    ///   `v8::IsolateHandle::terminate_execution` で打ち切って `Err` を
    ///   返す。ただし打ち切りが実際に効くのは `compiled.run`（実行）段階
    ///   であり、`v8::Script::compile` 自体は中断されない（1 MiB までの
    ///   スクリプトで実測。詳細は [`SCRIPT_EXECUTION_TIMEOUT`] のドキュメ
    ///   ントコメントを参照）。`while (true) {}` のような無限ループでも
    ///   呼び出しスレッドは戻る
    /// - 結果サイズ: 文字列型の評価結果は [`MAX_RESULT_STRING_UTF16_UNITS`]
    ///   を超える場合、変換前に `Err` を返す（[`value_to_js_value`]）
    ///
    /// これらの上限値は現状すべて本モジュールの定数で固定しており、
    /// `_options`（[`EvaluateOptions`]）からは調整できない。呼び出し側が
    /// 上限値を指定できるようにする拡張は `TASK-30`（`MS-3`）で扱う
    /// （モジュール冒頭「スタブについて」）。
    ///
    /// # 現在の制限（実装済みを装わない。REPAIR-3）
    ///
    /// - microtask checkpoint は行わない（`Promise` を使うスクリプトの
    ///   挙動は未定義。`TASK-30` 以降で扱う）
    /// - 評価結果が [`JsValue`] の variant で表現できない場合（object・
    ///   array・function・symbol・bigint 等）は `Err` を返す。文字列化して
    ///   成功したかのように返す（例: `[object Object]`）ことはしない
    ///   （security.md「偽装・回避機能の禁止」）。`JsValue` は
    ///   `#[non_exhaustive]` であり、必要になった時点で variant を追加する
    ///   （`TASK-29.7`/`TASK-30`）
    pub(crate) fn evaluate_script(
        &mut self,
        script: &str,
        _options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        // 外部入力（スクリプト文字列）の経路。長さを検証してからアロケー
        // ションに使う（coding-rust.md）。[`MAX_SCRIPT_SOURCE_BYTES`] は
        // アプリケーション側で定めた実効的な上限（同定数のドキュメント
        // コメント参照。codex レビュー指摘 #154 P0 対応）。
        if script.len() > MAX_SCRIPT_SOURCE_BYTES {
            return Err(JsEngineError::EvaluationFailed(format!(
                "script source exceeds the maximum supported length of {MAX_SCRIPT_SOURCE_BYTES} bytes"
            )));
        }

        // タイムアウト監視スレッド（後述）から呼び出す `IsolateHandle` は
        // `self.isolate` を可変借用する `scope`/`tc` より前に取得する
        // （`Isolate::thread_safe_handle` は `&self` のみで済み、
        // Send + Sync な `v8::IsolateHandle` を返すため、`self` を可変借用
        // したままの別スレッドへの受け渡しにはならない）。
        let isolate_handle = self.isolate.thread_safe_handle();

        let scope = std::pin::pin!(v8::HandleScope::new(&mut self.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        // 構文エラーも捕捉するため、コンパイルの前に TryCatch を開く。
        let tc = std::pin::pin!(v8::TryCatch::new(scope));
        let tc = tc.init();

        // JS-1: `tc` の例外情報から、切り詰め済みのエラーメッセージ文字列を
        // 作る。コンパイル失敗・実行失敗の両方の分岐から呼ぶため、`tc` を
        // 参照キャプチャするクロージャにする（`tc` の具象型は v8 crate 側の
        // 多重の生存期間パラメータを持ち、フリー関数として書き下すよりも
        // 呼び出し側での型推論に任せた方が単純になる）。
        // codex レビュー指摘 #154 P1: 例外全文を `to_rust_string_lossy` で
        // 複製してから切り詰めるのではなく、`v8_string_prefix_lossy` で
        // 取り出しの段階から [`MAX_ERROR_MESSAGE_EXTRACT_BYTES`] までしか
        // V8 側の文字列を複製しないようにする。
        let extract_message = || -> String {
            let raw = if let Some(message) = tc.message() {
                let text =
                    v8_string_prefix_lossy(&tc, message.get(&tc), MAX_ERROR_MESSAGE_EXTRACT_BYTES);
                match message.get_line_number(&tc) {
                    Some(line) => format!("{text} (line {line})"),
                    None => text,
                }
            } else if let Some(exception) = tc.exception() {
                match exception.to_string(&tc) {
                    Some(s) => v8_string_prefix_lossy(&tc, s, MAX_ERROR_MESSAGE_EXTRACT_BYTES),
                    None => "unknown script error".to_string(),
                }
            } else {
                "unknown script error".to_string()
            };
            truncate_error_message(raw)
        };

        // PR #503 レビュー指摘 P1: 監視スレッドをコンパイル開始「前」に
        // 起動する。以前は `compiled.run` の直前に起動していたため、
        // 最大 [`MAX_SCRIPT_SOURCE_BYTES`] のスクリプトの構文解析・
        // コンパイルにかかる時間がタイムアウト計測の対象外だった。
        // ここで起動しておけば、後続のコンパイル・実行のどちらの段階で
        // [`SCRIPT_EXECUTION_TIMEOUT`] を超えても `terminate_execution` が
        // 発火する（同定数のドキュメントコメント「コンパイル自体は打ち
        // 切られない場合がある」も参照）。
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if done_rx.recv_timeout(SCRIPT_EXECUTION_TIMEOUT).is_err() {
                isolate_handle.terminate_execution();
            }
        });
        // 監視スレッドへ完了を伝えて join し、`terminate_execution` が
        // 発火していたかどうかを読み取ってから解除する。コンパイル失敗・
        // 実行失敗のどちらの打ち切りでも同じ手順（送信 → join →
        // `has_terminated` → `cancel_terminate_execution`）を踏む必要が
        // あるため、`done_tx`/`watchdog` を消費するクロージャにまとめる
        // （呼び出しは高々 1 回。`tc` は共有参照でキャプチャする）。
        // `cancel_terminate_execution` を呼ぶと `has_terminated` が false に
        // 戻ってしまうため、解除より先に判定結果を読み取る。監視スレッドを
        // 生存させたまま関数を抜けると、次回の呼び出し中に前回のタイム
        // アウトが誤発火しうるため、打ち切り・非打ち切りのどちらの経路
        // でも必ず呼ぶ。
        let finish_watchdog =
            |done_tx: mpsc::Sender<()>, watchdog: std::thread::JoinHandle<()>| -> bool {
                let _ = done_tx.send(());
                let _ = watchdog.join();
                let was_terminated = tc.has_terminated();
                tc.cancel_terminate_execution();
                was_terminated
            };

        let source = match v8::String::new(&tc, script) {
            Some(source) => source,
            None => {
                finish_watchdog(done_tx, watchdog);
                return Err(JsEngineError::EvaluationFailed(
                    "failed to allocate script source string".to_string(),
                ));
            }
        };

        // 打ち切り済みかどうかに応じたタイムアウトメッセージを組み立てる。
        // `stage`（"compilation"/"execution"）でどちらの段階の打ち切りかを
        // 呼び出し側・テストが判別できるようにする（PR #503 レビュー
        // 指摘 P1 の回帰テストがこの文言差で判別する）。
        let timeout_message = |stage: &str| -> String {
            format!(
                "script {stage} exceeded the {} second timeout and was terminated",
                SCRIPT_EXECUTION_TIMEOUT.as_secs_f64()
            )
        };

        let compile_result = v8::Script::compile(&tc, source, None);

        let compiled = match compile_result {
            Some(compiled) => compiled,
            None => {
                // コンパイル段階で打ち切られた場合も、実行段階の打ち切りと
                // 同じ `JsEngineError::EvaluationFailed` を返す（呼び出し側
                // から見て打ち切り理由の扱いを変えない）。
                let was_terminated = finish_watchdog(done_tx, watchdog);
                let message = if was_terminated {
                    timeout_message("compilation")
                } else {
                    extract_message()
                };
                return Err(JsEngineError::EvaluationFailed(message));
            }
        };

        let run_result = compiled.run(&tc);
        let was_terminated = finish_watchdog(done_tx, watchdog);

        let result = match run_result {
            Some(result) => result,
            None => {
                let message = if was_terminated {
                    timeout_message("execution")
                } else {
                    extract_message()
                };
                return Err(JsEngineError::EvaluationFailed(message));
            }
        };

        value_to_js_value(&tc, result)
    }
}

/// [`V8Engine::evaluate_script`] のエラーメッセージ文字列を
/// [`MAX_ERROR_MESSAGE_CHARS`] 文字に切り詰める。切り詰めた場合は末尾に
/// `"..."` を付ける。文字境界（Unicode scalar value）単位で切るため
/// `chars()` を使う（バイト単位の添字アクセスは文字境界を壊しうる）。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "TASK-29.6/29.7 で create_engine から配線されるまで lib 本体からは未使用（REPAIR-3）"
    )
)]
fn truncate_error_message(message: String) -> String {
    if message.chars().count() <= MAX_ERROR_MESSAGE_CHARS {
        return message;
    }
    let truncated: String = message.chars().take(MAX_ERROR_MESSAGE_CHARS).collect();
    format!("{truncated}...")
}

/// V8 の文字列を、`max_bytes` までの UTF-8 バイト列に限定して Rust の
/// `String` へ変換する（`JS-1`・codex レビュー指摘 #154 P1 対応）。
///
/// `v8::String::to_rust_string_lossy` は変換前に V8 側の文字列全体を
/// 複製するため、巨大な文字列（例外メッセージ等）に対しては呼び出しの
/// 時点で一時的なメモリ消費を抑えられない。本関数は
/// `v8::String::write_utf8_v2` で固定長バッファへ直接書き込み、V8 側に
/// `max_bytes` を超える範囲を複製させない。`WriteFlags::kReplaceInvalidUtf8`
/// を指定するため、マルチバイト文字の途中でバッファが尽きても不正な
/// UTF-8 にはならない。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "TASK-29.6/29.7 で create_engine から配線されるまで lib 本体からは未使用（REPAIR-3）"
    )
)]
fn v8_string_prefix_lossy(
    scope: &v8::Isolate,
    value: v8::Local<v8::String>,
    max_bytes: usize,
) -> String {
    let mut buffer = vec![0u8; max_bytes];
    let written = value.write_utf8_v2(
        scope,
        &mut buffer,
        v8::WriteFlags::kReplaceInvalidUtf8,
        None,
    );
    buffer.truncate(written);
    String::from_utf8(buffer)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// V8 のスクリプト評価結果（`Local<Value>`）を、エンジン非依存の
/// [`JsValue`] へ変換する（`JS-1`・`TASK-29.3`）。
///
/// 簡易実装（現在の制限: オブジェクト・配列・関数・シンボル・BigInt 等の
/// 複合値は [`JsValue`] の variant が無いため `Err` にする。`JsValue` は
/// `#[non_exhaustive]` であり、必要になった時点で `TASK-29.7`/`TASK-30` で
/// variant を追加する。過剰設計を避ける。REPAIR-3）。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "TASK-29.6/29.7 で create_engine から配線されるまで lib 本体からは未使用（REPAIR-3）"
    )
)]
fn value_to_js_value(
    scope: &v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
) -> Result<JsValue, JsEngineError> {
    if value.is_undefined() {
        Ok(JsValue::Undefined)
    } else if value.is_null() {
        Ok(JsValue::Null)
    } else if value.is_boolean() {
        Ok(JsValue::Bool(value.boolean_value(scope)))
    } else if value.is_number() {
        match value.number_value(scope) {
            Some(number) => Ok(JsValue::Number(number)),
            None => Err(JsEngineError::EvaluationFailed(
                "failed to convert V8 number result to f64".to_string(),
            )),
        }
    } else if value.is_string() {
        // codex レビュー指摘 #154 P1: `to_rust_string_lossy` は変換前に
        // V8 側の文字列全体を複製する。`v8::String::length`（複製を伴わない
        // 問い合わせ）で長さを確認し、上限超過時は変換自体を行わず `Err`
        // を返す（結果を無断で切り詰めると誤った値を返すことになるため、
        // 切り詰めではなく `Err` にする。security.md「偽装・回避機能の禁止」）。
        match value.to_string(scope) {
            Some(s) if s.length() > MAX_RESULT_STRING_UTF16_UNITS => {
                Err(JsEngineError::EvaluationFailed(format!(
                    "string result exceeds the maximum supported length of {MAX_RESULT_STRING_UTF16_UNITS} UTF-16 code units"
                )))
            }
            Some(s) => Ok(JsValue::String(s.to_rust_string_lossy(scope))),
            None => Err(JsEngineError::EvaluationFailed(
                "failed to convert V8 string result to a UTF-16 string".to_string(),
            )),
        }
    } else {
        let type_name = value.type_of(scope).to_rust_string_lossy(scope);
        Err(JsEngineError::EvaluationFailed(format!(
            "evaluation result of type '{type_name}' is not representable as JsValue"
        )))
    }
}

impl Drop for V8Engine {
    /// このスレッドの [`V8_ISOLATE_ACTIVE`] フラグを解放し、以後
    /// このスレッドで新しい `V8Engine` を生成できるようにする。
    fn drop(&mut self) {
        V8_ISOLATE_ACTIVE.with(|active| active.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JS-1・TASK-29.2: `ensure_v8_initialized` を複数回呼んでも panic
    /// しないこと（AC-1）。副作用として V8 が実際に初期化されたことを、
    /// バージョン文字列が取得でき、先頭要素が数値として解釈できることで
    /// 具体的に確認する。
    #[test]
    fn js_1_v8_initialization_is_idempotent() {
        ensure_v8_initialized();
        ensure_v8_initialized();
        ensure_v8_initialized();

        let version = v8::V8::get_version();
        assert!(!version.is_empty(), "v8 version string must not be empty");
        let major = version
            .split('.')
            .next()
            .expect("version string must contain at least one component");
        assert!(
            major.parse::<u32>().is_ok(),
            "v8 version major component must be numeric, got: {major}"
        );
    }

    /// JS-1・TASK-29.2: 同じスレッドで Isolate の生成・破棄を繰り返しても
    /// 問題なく、毎回ヒープが確保されていること（AC-1）。2 つの
    /// `V8Engine` を同時に持たないことで `OwnedIsolate` の drop 順序制約
    /// （生成と逆順）を避ける。
    #[test]
    fn js_1_v8_isolate_can_be_created_repeatedly() {
        for _ in 0..3 {
            let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
            let heap_size = engine.total_heap_size();
            assert!(
                heap_size > 0,
                "newly created isolate must report a nonzero heap size"
            );
            drop(engine);
        }
    }

    /// JS-1・TASK-29.2: 同一スレッドで前の `V8Engine` を drop する前に
    /// 2 つ目を生成しようとすると、panic ではなく
    /// `IsolateAlreadyActiveOnThread` を返すこと。`OwnedIsolate` の
    /// drop 順序制約による panic を型レベルで避けていることの検証。
    #[test]
    fn js_1_v8_engine_new_rejects_second_isolate_on_same_thread() {
        let first = V8Engine::new().expect("first V8Engine must succeed");

        match V8Engine::new() {
            Ok(_) => panic!("a second V8Engine must not be created on the same thread"),
            Err(err) => assert_eq!(err, IsolateAlreadyActiveOnThread),
        }

        drop(first);

        // 先に drop すれば再び生成できる（フラグが解放されていること）。
        let third = V8Engine::new();
        assert!(third.is_ok());
    }

    /// JS-1・TASK-29.2: 複数スレッドから同時に初期化・Isolate 生成を
    /// 行っても、`Once` による直列化と unprotected Platform の採用により
    /// panic・クラッシュしないこと（AC-1）。`cargo test` は既定で複数の
    /// テストを並列実行するため、他のテストと合わせて実行されること
    /// 自体もこの検証の一部になる。
    #[test]
    fn js_1_v8_initialization_and_isolate_creation_from_multiple_threads() {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    ensure_v8_initialized();
                    let mut engine =
                        V8Engine::new().expect("each thread has its own V8_ISOLATE_ACTIVE flag");
                    engine.total_heap_size()
                })
            })
            .collect();

        for handle in handles {
            let heap_size = handle.join().expect("worker thread must not panic");
            assert!(
                heap_size > 0,
                "isolate created from a worker thread must report a nonzero heap size"
            );
        }
    }

    /// JS-1・TASK-29.3: 算術式を評価し、`JsValue::Number` で受け取れること
    /// （受け入れ条件「単純な JS 式を評価できる」）。
    #[test]
    fn js_1_v8_evaluate_arithmetic() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let result = engine
            .evaluate_script("1 + 2 * 3", &EvaluateOptions::default())
            .expect("arithmetic expression must evaluate successfully");
        assert_eq!(result, JsValue::Number(7.0));
    }

    /// JS-1・TASK-29.3: 文字列式（連結）を評価し、`JsValue::String` で
    /// 受け取れること。
    #[test]
    fn js_1_v8_evaluate_string_concat() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let result = engine
            .evaluate_script("'fandhe' + '-' + 'browser'", &EvaluateOptions::default())
            .expect("string concatenation must evaluate successfully");
        assert_eq!(result, JsValue::String("fandhe-browser".to_string()));
    }

    /// JS-1・TASK-29.3: `true`/`null`/`undefined` の各リテラルが対応する
    /// `JsValue` variant に変換されること。
    #[test]
    fn js_1_v8_evaluate_primitive_literals() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        assert_eq!(
            engine
                .evaluate_script("true", &EvaluateOptions::default())
                .expect("boolean literal must evaluate successfully"),
            JsValue::Bool(true)
        );
        assert_eq!(
            engine
                .evaluate_script("null", &EvaluateOptions::default())
                .expect("null literal must evaluate successfully"),
            JsValue::Null
        );
        assert_eq!(
            engine
                .evaluate_script("undefined", &EvaluateOptions::default())
                .expect("undefined literal must evaluate successfully"),
            JsValue::Undefined
        );
    }

    /// JS-1・TASK-29.3: 構文エラーは panic ではなく
    /// `Err(JsEngineError::EvaluationFailed(..))` を返すこと（受け入れ条件
    /// AC-2）。メッセージに V8 由来の `SyntaxError` を含むことを確認する。
    #[test]
    fn js_1_v8_evaluate_syntax_error_returns_err() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        match engine.evaluate_script("1 +", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("SyntaxError"),
                    "expected a SyntaxError message, got: {msg}"
                );
            }
            other => panic!("expected EvaluationFailed for a syntax error, got: {other:?}"),
        }
    }

    /// JS-1・TASK-29.3: 実行時例外（`throw`）も panic ではなく
    /// `Err(JsEngineError::EvaluationFailed(..))` を返すこと（AC-2）。
    #[test]
    fn js_1_v8_evaluate_runtime_exception_returns_err() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        match engine.evaluate_script("throw new Error('boom')", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("boom"),
                    "expected message to contain the thrown error text, got: {msg}"
                );
            }
            other => panic!("expected EvaluationFailed for a runtime exception, got: {other:?}"),
        }
    }

    /// JS-1・TASK-29.3: エラーの後もエンジンが使い続けられること（`TryCatch`
    /// がリークせず、次の評価に影響しないことの確認）。
    #[test]
    fn js_1_v8_engine_usable_after_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        assert!(
            engine
                .evaluate_script("throw new Error('boom')", &EvaluateOptions::default())
                .is_err()
        );
        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a prior evaluation error");
        assert_eq!(result, JsValue::Number(42.0));
    }

    /// JS-1・TASK-29.3: 永続 Context を使うため、`var` で定義した変数が
    /// 別の評価呼び出しをまたいで参照できること（29.4 の前提となる挙動）。
    #[test]
    fn js_1_v8_context_state_persists_across_evaluations() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .evaluate_script("var x = 1;", &EvaluateOptions::default())
            .expect("variable declaration must evaluate successfully");
        let result = engine
            .evaluate_script("x + 1", &EvaluateOptions::default())
            .expect("previously declared variable must still be visible");
        assert_eq!(result, JsValue::Number(2.0));
    }

    /// JS-1・TASK-29.3: `JsValue` の variant で表現できない結果（オブジェ
    /// クト等）は、文字列化して成功を装うのではなく `Err` を返すこと
    /// （security.md「偽装・回避機能の禁止」）。
    #[test]
    fn js_1_v8_evaluate_unsupported_result_type_returns_err() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        match engine.evaluate_script("({})", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("object"),
                    "expected message to mention the unsupported type, got: {msg}"
                );
            }
            other => {
                panic!("expected EvaluationFailed for an unsupported result type, got: {other:?}")
            }
        }
    }

    /// JS-1・TASK-29.3: 巨大な例外メッセージが [`MAX_ERROR_MESSAGE_CHARS`]
    /// 文字に切り詰められること（OWASP A04 対策の回帰確認）。
    #[test]
    fn js_1_v8_error_message_is_truncated() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        match engine.evaluate_script("throw 'a'.repeat(100000)", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.chars().count() <= MAX_ERROR_MESSAGE_CHARS + 3,
                    "message must be truncated to at most {} chars (+3 for the ellipsis), got {} chars",
                    MAX_ERROR_MESSAGE_CHARS,
                    msg.chars().count()
                );
            }
            other => {
                panic!("expected EvaluationFailed for a thrown oversized string, got: {other:?}")
            }
        }
    }

    /// JS-1・TASK-29.2/29.3: 生成・評価・drop を繰り返しても、`context`
    /// フィールドの drop 順（`isolate` より前）が正しく保たれ、クラッシュ
    /// しないことの回帰確認（`V8Engine` 構造体のドキュメントコメント参照）。
    #[test]
    fn js_1_v8_engine_drop_with_context_does_not_crash() {
        for _ in 0..3 {
            let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
            let result = engine
                .evaluate_script("1 + 1", &EvaluateOptions::default())
                .expect("evaluation must succeed before drop");
            assert_eq!(result, JsValue::Number(2.0));
            drop(engine);
        }
    }

    /// JS-1・codex レビュー指摘 #154 P0 の回帰確認: [`MAX_SCRIPT_SOURCE_BYTES`]
    /// を超えるスクリプト文字列は評価せず `Err` を返すこと。
    #[test]
    fn js_1_v8_evaluate_oversized_script_returns_err() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let oversized_script = "1".repeat(MAX_SCRIPT_SOURCE_BYTES + 1);
        match engine.evaluate_script(&oversized_script, &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("maximum supported length"),
                    "expected a message about the input size limit, got: {msg}"
                );
            }
            other => panic!("expected EvaluationFailed for an oversized script, got: {other:?}"),
        }
    }

    /// JS-1・codex レビュー指摘 #154 P0 の回帰確認: `while (true) {}` の
    /// ような無限ループが [`SCRIPT_EXECUTION_TIMEOUT`] で強制終了され、
    /// 呼び出しスレッドが戻ってくること（AGENTS.md「リソース上限」）。
    #[test]
    fn js_1_v8_evaluate_infinite_loop_times_out() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        match engine.evaluate_script("while (true) {}", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("timeout"),
                    "expected a timeout message, got: {msg}"
                );
            }
            other => panic!("expected EvaluationFailed for an infinite loop, got: {other:?}"),
        }
        // タイムアウト後もエンジンが使い続けられること（`terminate_execution`
        // の解除（`cancel_terminate_execution`）が効いていることの確認）。
        let result = engine
            .evaluate_script("1 + 1", &EvaluateOptions::default())
            .expect("engine must remain usable after a timeout");
        assert_eq!(result, JsValue::Number(2.0));
    }

    /// JS-1・codex レビュー指摘 #154 P1 の回帰確認: 評価結果の文字列が
    /// [`MAX_RESULT_STRING_UTF16_UNITS`] を超える場合、変換せず `Err` を
    /// 返すこと（切り詰めて成功を装わない。security.md「偽装・回避機能の
    /// 禁止」）。
    #[test]
    fn js_1_v8_evaluate_oversized_result_string_returns_err() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let script = format!("'a'.repeat({})", MAX_RESULT_STRING_UTF16_UNITS + 1);
        match engine.evaluate_script(&script, &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("UTF-16"),
                    "expected a message about the result size limit, got: {msg}"
                );
            }
            other => panic!("expected EvaluationFailed for an oversized result, got: {other:?}"),
        }
    }

    /// JS-1・PR #503 レビュー指摘 P1 の回帰確認: `v8::Script::compile` の
    /// 呼び出し時点で既に `terminate_execution` による終了要求が保留されて
    /// いる場合に、評価がタイムアウトとして `Err` を返し、かつその後も
    /// エンジンが使い続けられること。
    ///
    /// 監視スレッドの起動タイミングとのレースで再現しようとすると
    /// （例: `Duration::ZERO` を渡して監視スレッドの即時発火を期待する）、
    /// `1 + 1` のような極小スクリプトは監視スレッドが実際にスケジュール
    /// されるより先に完了しうるためフレークする（手元検証で確認済み）。
    /// そこで [`V8Engine::thread_safe_handle_for_test`] を使い、
    /// `evaluate_script` の呼び出し**前**に同期的に `terminate_execution`
    /// を呼んでおくことで、レースに頼らず決定的に「コンパイル開始時点で
    /// 終了要求が保留済み」の状況を作る。
    ///
    /// 手元検証では、この状態で `evaluate_script` を呼ぶと
    /// `v8::Script::compile` 自体は成功し（V8 のパーサは
    /// `TerminateExecution` の割り込みを検査しないため。
    /// [`SCRIPT_EXECUTION_TIMEOUT`] のドキュメントコメント参照）、保留
    /// されていた終了要求はその後の `compiled.run` の開始時点で効いた。
    #[test]
    fn js_1_v8_pending_termination_before_compile_is_handled() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine.thread_safe_handle_for_test().terminate_execution();

        match engine.evaluate_script("1 + 1", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                // 実測結果を具体値で固定する（REPAIR-3。手元検証と一致しない
                // 実装変更が入った場合にこのテストが検知する）: `1 + 1` 程度の
                // 小さいスクリプトでは `v8::Script::compile` は打ち切られず
                // 完了し、保留されていた終了要求は `compiled.run` の開始時点
                // で効く。したがって "execution" 段階のメッセージになる。
                assert!(
                    msg.contains("execution exceeded"),
                    "expected an execution-stage timeout message, got: {msg}"
                );
            }
            other => panic!(
                "expected EvaluationFailed due to a pending termination request, got: {other:?}"
            ),
        }
        // 打ち切り後もエンジンが使い続けられること（`cancel_terminate_execution`
        // が効いていることの確認）。
        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a pending termination request");
        assert_eq!(result, JsValue::Number(42.0));
    }

    /// JS-1・PR #503 レビュー指摘 P1 の回帰確認（コンパイル時間そのものが
    /// タイムアウト計測対象になるかの決定的な検証）:
    /// [`MAX_SCRIPT_SOURCE_BYTES`] 相当の大きさのスクリプトに対して
    /// 終了要求を保留した状態で評価しても、
    /// `js_1_v8_pending_termination_before_compile_is_handled` と同じく
    /// "execution" 段階のメッセージになること。
    ///
    /// 本 crate の手元検証では、許容される最大サイズ（1 MiB）のスクリプト
    /// でも V8 のコンパイルは打ち切られず完了した（このテストが
    /// "compilation" を返すよう変化した場合、コンパイル自体が打ち切り
    /// 可能になったことを意味する。その際は
    /// [`SCRIPT_EXECUTION_TIMEOUT`]・`evaluate_script` のドキュメント
    /// コメントの「コンパイル自体は打ち切られない場合がある」という
    /// 前提を見直す必要がある）。コンパイルにかかる実時間そのものの
    /// タイムアウト到達（無限ループ実行と違い、決定的に遅い入力を作る
    /// 手段がない）は、この保留終了要求による検証で代替する。
    #[test]
    fn js_1_v8_pending_termination_before_compile_of_max_sized_script_is_handled() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let statement = "var a=1;";
        assert_eq!(MAX_SCRIPT_SOURCE_BYTES % statement.len(), 0);
        let large_script = statement.repeat(MAX_SCRIPT_SOURCE_BYTES / statement.len());
        assert_eq!(large_script.len(), MAX_SCRIPT_SOURCE_BYTES);

        engine.thread_safe_handle_for_test().terminate_execution();

        match engine.evaluate_script(&large_script, &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("execution exceeded"),
                    "expected an execution-stage timeout message, got: {msg}"
                );
            }
            other => panic!(
                "expected EvaluationFailed due to a pending termination request, got: {other:?}"
            ),
        }
        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a pending termination request");
        assert_eq!(result, JsValue::Number(42.0));
    }

    /// JS-1・PR #503 レビュー指摘 P1 の回帰確認: 構文エラーによるコンパイル
    /// 失敗（`terminate_execution` は発火していない）でも、新しい監視スレッド
    /// の起動位置（コンパイル前）・後始末（送信・join・`has_terminated`・
    /// `cancel_terminate_execution`）が正しく機能し、`SyntaxError` を含む
    /// メッセージが得られ、その後もエンジンが使い続けられること。
    #[test]
    fn js_1_v8_syntax_error_still_reports_syntax_error_after_watchdog_refactor() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        match engine.evaluate_script("1 +", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("SyntaxError"),
                    "expected a SyntaxError message, got: {msg}"
                );
                assert!(
                    !msg.contains("timeout"),
                    "a syntax error must not be reported as a timeout, got: {msg}"
                );
            }
            other => panic!("expected EvaluationFailed for a syntax error, got: {other:?}"),
        }
        let result = engine
            .evaluate_script("1 + 1", &EvaluateOptions::default())
            .expect("engine must remain usable after a compile-time syntax error");
        assert_eq!(result, JsValue::Number(2.0));
    }
}
