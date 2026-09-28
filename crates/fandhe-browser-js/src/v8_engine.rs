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
//! `TASK-29.3`・Issue #154）と、実行時間・入力サイズ・結果サイズ・
//! ヒープサイズのリソース上限（AGENTS.md「リソース上限」P0・codex レビュー
//! 指摘 #154・#503 対応。ヒープ上限到達時の `Err` 化は Issue #506・#508・
//! #509・#510）に加え、逆方向 RPC（`NativeCall`）の**子側**（プロキシ関数の
//! 生成・呼び出し・fatal 時の打ち切り。`TASK-29`・Issue #511）を実装済み
//! である。以下は未実装（実装済みを装わない。REPAIR-3）。
//!
//! - [`super::engine_trait::JsEngine`] トレイトへの集約（`TASK-29.6`）。
//!   本モジュールは同トレイトと同じシグネチャの inherent メソッドとして
//!   `evaluate_script` を提供するに留め、`impl JsEngine for V8Engine` は
//!   まだ書かない（29.4「グローバル関数注入」・29.5「DOM 風バインディング」
//!   と並行して進めるため、共有の impl ブロックを編集し合うコンフリクトを
//!   避ける）
//! - 親→子の登録フレームを受け取って
//!   [`V8Engine::install_native_proxy_global`] を実際に呼ぶ経路
//!   （`TASK-29.4`「グローバル関数注入」・Issue #155）・DOM 風オブジェクト
//!   バインディング（`TASK-29.5`・Issue #156）。本モジュールが用意するのは
//!   プロキシ関数の生成・呼び出し本体までであり、親からの登録フレーム
//!   自体は未配線
//! - 親側の `NativeCall` dispatch（`NativeFn` の実行・`NativeReturn` の
//!   返信。`super::process_engine`・Issue #526）
//! - 子の再起動時に、以前登録していたプロキシ関数・bind 済みオブジェクトを
//!   再登録する処理（Issue #512）
//! - `create_engine`（`engine_trait.rs`）への配線（`TASK-29.6`・`29.7`）。
//!   配線すると `tests/conformance.rs` の同梱エンジン向けコンフォーマンス
//!   テスト（注入・バインディングの契約検証を含む）が V8 に対しても走る
//!   ため、29.4・29.5 の完了を待つ
//! - `v8` 由来のエラーを共通エラー型へ変換する仕組み（`TASK-29.6`）
//! - 実行時間・入力サイズ・結果サイズ・ヒープサイズの上限値を呼び出し側
//!   から調整する経路（[`super::engine_trait::EvaluateOptions`] の拡張。
//!   `TASK-30`（`MS-3`）で扱う。現状は本モジュールの定数で固定値を使う）
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
//! `dispose` に関する `unsafe` は本モジュールでは使わない。本モジュールは
//! `unsafe` を一切使わない（TASK-29・Issue #503・JS プロセス分離の設計書
//! §5「案 X」。以前存在した `near_heap_limit_callback`〔Issue #507 で承認
//! した唯一の `unsafe`〕は、ヒープ上限到達時の挙動を「V8 の既定の fatal
//! OOM で Isolate を含むプロセスが終了する」ことに一本化したため削除した。
//! この Isolate は子プロセス（`super::worker`）の中でだけ生成されるため、
//! fatal OOM で終了してもホストプロセスは道連れにならない。新たに
//! `unsafe` が必要になった場合は実装を止めてユーザーへ報告する
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
//! [`JsEngineError::BindingFailed`]（[`IsolateAlreadyActiveOnThread`] の
//! メッセージを含む）エラーを返す。

use std::cell::Cell;
use std::sync::Once;
use std::sync::mpsc;
use std::time::Duration;

use super::engine_trait::{EvaluateOptions, JsEngineError, JsValue};
use super::worker_protocol::{self, NativeReturn};

/// [`install_native_proxy_global`](V8Engine::install_native_proxy_global) が
/// 受け付ける関数名の最大バイト数（`JS-1`・`TASK-29`・Issue #511）。
///
/// グローバルオブジェクトへ登録するプロパティ名は外部入力（親からの登録
/// フレーム由来。#155）として扱い、無制限の長さの文字列で
/// `v8::String::new` を呼ばないよう、確保前に上限を検査する
/// （coding-rust.md「外部入力」節・OWASP A04）。
const MAX_NATIVE_PROXY_NAME_BYTES: usize = 256;

/// 子プロセス側から親プロセスへ逆方向 RPC（`NativeCall`）を送り、
/// [`NativeReturn`] が届くまで**同期的にブロックする**窓口（`JS-1`・
/// `TASK-29`・Issue #511）。
///
/// 呼び出し元: [`native_proxy_callback`]（本モジュール）が、JS から
/// プロキシ関数が呼ばれるたびに呼ぶ。実装は [`super::worker`] の
/// `StdioTransport`（stdio 越しに親と一問一答する）を想定するが、本
/// モジュールはその具象型に依存しない（テストでは台本どおりに応答する
/// 偽の実装を使う）。
///
/// スレッド安全性: [`V8Engine`] が `!Send` であるのと同様、本トレイトの
/// 実装も単一スレッド（Isolate を保持するスレッド）でのみ使われる前提で
/// よい（`Send`/`Sync` 境界を付けない）。
pub(crate) trait NativeCallTransport {
    /// `id` で識別されるホスト関数を `args` を渡して呼び出し、応答が
    /// 届くまでブロックする。
    fn call(&mut self, id: u32, args: &[JsValue]) -> Result<NativeReturn, NativeCallFailure>;
}

/// [`NativeCallTransport::call`] が失敗した際の分類（`JS-1`・Issue #511）。
#[derive(Debug)]
pub(crate) enum NativeCallFailure {
    /// 送信前に拒否した（非 fatal。呼び出し元は JS の `RangeError` として
    /// 投げる。子・親のプロセス・接続は生き続ける）。
    Rejected(String),
    /// プロトコル違反・EOF・I/O エラー（fatal。呼び出し元は評価を打ち切り、
    /// このプロセス自体を終了させる。[`V8Engine::take_native_call_fatal`]
    /// のドキュメントコメント参照）。
    Fatal(String),
}

/// [`V8Engine`] の Isolate スロットに設定する、逆方向 RPC の状態
/// （`JS-1`・Issue #511）。
///
/// [`V8Engine::new_with_heap_limit`] が必ず（`transport: None` の状態で）
/// 設定するため、[`native_proxy_callback`] からの `get_slot_mut` は常に
/// `Some` を返す。V8 のコールバック（`fn`。クロージャ不可）へ transport を
/// 届けるための橋渡し役。
struct NativeCallBridge {
    /// 実際の送受信を担う実装（[`super::worker::StdioTransport`] 等）。
    /// `None` のうちはプロキシ関数を呼んでも `Error` を投げる。
    transport: Option<Box<dyn NativeCallTransport>>,
    /// [`NativeCallFailure::Fatal`] を検出した際のメッセージ
    /// （[`V8Engine::take_native_call_fatal`] 参照）。
    fatal: Option<String>,
}

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
pub(crate) const MAX_SCRIPT_SOURCE_BYTES: usize = 1_048_576; // 1 MiB

