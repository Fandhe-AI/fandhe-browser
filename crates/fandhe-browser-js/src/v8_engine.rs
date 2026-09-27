//! V8（`rusty_v8` = crates.io の `v8` crate `=152.2.0`）の Platform 初期化と
//! Isolate 生成を受け持つ、feature `js-v8` 配下の非公開モジュール
//! （`JS-1`・TASK-29（29.2）・MS-3・Issue #153）。
//!
//! 呼び出し元（将来）: [`super::engine_trait::create_engine`] の
//! `EngineKind::V8` 分岐（TASK-29.3/29.6 で配線する）。上位 crate（core・
//! TASK-30）は [`super::engine_trait::JsEngine`] トレイト越しにだけ使い、
//! 本モジュールの型を直接見ない（`pub` を付けず crate 内に閉じる。AC-2）。
//!
//! # スタブについて
//!
//! 本モジュールは Platform/Isolate の初期化のみを担う。以下は未実装
//! （実装済みを装わない。REPAIR-3）。
//!
//! - [`super::engine_trait::JsEngine`] の実装（`TASK-29.3`〜`29.5`）
//! - `v8` 由来のエラーを共通エラー型へ変換する仕組み（`TASK-29.6`）
//! - ヒープ上限（`near_heap_limit_callback`）・実行タイムアウトの導入
//!   （[`super::engine_trait::EvaluateOptions`] の拡張と合わせて
//!   `TASK-29.3` 以降 / `TASK-30`（`MS-3`）で扱う）
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
/// [`super::engine_trait::JsEngine`] の実装は `TASK-29.3`〜`29.5` で追加する
/// ため、本 Issue の時点では lib 本体から生成されない
/// （`create_engine` 未配線。§スコープ境界）。テストからのみ使われるので、
/// `cfg(not(test))` の場合に `dead_code` の期待を宣言する。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "TASK-29.3/29.6 で create_engine から配線されるまで lib 本体からは未使用（REPAIR-3）"
    )
)]
pub(crate) struct V8Engine {
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
        reason = "TASK-29.3/29.6 で create_engine から配線されるまで lib 本体からは未使用（REPAIR-3）"
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
        let isolate = v8::Isolate::new(v8::CreateParams::default());
        Ok(Self { isolate })
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
}
