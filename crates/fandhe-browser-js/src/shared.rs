//! V8 子プロセス（`v8_engine`）と boa 子プロセス（`boa_worker`）が共有する
//! 定数・逆方向 RPC の窓口（`JS-1`・`TASK-29`・`TASK-32.2`・Issue #166）。
//!
//! 呼び出し元: [`super::worker`]（子側のフレームループ）・
//! [`super::process_engine`]（親側の期限計算）・[`super::v8_engine`]・
//! [`super::boa_worker`]。エンジン固有の型（`v8`・`boa_engine`）には依存しない。

use std::time::Duration;

use super::engine_trait::JsValue;
use super::worker_protocol::NativeReturn;

/// 評価できるスクリプトの最大バイト数（`JS-1`・OWASP A04「不安全な設計」。
/// security.md）。V8 版・boa 版・親側の事前検査で同じ値を使う。
pub(crate) const MAX_SCRIPT_SOURCE_BYTES: usize = 1_048_576; // 1 MiB

/// 1 回の評価に許容する実時間の基準値（`JS-1`・`TASK-29`）。
///
/// V8 は子プロセス内の watchdog がこの時間で `terminate_execution` する。
/// boa には中断 API が無いため、親（[`super::process_engine`]）が
/// この値に猶予を足した期限で子プロセスごと kill する（`Timeout`）。
pub(crate) const SCRIPT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(2);

/// 子プロセス側から親プロセスへ逆方向 RPC（`NativeCall`）を送り、
/// [`NativeReturn`] が届くまで**同期的にブロックする**窓口（`JS-1`・
/// `TASK-29`・Issue #511）。
///
/// 呼び出し元: V8 のプロキシ関数コールバック（`v8_engine`）と boa の
/// プロキシ関数（`boa_worker`）。実装は [`super::worker`] の
/// `StdioTransport`（stdio 越しに親と一問一答する）を想定するが、呼び出し側は
/// その具象型に依存しない（テストでは台本どおりに応答する偽の実装を使う）。
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
    /// このプロセス自体を終了させる）。
    Fatal(String),
}
