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
//! Isolate は生成したスレッドの中だけで使う。同じスレッドで複数の
//! `OwnedIsolate` を持つ場合は、生成と逆の順序で drop しなければならない
//! （v8 152.2.0 の `OwnedIsolate::Drop` が `assert!` で検査する）。
//! そのため本モジュールは 1 つの `V8Engine` が `OwnedIsolate` を
//! ちょうど 1 つだけ持つ構造に留める。

use std::sync::Once;

/// プロセス内で V8 の Platform 初期化を 1 回だけ実行するためのフラグ。
static V8_INIT: Once = Once::new();

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
    /// v8 の実行コンテキストを保持する Isolate 本体。ちょうど 1 つだけ
    /// 持つことで、`OwnedIsolate` の drop 順制約（生成と逆順であること）
    /// を型の構造だけで単純に満たす。
    isolate: v8::OwnedIsolate,
}

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
    pub(crate) fn new() -> Self {
        ensure_v8_initialized();
        let isolate = v8::Isolate::new(v8::CreateParams::default());
        Self { isolate }
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
            let mut engine = V8Engine::new();
            let heap_size = engine.total_heap_size();
            assert!(
                heap_size > 0,
                "newly created isolate must report a nonzero heap size"
            );
            drop(engine);
        }
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
                    let mut engine = V8Engine::new();
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