/// [`V8Engine::new`] が生成する Isolate に設定するヒープサイズの上限
/// （バイト。`JS-1`・codex レビュー指摘 #154 P0 対応・Issue #503 JS
/// プロセス分離の設計書 §5「案 X」）。
///
/// `v8::CreateParams::heap_limits` で V8 に伝える実効的な上限。これにより
/// 1 つの Isolate が確保できるヒープが際限なく増え続けることはなくなる
/// （OWASP A04「不安全な設計」対策）。
///
/// # ヒープ上限到達時の挙動（子プロセスへの分離。Issue #503・#506）
///
/// 以前（案 A・Issue #507）は `near_heap_limit_callback` で上限到達を
/// 打ち切りに変換し、同じプロセス内でエンジンを使い続けられるようにして
/// いた。この方式は、単発の巨大確保（`Array.prototype.fill` 等、割り込み
/// チェックを挟まない 1 回のネイティブ呼び出しで完結する確保）に対しては
/// V8 がコールバックを呼ぶ機会が 1 回の GC シリーズあたり高々 2 回しか
/// なく、際限のない拡張を避けようとすると `FatalProcessOutOfMemory` に
/// よるプロセス全体の abort を防ぎきれない（codex レビューの P0 指摘）。
///
/// そこで本 crate は、[`V8Engine`] を子プロセス（`super::worker`）の中
/// でだけ生成する構成に変更した。ヒープ上限に達すると、コールバックで
/// 打ち切りを試みることはせず、V8 の既定の挙動どおり
/// `Fatal JavaScript out of memory: Reached heap limit` を stderr に
/// 出して、その子プロセスだけが異常終了する（正規の挙動）。ホスト
/// プロセスは Isolate を直接持たないため、この終了に巻き込まれない。
/// 親（`super::process_engine`）は子の異常終了と stderr の内容を検出し、
/// [`JsEngineError::ResourceLimitExceeded`] へ変換したうえで、次回の
/// 評価から新しい子プロセス（＝新しい `V8Engine`）を自動的に起動し直す
/// （状態は失われる。詳細は `super::process_engine` のドキュメント
/// コメントを参照）。
///
/// # 残る既知の制限（実装済みを装わない。REPAIR-3）
///
/// - この上限は V8 が管理するヒープだけに効く。`ArrayBuffer` の
///   backing store 等、ヒープ外のメモリ確保は本上限の対象外であり、
///   無制限ではない（後述のとおり `ArrayBuffer` 確保上限・OS 側の上限・
///   親側の RSS 監視という多層防御の対象になる）。詳細・具体的な値・
///   OS ごとの強制の強さは [`crate::resource_limits`] のモジュール
///   ドキュメントコメント（「OS ごとの強制の強さ」「既知の制限」節）を
///   正とする（本コメントでの重複記載はしない。codex レビュー指摘
///   #503 P0 対応）
/// - 1 つの評価の中で、割り込みチェックを挟まずに複数回の大きな確保を
///   連続して行うコードが、ヒープ上限をどれだけ超過してから
///   `FatalProcessOutOfMemory` に至るかは本 crate 側では制御できない。
///   子プロセスへの分離は「その超過分がホストへ波及しない」ことを保証
///   するものであり、「子プロセス自身の異常終了を防ぐ」ものではない
pub(crate) const MAX_ISOLATE_HEAP_BYTES: usize = 128 * 1024 * 1024; // 128 MiB

/// テスト専用のヒープ上限（[`super::worker::TEST_HEAP_LIMIT_ENV_VAR`]・
/// [`super::process_engine::WorkerSpawnConfigForTest::heap_limit_bytes`]）
/// に許す最小値（バイト。codex レビュー指摘 #503 P0「テスト専用経路が
/// 本番の上限を上回れてしまう」対応の一部）。
///
/// この下限を設けず `0` や極端に小さい値をそのまま
/// `v8::CreateParams::heap_limits` へ渡すと、Isolate 生成直後に
/// ヒープが尽きて実質どんなスクリプトも評価できなくなる（意図した
/// 「小さいが動作はする」テスト用ヒープにならない）。1 MiB は
/// [`MAX_SCRIPT_SOURCE_BYTES`]（スクリプト本体の入力上限）と同じ大きさで
/// あり、単純なスクリプトを評価できる実用上の下限として選んだ。
const MIN_TEST_HEAP_LIMIT_BYTES: usize = 1_048_576; // 1 MiB

/// テスト専用経路（`super::worker` の環境変数・`super::process_engine` の
/// [`super::process_engine::WorkerSpawnConfigForTest`]）で要求された
/// ヒープ上限を、本番の既定値（[`MAX_ISOLATE_HEAP_BYTES`]）を超えない
/// 範囲へクランプする（codex レビュー指摘 #503 P0 対応）。
///
/// テスト専用経路は「本番の既定値より小さいヒープで OOM を素早く
/// 再現する」ことだけを目的とし、既定値を上回る値を指定できてはならない
/// （AGENTS.md「リソース上限」P0）。この関数を**親側**
/// （`process_engine::spawn_worker` が子へ渡す環境変数を組み立てる際）と
/// **子側**（`worker::worker_main` が環境変数を読み取った直後）の
/// 両方で呼ぶことで、親を経由しない直接起動（環境変数を直接設定した
/// 起動）に対しても子プロセス自身が上限を超えられないことを保証する
/// （多層防御。子側の環境変数は untrusted な入力として扱う。
/// coding-rust.md「外部入力」節）。
pub(crate) fn clamp_test_heap_limit_bytes(requested_bytes: usize) -> usize {
    requested_bytes.clamp(MIN_TEST_HEAP_LIMIT_BYTES, MAX_ISOLATE_HEAP_BYTES)
}

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
pub(crate) const SCRIPT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(2);

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
        // フラグは Platform 初期化より前にしか反映されないため、
        // `initialize_platform` の前に設定する（codex レビュー指摘 #503
        // P0「ヒープ外メモリが無制限」対応の一部。wasm の単一メモリ
        // インスタンスへの上限。`super::resource_limits` のドキュメント
        // コメント参照）。
        //
        // WebAssembly を無効化する主たる対策は、ここではなく Context
        // 生成直後（[`V8Engine::new_with_heap_limit`]）のグローバル
        // オブジェクト上書きで行う（V8 152.2.0 にはグローバル露出を
        // 切り替える単一フラグが存在しないため、フラグベースのこの
        // 初期化関数では扱えない）。この関数が設定するページ数上限は、
        // その主たる対策が失敗した場合の多層防御である。
        super::resource_limits::configure_wasm_max_mem_pages_flag();
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
/// 「スタブについて」）。lib 本体からは [`super::worker`]（Issue #503・
/// JS プロセス分離。W3）が子プロセスの中でだけ生成する
/// （`create_engine` への配線は引き続き未配線。§スコープ境界）。
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
    /// （`JS-1`・codex レビュー指摘 #154 P0 対応。上限到達時の挙動は同定数の
    /// ドキュメントコメントを参照）。加えて、`ArrayBuffer` の backing
    /// store 確保量に独自の上限を課すアロケータを設定する（codex レビュー
    /// 指摘 #503 P0「macOS の JS ワーカーに強制的なメモリ上限がない」
    /// 対応。`super::resource_limits::new_bounded_array_buffer_allocator`
    /// のドキュメントコメント参照）。
    ///
    /// 戻り値のエラー型が [`JsEngineError`] である理由:
    /// [`IsolateAlreadyActiveOnThread`]（同一スレッドでの二重生成）に
    /// 加えて、WebAssembly の無効化検証に失敗した場合
    /// （[`JsEngineError::BindingFailed`]。codex レビュー指摘 #503 P1
    /// 「WebAssembly の無効化失敗を見逃している」対応）もこの関数の
    /// 呼び出し元へ伝える必要があるため。
    pub(crate) fn new() -> Result<Self, JsEngineError> {
        Self::new_with_heap_limit(MAX_ISOLATE_HEAP_BYTES)
    }

    /// [`V8Engine::new`] と同じだが、ヒープ上限を呼び出し側から指定できる。
    ///
    /// 本番経路は常に [`MAX_ISOLATE_HEAP_BYTES`]（[`V8Engine::new`] 経由）
    /// を使う。呼び出し元（将来）: `super::worker`（TASK-29・Issue #503）が
    /// テスト専用のヒープ上限（環境変数経由。本番では無効）を子プロセスの
    /// 中で使う際、および本モジュールの回帰テストがヒープ上限到達を高速に
    /// 再現する際に、この関数を直接呼ぶ（Issue #510 の経緯を引き継ぐ）。
    pub(crate) fn new_with_heap_limit(heap_limit_bytes: usize) -> Result<Self, JsEngineError> {
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
            return Err(JsEngineError::BindingFailed(
                IsolateAlreadyActiveOnThread.to_string(),
            ));
        }
        let create_params = v8::CreateParams::default()
            .heap_limits(0, heap_limit_bytes)
            .array_buffer_allocator(super::resource_limits::new_bounded_array_buffer_allocator());
        let mut isolate = v8::Isolate::new(create_params);

        let context = match Self::new_context_with_wasm_disabled(&mut isolate) {
            Ok(context) => context,
            Err(err) => {
                // Isolate 生成には成功したが、この後 `Self` を組み立てず
                // 早期リターンするため `Drop for V8Engine`（フラグ解放）が
                // 走らない。ここで明示的にフラグを戻す（`isolate` 自体は
                // このスコープを抜ける際に通常どおり drop される）。
                V8_ISOLATE_ACTIVE.with(|active| active.set(false));
                return Err(err);
            }
        };
        // 逆方向 RPC（`JS-1`・Issue #511）の橋渡し役を必ず設定する。
        // `native_proxy_callback` の `get_slot_mut` が常に `Some` を返す
        // ことをこの時点で保証する（プロキシ関数がまだ 1 つも登録されて
        // いなくても、Isolate 生成直後からこの不変条件を保つ）。
        isolate.set_slot(NativeCallBridge {
            transport: None,
            fatal: None,
        });
        Ok(Self { context, isolate })
    }

    /// [`V8Engine::new_with_heap_limit`] の本体のうち、Context 生成と
    /// WebAssembly 無効化・検証だけを切り出したもの（早期リターンが
    /// 絡む制御フローを読みやすくするため）。
    ///
    /// WebAssembly の無効化（`WebAssembly` グローバルを `undefined` へ
    /// 上書き）は、(1) 上書き自体の戻り値が `Some(true)` であること、
    /// (2) 上書き後に改めて `WebAssembly` を読み出し、`undefined` に
    /// なっていること、の両方を確認する（codex レビュー指摘 #503 P1
    /// 「`global.set` の戻り値を確認していないため、上書きに失敗しても
    /// `WebAssembly` が到達可能なまま起動しうる」対応）。どちらかが
    /// 満たされなければ、無制限の wasm メモリ確保という抜け穴を残さない
    /// よう、Isolate を起動せず [`JsEngineError::BindingFailed`] を返す
    /// （fail-closed。実装済みを装わない。REPAIR-3）。
    fn new_context_with_wasm_disabled(
        isolate: &mut v8::OwnedIsolate,
    ) -> Result<v8::Global<v8::Context>, JsEngineError> {
        let scope = std::pin::pin!(v8::HandleScope::new(isolate));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        {
            // WebAssembly を無効化する（codex レビュー指摘 #503 P0「macOS
            // の JS ワーカーに強制的なメモリ上限がない」対応。ユーザー
            // 承認 2026-09-28）。V8 152.2.0 にはこの版のグローバル露出を
            // 切り替える単一フラグが無い（`v8/src/flags/flag-definitions.h`
            // に `expose_wasm` 相当のフラグが存在しないことを実機で確認
            // 済み）ため、生成直後の Context のグローバルオブジェクトから
            // `WebAssembly` を上書きして到達不能にする。`delete` ではなく
            // `undefined` への代入にしているのは、削除には対象プロパティが
            // configurable であることが必要だが、代入は writable であれば
            // 足りる（一般に組み込みグローバルは configurable かつ
            // writable だが、writable の方が要求が弱く確実）ため。
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let global = context.global(scope);
            let Some(key) = v8::String::new(scope, "WebAssembly") else {
                return Err(JsEngineError::BindingFailed(
                    "failed to allocate the \"WebAssembly\" property key string while \
                     disabling WebAssembly"
                        .to_string(),
                ));
            };
            let key: v8::Local<v8::Value> = key.into();
            let undefined: v8::Local<v8::Value> = v8::undefined(scope).into();

            // codex レビュー指摘 #503 P1「`global.set` の戻り値を確認して
            // いないため、上書きに失敗しても `WebAssembly` が到達可能な
            // まま起動しうる」対応: 戻り値が `Some(true)` であることを
            // 確認する。V8 の `Object::Set` は「代入できたか」を
            // `Maybe<bool>` で返す契約であり、`None`（例外発生）・
            // `Some(false)`（strict mode 相当で失敗を無視せず伝える値。
            // 通常のグローバルへの代入では起こらないはずだが、本 crate が
            // 制御しない V8 のビルド設定・将来のバージョンでの挙動変化に
            // 備える）のいずれでも、無効化に失敗したとみなし fail-closed
            // にする。
            if global.set(scope, key, undefined) != Some(true) {
                return Err(JsEngineError::BindingFailed(
                    "failed to overwrite the \"WebAssembly\" global property with undefined \
                     (Object::Set did not report success)"
                        .to_string(),
                ));
            }

            // 上書きの戻り値だけに頼らず、実際に読み出して `undefined` に
            // なっていることも確認する（`Set` が成功を報告しても、
            // getter/setter の組み合わせ次第では読み出し値が異なりうる
            // ため、二重に確認する。実装済みを装わない。REPAIR-3）。
            match global.get(scope, key) {
                Some(value) if value.is_undefined() => {}
                other => {
                    return Err(JsEngineError::BindingFailed(format!(
                        "\"WebAssembly\" global property is not undefined after the overwrite \
                         (read back: {other:?}); refusing to start the isolate with \
                         WebAssembly potentially still reachable"
                    )));
                }
            }
        }
        Ok(v8::Global::new(&scope, context))
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
    ///   [`MAX_ISOLATE_HEAP_BYTES`] を上限として設定する。上限に達すると
    ///   本メソッドは `Err` を返さず、V8 の既定の挙動どおりこの
    ///   （子）プロセスが fatal OOM で終了する。この Isolate が子プロセス
    ///   の中でだけ生成される前提（`super::worker`）のもと、ホスト
    ///   プロセスへの分離・`Err` への変換は呼び出し元の
    ///   `super::process_engine` が担う（挙動の詳細は同定数のドキュメント
    ///   コメントを参照。Issue #503・#506）
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
        options: &EvaluateOptions,
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

        // JS-1・Issue #511: 逆方向 RPC の呼び出し中に fatal
        // （[`NativeCallFailure::Fatal`]）が記録済みなら、この Context は
        // 既に使い物にならない（`terminate_execution` 済みで、後続の JS
        // からのホスト呼び出しも失敗し続ける）。新しい評価を始めず
        // `Err` を返す（fail-closed。実装済みを装わない。REPAIR-3）。
        if let Some(message) = self
            .isolate
            .get_slot::<NativeCallBridge>()
            .and_then(|bridge| bridge.fatal.clone())
        {
            return Err(JsEngineError::EngineUnavailable(format!(
                "a previous native call ended the worker protocol connection; \
                 context was discarded: {message}"
            )));
        }

        Self::evaluate_in_isolate(&mut self.isolate, &self.context, script, options)
    }

    /// この Isolate に、子プロセス（[`super::worker`]）が親と通信するための
    /// [`NativeCallTransport`] 実装を設定する（`JS-1`・Issue #511）。
    ///
    /// 呼び出し元: `super::worker::worker_main` が、Hello を送る前
    /// （エンジン生成の直後）に一度だけ呼ぶ。テストでは台本どおりに応答
    /// する偽の実装を渡す。
    pub(crate) fn set_native_call_transport(&mut self, transport: Box<dyn NativeCallTransport>) {
        if let Some(bridge) = self.isolate.get_slot_mut::<NativeCallBridge>() {
            bridge.transport = Some(transport);
        }
    }

    /// 直近の [`NativeCallTransport::call`] が [`NativeCallFailure::Fatal`]
    /// を返していれば、そのメッセージを取り出す（`JS-1`・Issue #511）。
    ///
    /// 呼び出し元: `super::worker::evaluate_and_respond` が、評価の直後・
    /// 応答フレームを送信する**前**に呼ぶ。`Some` の場合、呼び出し元は
    /// 通常の `Result`/`Error` フレームを送らず、プロトコル違反として
    /// プロセスを終了させる（親はこれを EOF として検出し
    /// `EngineUnavailable` へ変換する。`super::process_engine` を参照）。
    #[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
    pub(crate) fn take_native_call_fatal(&mut self) -> Option<String> {
        self.isolate
            .get_slot_mut::<NativeCallBridge>()
            .and_then(|bridge| bridge.fatal.take())
    }

    /// 永続 Context のグローバルスコープへ、逆方向 RPC のプロキシ関数を
    /// `name` という名前で登録する（`JS-1`・`TASK-29`・Issue #511）。
    ///
    /// 呼び出し元（将来）: 親からの登録フレーム（#155）を受け取った
    /// [`super::worker`] が、注入すべきグローバル関数ごとに一意な `id` を
    /// 割り当てて呼ぶ。それまでは本番経路から呼ばれない
    /// （`#155` 完了までの間、プロキシを登録する手段が本番に無いため）。
    ///
    /// `name` は外部入力（親からの登録フレーム由来）として検証する:
    /// 空文字列・[`MAX_NATIVE_PROXY_NAME_BYTES`] 超過はいずれも `Err` にする
    /// （coding-rust.md「外部入力」節）。`global.set` の戻り値が
    /// `Some(true)` であることも確認する（`Self::new_context_with_wasm_disabled`
    /// と同じ考え方。戻り値を確認せず「登録できたと装う」ことをしない。
    /// security.md「偽装・回避機能の禁止」）。
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "親→子の登録フレーム（#155）から呼ばれるまで未配線。\
                      REPAIR-3"
        )
    )]
    pub(crate) fn install_native_proxy_global(
        &mut self,
        name: &str,
        id: u32,
    ) -> Result<(), JsEngineError> {
        if name.is_empty() {
            return Err(JsEngineError::BindingFailed(
                "native proxy function name must not be empty".to_string(),
            ));
        }
        if name.len() > MAX_NATIVE_PROXY_NAME_BYTES {
            return Err(JsEngineError::BindingFailed(format!(
                "native proxy function name exceeds the maximum supported length of \
                 {MAX_NATIVE_PROXY_NAME_BYTES} bytes"
            )));
        }

        let scope = std::pin::pin!(v8::HandleScope::new(&mut self.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        let Some(function) = new_native_proxy_function(scope, id) else {
            return Err(JsEngineError::BindingFailed(
                "failed to create the native proxy function".to_string(),
            ));
        };
        let Some(key) = v8::String::new(scope, name) else {
            return Err(JsEngineError::BindingFailed(
                "failed to allocate the native proxy function name string".to_string(),
            ));
        };
        let global = context.global(scope);
        let key: v8::Local<v8::Value> = key.into();
        let value: v8::Local<v8::Value> = function.into();
        if global.set(scope, key, value) != Some(true) {
            return Err(JsEngineError::BindingFailed(format!(
                "failed to register the native proxy function {name:?} on the global object \
                 (Object::Set did not report success)"
            )));
        }
        Ok(())
    }

    /// [`V8Engine::evaluate_script`] のコンパイル・実行本体（`JS-1`・
    /// `TASK-29.3`・Issue #154）。
    ///
    /// `&mut self` を取らず Isolate・Context を直接受け取るのは、静的関数
    /// として `self` の他フィールドから独立させ、テストや将来の変更で
    /// 借用の取り回しを単純に保つため。
    fn evaluate_in_isolate(
        isolate: &mut v8::OwnedIsolate,
        context: &v8::Global<v8::Context>,
        script: &str,
        _options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        // タイムアウト監視スレッド（後述）から呼び出す `IsolateHandle` は
        // `isolate` を可変借用する `scope`/`tc` より前に取得する
        // （`Isolate::thread_safe_handle` は `&self` のみで済み、
        // Send + Sync な `v8::IsolateHandle` を返すため、`isolate` を
        // 可変借用したままの別スレッドへの受け渡しにはならない）。
        let isolate_handle = isolate.thread_safe_handle();

        let scope = std::pin::pin!(v8::HandleScope::new(isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, context);
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
        //
        // codex 再指摘 P0 対応（PR #503）: `throw { toString() { while
        // (true) {} } }` のように、投げられた値のユーザー定義
        // `toString`/getter/`Symbol.toPrimitive` が無限ループになりうる。
        // 本クロージャはユーザー定義コードを一切実行しない経路だけを使う:
        //
        // - `tc.message()`（`v8::Message::Get`/`GetLineNumber`）は V8
        //   内部で `EnterV8NoScriptNoExceptionScope` を通る（v8 152.2.0
        //   の `src/api/api.cc` で確認済み。スクリプト実行を許さない
        //   スコープ）ため、呼び出し時点でユーザーコードを実行しない。
        //   スロー時に副作用なしで組み立てられた表示用文字列を読み出す
        //   だけである（実機検証で `toString`/getter/`Symbol.toPrimitive`
        //   のいずれも呼ばれないことを確認済み。回帰テスト参照）
        // - `tc.exception()` しか無い場合、投げられた値がオブジェクト
        //   （`is_object()`）であれば `to_string`（ECMA-262 の ToString
        //   抽象操作）を呼ばない。オブジェクトの ToString は
        //   ToPrimitive 経由でユーザー定義の `toString`/`valueOf`/
        //   `Symbol.toPrimitive` を呼びうるため。オブジェクトでない
        //   場合（文字列・数値・真偽値・`null`・`undefined`）は
        //   ToString がユーザーコードを経由しない組み込み操作のため
        //   安全に呼べる
        //
        // 実装済みを装わない（REPAIR-3）: 手元検証では、本モジュールの
        // `TryCatch` は既定で `capture_message` が有効（v8 152.2.0
        // `TryCatch` のコンストラクタで既定 `true`）であるため、
        // `tc.message()` は本 crate が実際に評価するどの入力に対しても
        // 常に `Some` を返し、`tc.exception()` 側の分岐（この
        // `is_object()` によるガード）に到達するケースを実際には再現
        // できなかった。したがって、この分岐の修正は主に「将来
        // `tc.message()` が `None` になる経路が生じた場合の防御」であり、
        // 本 PR の時点で実際に到達可能な脆弱性を塞いだことを示す再現
        // テストは書けていない（回帰テストは「ハングしない」ことを
        // 確認するに留まる。詳細は同テストのドキュメントコメント参照）
        let extract_message = || -> String {
            let raw = if let Some(message) = tc.message() {
                let text =
                    v8_string_prefix_lossy(&tc, message.get(&tc), MAX_ERROR_MESSAGE_EXTRACT_BYTES);
                match message.get_line_number(&tc) {
                    Some(line) => format!("{text} (line {line})"),
                    None => text,
                }
            } else if let Some(exception) = tc.exception() {
                if exception.is_object() {
                    // オブジェクトの文字列表現は計算しない（上記のとおり
                    // ユーザーコードの実行を避けるため）。型名だけを
                    // 報告する（文字列化に成功したかのように装わない。
                    // security.md「偽装・回避機能の禁止」）。
                    let type_name = exception.type_of(&tc).to_rust_string_lossy(&tc);
                    format!(
                        "an object of type '{type_name}' was thrown; its string \
                         representation was not computed to avoid executing user-defined code"
                    )
                } else {
                    match exception.to_string(&tc) {
                        Some(s) => v8_string_prefix_lossy(&tc, s, MAX_ERROR_MESSAGE_EXTRACT_BYTES),
                        None => "unknown script error".to_string(),
                    }
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
        //
        // codex 再指摘 P0 対応（PR #503）: コンパイル・実行が失敗した
        // 分岐では、本関数は `extract_message()` の呼び出しが終わった
        // 「後」に呼ぶ（呼び出し順序を変えた。以前は `finish_watchdog` を
        // 先に呼んでから `extract_message()` を呼んでいたため、監視
        // スレッドが既に停止した状態で `extract_message()` がユーザー
        // コードを実行すると `SCRIPT_EXECUTION_TIMEOUT` の打ち切りが
        // 効かなくなっていた）。`extract_message` はユーザー定義コードを
        // 実行しない設計にしている（同クロージャのコメント参照）が、
        // 万一その前提が崩れていても、監視スレッドがまだ生きていれば
        // `terminate_execution` で打ち切れるようにする多重の安全策。
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
                // 同じ扱いにする（呼び出し側から見て打ち切り理由の扱いを
                // 変えない）。`extract_message` を監視スレッドが生きている
                // 間に呼ぶ（`finish_watchdog` のドキュメントコメント参照）。
                //
                // Issue #503 W5: タイムアウトによる打ち切りは
                // `JsEngineError::Timeout` を返す（構文エラー等の
                // `EvaluationFailed` と呼び出し側が判別できるようにする。
                // 子プロセス分離後は、この打ち切りでも子プロセス自体は
                // 生き続け Context も残るため（`super::process_engine` が
                // 検出する「子の watchdog による打ち切り」に対応）、
                // メッセージに "context was discarded" は含めない。
                let message = extract_message();
                let was_terminated = finish_watchdog(done_tx, watchdog);
                if was_terminated {
                    return Err(JsEngineError::Timeout(timeout_message("compilation")));
                }
                return Err(JsEngineError::EvaluationFailed(message));
            }
        };

        let run_result = compiled.run(&tc);
        // `extract_message` を監視スレッドが生きている間に呼ぶ
        // （`finish_watchdog` のドキュメントコメント参照）。失敗して
        // いない場合は呼ぶ必要がなく、副作用も無いため呼ばない。
        let message = match &run_result {
            Some(_) => None,
            None => Some(extract_message()),
        };
        let was_terminated = finish_watchdog(done_tx, watchdog);

        let result = match run_result {
            Some(result) => result,
            None => {
                if was_terminated {
                    return Err(JsEngineError::Timeout(timeout_message("execution")));
                }
                let message = match message {
                    Some(message) => message,
                    None => "unknown script error".to_string(),
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

/// [`JsValue`] を V8 の値へ変換する（`JS-1`・Issue #511）。
///
/// [`value_to_js_value`] の逆方向。逆方向 RPC の戻り値
/// （[`NativeReturn::Ok`]）を JS 側の関数呼び出し結果へ変換する際に
/// [`native_proxy_callback`] から使う。`JsValue` の全 variant を扱えるため
/// `Option` を返す（`v8::String::new`・`v8::Number::new` は理論上
/// アロケーション失敗で `None` を返しうる）。
#[cfg_attr(not(any(test, feature = "js-v8")), allow(dead_code))]
fn js_value_to_v8_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsValue,
) -> Option<v8::Local<'s, v8::Value>> {
    Some(match value {
        JsValue::Undefined => v8::undefined(scope).into(),
        JsValue::Null => v8::null(scope).into(),
        JsValue::Bool(b) => v8::Boolean::new(scope, *b).into(),
        JsValue::Number(n) => v8::Number::new(scope, *n).into(),
        JsValue::String(s) => v8::String::new(scope, s)?.into(),
    })
}

/// [`V8Engine::install_native_proxy_global`] が生成する、逆方向 RPC の
/// プロキシ関数を作る（`JS-1`・Issue #511）。
///
/// `data` には `id` の数値だけを載せる（`External`・生ポインタは使わない。
/// 計画書 §3.2「data には数値 ID だけを入れる」）。子プロセスの再起動時に
/// 同じ `id` で登録し直せば、V8 側のポインタ寿命を気にせず再現できる
/// （#155・#512 が担う）。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "V8Engine::install_native_proxy_global 経由で #155 完了後に配線される。REPAIR-3"
    )
)]
fn new_native_proxy_function<'s>(
    scope: &v8::PinScope<'s, '_>,
    id: u32,
) -> Option<v8::Local<'s, v8::Function>> {
    let data: v8::Local<v8::Value> = v8::Integer::new_from_unsigned(scope, id).into();
    v8::Function::builder(native_proxy_callback)
        .data(data)
        .build(scope)
}

/// [`native_proxy_callback`] が引数列の合計エンコード後サイズを見積もる
/// ための補助関数（`JS-1`・Issue #511・codex レビュー指摘 #533 P0）。
///
/// [`value_to_js_value`] は文字列を [`v8::String::to_rust_string_lossy`]
/// で Rust の `String` へ複製する。最大 [`worker_protocol::MAX_NATIVE_CALL_ARGS`]
/// （64）個の引数それぞれが最大 [`MAX_RESULT_STRING_UTF16_UNITS`] 相当の
/// 文字列でありうるため、合計サイズを検証する前に全件を複製すると、
/// [`worker_protocol::encode_native_call`] が合計サイズ超過を検出する前に
/// 大量のプロセスメモリを確保してしまう
/// （security.md「外部入力は長さ・件数を上限検証してからアロケーションに
/// 使う」）。本関数は [`v8::String::utf8_length`]（文字列内容の複製を
/// 伴わない問い合わせ）だけを使って、[`worker_protocol::encode_js_value`]
/// が実際に書き込むバイト数（タグ 1 バイト＋種別ごとの値）を見積もる。
///
/// 表現形式は [`worker_protocol::encoded_js_value_len`] と手作業で
/// 同期する必要がある（変更時は両方を更新すること）。
/// `js_1_native_proxy_function_rejects_oversized_argument_payload_without_calling_transport`
/// が、複数引数の合計サイズが上限を超えるケースで transport が呼ばれない
/// ことを確認している。
///
/// 表現できない値（object・function 等）は `0` を返す。そうした値は
/// 呼び出し元（[`native_proxy_callback`]）の後段で必ず [`value_to_js_value`]
/// が `Err`（変換不可）にし、transport を呼ぶ前に弾くため、サイズ見積もりに
/// 含めなくても DoS 対策としては安全である。
fn native_call_arg_encoded_len(scope: &v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> usize {
    if value.is_undefined() || value.is_null() {
        1
    } else if value.is_boolean() {
        2
    } else if value.is_number() {
        9
    } else if value.is_string() {
        match value.to_string(scope) {
            // タグ 1 バイト＋長さプレフィックス（u32）4 バイト＋ UTF-8
            // バイト列。`worker_protocol::encoded_js_value_len` の
            // `JsValue::String` 分岐と一致させる。
            Some(s) => 1usize
                .saturating_add(4)
                .saturating_add(s.utf8_length(scope)),
            None => 0,
        }
    } else {
        0
    }
}

/// [`new_native_proxy_function`] が生成する関数の本体（`JS-1`・
/// Issue #511）。
///
/// JS からこの関数が呼ばれるたびに、次の手順を踏む:
///
/// 1. `data()` から `id`（`u32`）を取り出す（数値以外なら `TypeError`）
/// 2. 引数の個数が [`worker_protocol::MAX_NATIVE_CALL_ARGS`] を超える場合は
///    transport を呼ばずに `RangeError` を投げる
/// 3. 各引数を [`super::v8_engine::value_to_js_value`] 相当の変換で
///    [`JsValue`] へ変換する（表現できない値は `TypeError`。文字列化して
///    送らない。security.md「偽装・回避機能の禁止」）
/// 4. Isolate スロットの [`NativeCallBridge`] を取り、`fatal` が既にあれば
///    `terminate_execution` して打ち切る。無ければ transport を呼ぶ
///    （可変借用は呼び出しの間だけに限定し、その後 V8 の値を作る前に
///    解放する）
/// 5. 結果を JS の戻り値・例外へ変換する（[`NativeCallFailure::Fatal`] は
///    `bridge.fatal` に記録したうえで `terminate_execution` する。この
///    終了要求は JS の `try`/`catch` では捕捉できない）
///
/// # watchdog との関係
///
/// 本コールバックでブロックしている時間も
/// [`SCRIPT_EXECUTION_TIMEOUT`] に含まれる（`evaluate_in_isolate` の
/// 監視スレッドは呼び出し元のスタックを区別しない）。ネイティブ呼び出し
/// 中に watchdog が発火した場合は、本コールバックから戻った時点で V8 が
/// 打ち切る。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "V8Engine::install_native_proxy_global 経由で #155 完了後に配線される。REPAIR-3"
    )
)]
fn native_proxy_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    // `data()` は必ず `new_native_proxy_function` が設定した
    // `v8::Integer::new_from_unsigned` の値であるはずだが、コールバックの
    // 型契約自体は呼び出し元が壊せる余地を排除しない（外部入力として
    // 扱う。coding-rust.md「外部入力」節）。`is_uint32()` で型を確認して
    // から `uint32_value()`（変換）を呼ぶ。
    let data = args.data();
    let Some(id) = data.is_uint32().then(|| data.uint32_value(scope)).flatten() else {
        let Some(message) = v8::String::new(scope, "native proxy function data is not a uint32")
        else {
            return;
        };
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
        return;
    };

    let argc = args.length();
    if argc < 0 || argc as usize > worker_protocol::MAX_NATIVE_CALL_ARGS {
        let text = format!(
            "native call accepts at most {} arguments, got {argc}",
            worker_protocol::MAX_NATIVE_CALL_ARGS
        );
        let Some(message) = v8::String::new(scope, &text) else {
            return;
        };
        let exception = v8::Exception::range_error(scope, message);
        scope.throw_exception(exception);
        return;
    }

    // codex レビュー指摘 #533 P0: 件数（上記）を確認しただけでは、1 引数
    // あたり最大 ~1 MiB の文字列を渡すことで合計サイズの DoS を防げない。
    // `value_to_js_value`（複製を伴う）を呼ぶ**前**に、複製を伴わない
    // [`native_call_arg_encoded_len`] で合計サイズを見積もり、
    // [`worker_protocol::encode_native_call`] と同じ上限
    // （`MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`）で検証する。
    let mut estimated_total_size: usize = 8; // id（4B）＋ argc（4B）。encode_native_call と揃える
    for i in 0..argc {
        let encoded_len = native_call_arg_encoded_len(scope, args.get(i));
        let Some(next_total) = estimated_total_size.checked_add(encoded_len) else {
            let Some(message) = v8::String::new(
                scope,
                "native call argument payload size overflowed while estimating",
            ) else {
                return;
            };
            let exception = v8::Exception::range_error(scope, message);
            scope.throw_exception(exception);
            return;
        };
        estimated_total_size = next_total;
        if estimated_total_size > worker_protocol::MAX_FRAME_PAYLOAD_CHILD_TO_PARENT {
            let text = format!(
                "native call argument payload is too large: estimated {estimated_total_size} bytes exceeds the maximum of {} bytes",
                worker_protocol::MAX_FRAME_PAYLOAD_CHILD_TO_PARENT
            );
            let Some(message) = v8::String::new(scope, &text) else {
                return;
            };
            let exception = v8::Exception::range_error(scope, message);
            scope.throw_exception(exception);
            return;
        }
    }

    let mut js_args = Vec::with_capacity(argc as usize);
    for i in 0..argc {
        match value_to_js_value(scope, args.get(i)) {
            Ok(value) => js_args.push(value),
            Err(err) => {
                let Some(message) = v8::String::new(scope, &err.to_string()) else {
                    return;
                };
                let exception = v8::Exception::type_error(scope, message);
                scope.throw_exception(exception);
                return;
            }
        }
    }

    // Isolate スロットの可変借用はこのブロックの中だけに限定する
    // （transport 呼び出しの結果を使って V8 の値を作る前に解放する）。
    let outcome = {
        let Some(bridge) = scope.get_slot_mut::<NativeCallBridge>() else {
            let Some(message) = v8::String::new(scope, "native call bridge is not initialized")
            else {
                return;
            };
            let exception = v8::Exception::error(scope, message);
            scope.throw_exception(exception);
            return;
        };
        if let Some(fatal) = bridge.fatal.clone() {
            let _ = fatal;
            scope.terminate_execution();
            return;
        }
        let Some(transport) = bridge.transport.as_mut() else {
            let Some(message) = v8::String::new(scope, "native call transport is not configured")
            else {
                return;
            };
            let exception = v8::Exception::error(scope, message);
            scope.throw_exception(exception);
            return;
        };
        transport.call(id, &js_args)
    };

    match outcome {
        Ok(NativeReturn::Ok(value)) => {
            let Some(v8_value) = js_value_to_v8_value(scope, &value) else {
                let Some(message) = v8::String::new(
                    scope,
                    "failed to convert the native call result to a V8 value",
                ) else {
                    return;
                };
                let exception = v8::Exception::error(scope, message);
                scope.throw_exception(exception);
                return;
            };
            rv.set(v8_value);
        }
        Ok(NativeReturn::Err(message)) => {
            let Some(message) = v8::String::new(scope, &message) else {
                return;
            };
            let exception = v8::Exception::error(scope, message);
            scope.throw_exception(exception);
        }
        Err(NativeCallFailure::Rejected(message)) => {
            let Some(message) = v8::String::new(scope, &message) else {
                return;
            };
            let exception = v8::Exception::range_error(scope, message);
            scope.throw_exception(exception);
        }
        Err(NativeCallFailure::Fatal(message)) => {
            if let Some(bridge) = scope.get_slot_mut::<NativeCallBridge>() {
                bridge.fatal = Some(message);
            }
            scope.terminate_execution();
        }
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

    /// codex レビュー指摘 #503 P0: テスト専用のヒープ上限は、本番の
    /// 既定値（[`MAX_ISOLATE_HEAP_BYTES`]。128 MiB）を上回る値を要求しても
    /// 既定値そのものへクランプされ、それを超えないこと。
    #[test]
    fn js_1_clamp_test_heap_limit_bytes_never_exceeds_production_default() {
        assert_eq!(
            clamp_test_heap_limit_bytes(MAX_ISOLATE_HEAP_BYTES),
            MAX_ISOLATE_HEAP_BYTES
        );
        assert_eq!(
            clamp_test_heap_limit_bytes(MAX_ISOLATE_HEAP_BYTES + 1),
            MAX_ISOLATE_HEAP_BYTES
        );
        assert_eq!(
            clamp_test_heap_limit_bytes(usize::MAX),
            MAX_ISOLATE_HEAP_BYTES
        );
    }

    /// codex レビュー指摘 #503 P0: `0` や極端に小さい値は、実用上動作する
    /// 最小値（[`MIN_TEST_HEAP_LIMIT_BYTES`]）まで引き上げられ、無効な
    /// 値がそのまま `v8::CreateParams::heap_limits` へ渡ることはないこと。
    #[test]
    fn js_1_clamp_test_heap_limit_bytes_enforces_a_floor() {
        assert_eq!(clamp_test_heap_limit_bytes(0), MIN_TEST_HEAP_LIMIT_BYTES);
        assert_eq!(clamp_test_heap_limit_bytes(1), MIN_TEST_HEAP_LIMIT_BYTES);
        assert_eq!(
            clamp_test_heap_limit_bytes(MIN_TEST_HEAP_LIMIT_BYTES),
            MIN_TEST_HEAP_LIMIT_BYTES
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
    /// `JsEngineError::BindingFailed`（[`IsolateAlreadyActiveOnThread`] の
    /// メッセージを含む）を返すこと。`OwnedIsolate` の drop 順序制約に
    /// よる panic を型レベルで避けていることの検証。
    #[test]
    fn js_1_v8_engine_new_rejects_second_isolate_on_same_thread() {
        let first = V8Engine::new().expect("first V8Engine must succeed");

        match V8Engine::new() {
            Ok(_) => panic!("a second V8Engine must not be created on the same thread"),
            Err(JsEngineError::BindingFailed(msg)) => {
                assert_eq!(msg, IsolateAlreadyActiveOnThread.to_string());
            }
            Err(other) => panic!("expected BindingFailed, got: {other:?}"),
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
            Err(JsEngineError::Timeout(msg)) => {
                assert!(
                    msg.contains("timeout"),
                    "expected a timeout message, got: {msg}"
                );
            }
            other => panic!("expected Timeout for an infinite loop, got: {other:?}"),
        }
        // タイムアウト後もエンジンが使い続けられること（`terminate_execution`
        // の解除（`cancel_terminate_execution`）が効いていることの確認）。
        let result = engine
            .evaluate_script("1 + 1", &EvaluateOptions::default())
            .expect("engine must remain usable after a timeout");
        assert_eq!(result, JsValue::Number(2.0));
    }

    /// JS-1・PR #503 の codex 再指摘 P0 の回帰確認: 投げられた値の
    /// ユーザー定義 `toString` が無限ループでも、エラーメッセージの
    /// 取り出し処理がハングせず、[`SCRIPT_EXECUTION_TIMEOUT`] 相当の
    /// 時間で `Err` を返すこと。所要時間を具体値で確認し（タイムアウト
    /// の 2 倍を上限とする）、ハングしていないことを直接示す。
    ///
    /// 実装済みを装わない（REPAIR-3）: 手元検証では、既定の
    /// `capture_message`（`v8::TryCatch` のドキュメントコメント参照）に
    /// より `tc.message()` が常に `Some` を返すため、この入力は修正前の
    /// コードでも（`tc.exception().to_string()` の分岐に到達せず）実際
    /// にはハングしなかった。したがって本テストは「修正で実際に直った
    /// ハングの再現」ではなく、「この入力に対してハングしないこと」の
    /// 回帰確認である。
    #[test]
    fn js_1_v8_thrown_object_with_hanging_tostring_does_not_hang() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let started = std::time::Instant::now();
        let result = engine.evaluate_script(
            "throw { toString() { while (true) {} } }",
            &EvaluateOptions::default(),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < SCRIPT_EXECUTION_TIMEOUT * 2,
            "evaluate_script must not hang past twice the timeout, took {elapsed:?}"
        );
        assert!(
            matches!(result, Err(JsEngineError::EvaluationFailed(_))),
            "expected EvaluationFailed for a thrown object with a hanging toString, got: {result:?}"
        );

        // 打ち切り後もエンジンが使い続けられること。
        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a hanging toString is cut off");
        assert_eq!(result, JsValue::Number(42.0));
    }

    /// JS-1・PR #503 の codex 再指摘 P0 の回帰確認: 投げられた plain
    /// object（`Error.prototype` を継承しない）の `message` プロパティの
    /// getter が無限ループでもハングしないこと。本 crate は投げられた値が
    /// オブジェクトであれば `to_string` 自体を呼ばないため、この getter は
    /// 呼ばれない。
    ///
    /// 実装済みを装わない（REPAIR-3）: 手元検証では、この入力
    /// （plain object）に対する V8 のメッセージ生成は getter を呼ばず
    /// "Uncaught #<Object>" のような一般的な文言になり、修正前のコードでも
    /// ハングは再現できなかった。`Error` インスタンス自体の `message`
    /// プロパティを getter に差し替えた入力（`var e = new Error("x");
    /// Object.defineProperty(e, "message", { get() { while (true) {} } });
    /// throw e;`）——つまり `Error.prototype.toString` が本来 `.message`
    /// を読み出す経路——でも同様に、修正前のコードで "Uncaught Error" が
    /// 返り getter は呼ばれずハングしなかった（V8 のメッセージ生成自体が
    /// 副作用なしの経路のみを使うため）。
    #[test]
    fn js_1_v8_thrown_object_with_hanging_message_getter_does_not_hang() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let started = std::time::Instant::now();
        let result = engine.evaluate_script(
            "throw { get message() { while (true) {} } }",
            &EvaluateOptions::default(),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < SCRIPT_EXECUTION_TIMEOUT * 2,
            "evaluate_script must not hang past twice the timeout, took {elapsed:?}"
        );
        assert!(
            matches!(result, Err(JsEngineError::EvaluationFailed(_))),
            "expected EvaluationFailed for a thrown object with a hanging message getter, got: {result:?}"
        );

        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a hanging getter is cut off");
        assert_eq!(result, JsValue::Number(42.0));
    }

    /// JS-1・PR #503 の codex 再指摘 P0 の回帰確認: 投げられた値の
    /// `Symbol.toPrimitive` が無限ループでもハングしないこと（ECMA-262 の
    /// ToPrimitive/ToString 抽象操作を経由しうる、もう 1 つの経路）。
    #[test]
    fn js_1_v8_thrown_object_with_hanging_to_primitive_does_not_hang() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        let started = std::time::Instant::now();
        let result = engine.evaluate_script(
            "throw { [Symbol.toPrimitive]() { while (true) {} } }",
            &EvaluateOptions::default(),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < SCRIPT_EXECUTION_TIMEOUT * 2,
            "evaluate_script must not hang past twice the timeout, took {elapsed:?}"
        );
        assert!(
            matches!(result, Err(JsEngineError::EvaluationFailed(_))),
            "expected EvaluationFailed for a thrown object with a hanging Symbol.toPrimitive, got: {result:?}"
        );

        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a hanging Symbol.toPrimitive is cut off");
        assert_eq!(result, JsValue::Number(42.0));
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

    /// JS-1・`TASK-29`・Issue #508 の回帰確認: V8 の組み込み上限
    /// （`String::kMaxLength`）を超える文字列確保は、ヒープ上限の設定に
    /// 関わらず（プロセスをまったく巻き込まずに）常に `RangeError` で
    /// `EvaluationFailed` になること。この検証はヒープサイズに依存しない
    /// （`v8::String::kMaxLength` を超える長さの要求はアプリケーション側の
    /// ヒープ上限を確認する前に拒否される。実機検証で 2 ミリ秒未満で完了
    /// することを確認済み）。
    #[test]
    fn js_1_v8_string_exceeding_max_length_returns_range_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        // `String::kMaxLength`（64-bit で 536,870,888 文字）を超える長さ
        // （2**30 = 1,073,741,824 文字）を要求する。
        match engine.evaluate_script("'x'.repeat(2**30)", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("RangeError"),
                    "expected a RangeError message, got: {msg}"
                );
            }
            other => panic!(
                "expected EvaluationFailed with a RangeError for a string exceeding the max \
                 length, got: {other:?}"
            ),
        }

        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a RangeError");
        assert_eq!(result, JsValue::Number(42.0));
    }

    /// JS-1・`TASK-29`・Issue #508 の回帰確認: 配列の最大長（`2**32 - 1`。
    /// ECMA-262 の仕様上の上限）を超える長さの指定は、ヒープ確保を試みる
    /// 前に `RangeError` になること。文字列の場合と同様、ヒープサイズに
    /// 依存しない。
    #[test]
    fn js_1_v8_array_exceeding_max_length_returns_range_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        match engine.evaluate_script("new Array(2**32)", &EvaluateOptions::default()) {
            Err(JsEngineError::EvaluationFailed(msg)) => {
                assert!(
                    msg.contains("RangeError"),
                    "expected a RangeError message, got: {msg}"
                );
            }
            other => panic!(
                "expected EvaluationFailed with a RangeError for an array exceeding the max \
                 length, got: {other:?}"
            ),
        }

        let result = engine
            .evaluate_script("40 + 2", &EvaluateOptions::default())
            .expect("engine must remain usable after a RangeError");
        assert_eq!(result, JsValue::Number(42.0));
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
            Err(JsEngineError::Timeout(msg)) => {
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
            other => {
                panic!("expected Timeout due to a pending termination request, got: {other:?}")
            }
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
            Err(JsEngineError::Timeout(msg)) => {
                assert!(
                    msg.contains("execution exceeded"),
                    "expected an execution-stage timeout message, got: {msg}"
                );
            }
            other => {
                panic!("expected Timeout due to a pending termination request, got: {other:?}")
            }
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

    /// JS-1・Issue #511: テスト専用の [`NativeCallTransport`]。台本
    /// （`script`）どおりの応答を順番に返し、実際に呼ばれた `(id, args)` を
    /// `calls` に記録する。実 stdio・実子プロセスを介さず、逆方向 RPC の
    /// 呼び出し契約（V8 の値変換・エラー分類・fatal 時の挙動）だけを検証
    /// するために使う。
    struct ScriptedTransport {
        script: std::collections::VecDeque<Result<NativeReturn, NativeCallFailure>>,
        calls: Vec<(u32, Vec<JsValue>)>,
    }

    impl NativeCallTransport for ScriptedTransport {
        fn call(&mut self, id: u32, args: &[JsValue]) -> Result<NativeReturn, NativeCallFailure> {
            self.calls.push((id, args.to_vec()));
            self.script
                .pop_front()
                .unwrap_or(Err(NativeCallFailure::Fatal(
                    "ScriptedTransport script exhausted".to_string(),
                )))
        }
    }

    /// JS-1・Issue #511: プロキシ関数を登録して呼ぶと、transport が期待した
    /// `(id, args)` を受け取り、[`NativeReturn::Ok`] が JS 側の評価結果に
    /// なること。
    #[test]
    fn js_1_native_proxy_function_calls_transport_and_returns_ok_value() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .install_native_proxy_global("hostAdd", 7)
            .expect("installing the proxy function must succeed");

        type RecordedCalls = std::rc::Rc<std::cell::RefCell<Vec<(u32, Vec<JsValue>)>>>;
        let calls: RecordedCalls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        struct RecordingTransport {
            calls: RecordedCalls,
        }
        impl NativeCallTransport for RecordingTransport {
            fn call(
                &mut self,
                id: u32,
                args: &[JsValue],
            ) -> Result<NativeReturn, NativeCallFailure> {
                self.calls.borrow_mut().push((id, args.to_vec()));
                Ok(NativeReturn::Ok(JsValue::Number(42.0)))
            }
        }
        engine.set_native_call_transport(Box::new(RecordingTransport {
            calls: calls.clone(),
        }));

        let result = engine
            .evaluate_script("hostAdd(1, 'x')", &EvaluateOptions::default())
            .expect("native call must succeed");
        assert_eq!(result, JsValue::Number(42.0));
        assert_eq!(
            *calls.borrow(),
            vec![(
                7,
                vec![JsValue::Number(1.0), JsValue::String("x".to_string())]
            )]
        );
    }

    /// JS-1・Issue #511: [`NativeReturn::Err`] は JS の例外になり、
    /// `try`/`catch` で捕捉できること。
    #[test]
    fn js_1_native_proxy_function_err_return_becomes_a_catchable_exception() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .install_native_proxy_global("hostAdd", 1)
            .expect("installing the proxy function must succeed");
        engine.set_native_call_transport(Box::new(ScriptedTransport {
            script: std::collections::VecDeque::from([Ok(NativeReturn::Err("boom".to_string()))]),
            calls: Vec::new(),
        }));

        let result = engine
            .evaluate_script(
                "try { hostAdd(); 'unreachable' } catch (e) { e.message }",
                &EvaluateOptions::default(),
            )
            .expect("evaluation itself must succeed (the exception is caught in JS)");
        assert_eq!(result, JsValue::String("boom".to_string()));
    }

    /// JS-1・Issue #511: [`NativeCallFailure::Fatal`] のとき、
    /// `terminate_execution` により評価自体が打ち切られ（JS の
    /// `try`/`catch` では捕捉できない）、[`V8Engine::take_native_call_fatal`]
    /// が `Some` になり、以後の評価は `Err` を返すこと。
    #[test]
    fn js_1_native_proxy_function_fatal_terminates_execution_and_marks_the_engine_unusable() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .install_native_proxy_global("hostAdd", 1)
            .expect("installing the proxy function must succeed");
        engine.set_native_call_transport(Box::new(ScriptedTransport {
            script: std::collections::VecDeque::from([Err(NativeCallFailure::Fatal(
                "connection lost".to_string(),
            ))]),
            calls: Vec::new(),
        }));

        // `terminate_execution` はスケジュールされるだけで、割り込みチェック
        // （ループの backedge・関数呼び出し等）を経て初めて実際の中断が
        // 効く。`hostAdd()` の直後に平文の式を置くだけでは割り込み
        // チェックポイントが無く中断が観測できない場合があるため、
        // 直後に `while` ループを置いて確実にチェックポイントを踏ませる。
        let result = engine.evaluate_script(
            "try { hostAdd(); while (true) {} } catch (e) { 'swallowed' }",
            &EvaluateOptions::default(),
        );
        assert!(
            result.is_err(),
            "a fatal native call must not be swallowed by JS try/catch, got: {result:?}"
        );

        // `take_native_call_fatal` で取り出す**前**に、fatal が記録されて
        // いる間は新しい評価を始めないこと（`evaluate_script` 冒頭の
        // fail-closed 確認）を先に確認する。
        match engine.evaluate_script("1 + 1", &EvaluateOptions::default()) {
            Err(JsEngineError::EngineUnavailable(msg)) => {
                assert!(msg.contains("context was discarded"));
            }
            other => panic!("expected EngineUnavailable after a fatal native call, got: {other:?}"),
        }

        let fatal = engine
            .take_native_call_fatal()
            .expect("fatal message must be recorded");
        assert!(fatal.contains("connection lost"));
    }

    /// JS-1・Issue #511: 表現できない引数（object）は `TypeError` になり、
    /// transport は呼ばれないこと（送信前に検証する。security.md）。
    #[test]
    fn js_1_native_proxy_function_rejects_unrepresentable_argument_without_calling_transport() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .install_native_proxy_global("hostAdd", 1)
            .expect("installing the proxy function must succeed");
        let calls = std::rc::Rc::new(std::cell::RefCell::new(0usize));
        struct CountingTransport {
            calls: std::rc::Rc<std::cell::RefCell<usize>>,
        }
        impl NativeCallTransport for CountingTransport {
            fn call(
                &mut self,
                _id: u32,
                _args: &[JsValue],
            ) -> Result<NativeReturn, NativeCallFailure> {
                *self.calls.borrow_mut() += 1;
                Ok(NativeReturn::Ok(JsValue::Undefined))
            }
        }
        engine.set_native_call_transport(Box::new(CountingTransport {
            calls: calls.clone(),
        }));

        let result = engine.evaluate_script(
            "try { hostAdd({}); 'no-throw' } catch (e) { e instanceof TypeError }",
            &EvaluateOptions::default(),
        );
        assert_eq!(
            result.expect("evaluation must succeed"),
            JsValue::Bool(true)
        );
        assert_eq!(*calls.borrow(), 0, "transport must not be called");
    }

    /// JS-1・Issue #511: 引数の個数が上限を超える場合は `RangeError` になり、
    /// transport は呼ばれないこと。
    #[test]
    fn js_1_native_proxy_function_rejects_too_many_arguments_without_calling_transport() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .install_native_proxy_global("hostAdd", 1)
            .expect("installing the proxy function must succeed");
        engine.set_native_call_transport(Box::new(ScriptedTransport {
            script: std::collections::VecDeque::new(),
            calls: Vec::new(),
        }));

        let too_many_args = "0,".repeat(worker_protocol::MAX_NATIVE_CALL_ARGS + 1);
        let script = format!(
            "try {{ hostAdd({too_many_args}0); 'no-throw' }} catch (e) {{ e instanceof RangeError }}"
        );
        let result = engine.evaluate_script(&script, &EvaluateOptions::default());
        assert_eq!(
            result.expect("evaluation must succeed"),
            JsValue::Bool(true)
        );
    }

    /// JS-1・Issue #511・codex レビュー指摘 #533 P0: 引数の**件数**は
    /// [`worker_protocol::MAX_NATIVE_CALL_ARGS`] 以内でも、合計エンコード後
    /// サイズが [`worker_protocol::MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`] を
    /// 超える場合は `RangeError` になり、transport は呼ばれないこと。
    ///
    /// 各文字列は [`MAX_RESULT_STRING_UTF16_UNITS`]（1M UTF-16 単位）以内
    /// だが、4 引数分（各 900,000 バイトの ASCII 文字列）の合計は
    /// `MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`（3 MiB + 64 KiB）を超える。
    /// `transport` が一度も呼ばれないことを確認することで、合計サイズの
    /// 検証が `value_to_js_value`（V8 文字列の複製）より前に効いている
    /// ことを間接的に確認する（複製後にしか検証していなければ、この
    /// テストサイズでは複製自体は成功してしまい、区別できなくなる）。
    #[test]
    fn js_1_native_proxy_function_rejects_oversized_argument_payload_without_calling_transport() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .install_native_proxy_global("hostAdd", 1)
            .expect("installing the proxy function must succeed");
        let calls = std::rc::Rc::new(std::cell::RefCell::new(0usize));
        struct CountingTransport {
            calls: std::rc::Rc<std::cell::RefCell<usize>>,
        }
        impl NativeCallTransport for CountingTransport {
            fn call(
                &mut self,
                _id: u32,
                _args: &[JsValue],
            ) -> Result<NativeReturn, NativeCallFailure> {
                *self.calls.borrow_mut() += 1;
                Ok(NativeReturn::Ok(JsValue::Undefined))
            }
        }
        engine.set_native_call_transport(Box::new(CountingTransport {
            calls: calls.clone(),
        }));

        let result = engine.evaluate_script(
            "try { \
                 const s = 'a'.repeat(900000); \
                 hostAdd(s, s, s, s); \
                 'no-throw' \
             } catch (e) { e instanceof RangeError }",
            &EvaluateOptions::default(),
        );
        assert_eq!(
            result.expect("evaluation must succeed"),
            JsValue::Bool(true)
        );
        assert_eq!(*calls.borrow(), 0, "transport must not be called");
    }

    /// JS-1・Issue #511: transport が未設定のときは `Error` が投げられる
    /// こと。
    #[test]
    fn js_1_native_proxy_function_without_transport_throws_an_error() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        engine
            .install_native_proxy_global("hostAdd", 1)
            .expect("installing the proxy function must succeed");

        let result = engine.evaluate_script(
            "try { hostAdd(); 'no-throw' } catch (e) { e instanceof Error }",
            &EvaluateOptions::default(),
        );
        assert_eq!(
            result.expect("evaluation must succeed"),
            JsValue::Bool(true)
        );
    }

    /// JS-1・Issue #511: 関数名の検証（空文字列・上限超過）が
    /// `BindingFailed` を返すこと。
    #[test]
    fn js_1_install_native_proxy_global_rejects_invalid_names() {
        let mut engine = V8Engine::new().expect("no other V8Engine is active on this thread");
        assert!(matches!(
            engine.install_native_proxy_global("", 1),
            Err(JsEngineError::BindingFailed(_))
        ));
        let too_long = "a".repeat(MAX_NATIVE_PROXY_NAME_BYTES + 1);
        assert!(matches!(
            engine.install_native_proxy_global(&too_long, 1),
            Err(JsEngineError::BindingFailed(_))
        ));
    }
}
