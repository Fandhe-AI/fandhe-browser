//! boa を JS ワーカー子プロセスの評価エンジンとして使うアダプタ
//! （MS-3・TASK-32（32.2）・ビヘイビア `JS-1`・Issue #166）。
//!
//! 呼び出し元: [`super::worker`] の `create_boa_child_engine` が、子プロセスの
//! 中で [`BoaChildEngine::new`] を呼び、`Box<dyn ChildEngine>` としてフレーム
//! ループへ渡す。親（[`super::process_engine::V8ProcessEngine`] を
//! `EngineKind::Boa` で生成したもの）が子を kill・破棄できるため、boa 自体に
//! 中断 API・ヒープ上限 API が無くても、次の上限をプロセス境界で強制できる
//! （AGENTS.md「リソース上限」P0。codex レビュー指摘 #656）。
//!
//! - **実時間**: 親が評価開始時に決めた期限（`process_engine` の
//!   `EVALUATE_RECV_TIMEOUT`）を過ぎると子プロセスごと `kill` し、
//!   [`crate::JsEngineError::Timeout`] を返す。入れ子ループ・重い組込み関数でも
//!   子が占有するのは子の CPU だけで、ホストの呼び出しスレッドは期限で戻る
//! - **メモリ**: 起動直後に [`super::resource_limits::enforce_child_memory_limit`]
//!   が子へ OS 側の上限（Linux `RLIMIT_DATA`・Windows Job Object）を掛け、
//!   親の RSS 監視（`MemoryMonitor`）が超過を検出して `kill` する。巨大確保は
//!   子の異常終了となり、ホストは [`crate::JsEngineError::ResourceLimitExceeded`]
//!   として受け取る。OS ごとの強制の強さは `resource_limits` のドキュメント参照
//!
//! - **バッファ確保（boa 内）**: `ArrayBuffer`・`SharedArrayBuffer` の 1 回の確保は
//!   boa のホストフック（`boa_engine::BoundedBufferHooks`）で 128 MiB（V8 版の
//!   `HEAP_EXTERNAL_ALLOWANCE_BYTES` と同値）に制限し、超過は catch 可能な
//!   `RangeError` になる
//!
//! **macOS では boa を無効にしている（fail-closed。実装済みを装わない。REPAIR-3）**:
//! macOS には確保時に効く OS のプロセス単位メモリ上限が無く（`resource_limits`
//! の macOS 行参照）、boa にはヒープ上限 API も無いため、親の事後 RSS 監視だけでは
//! 強制にならない（AGENTS.md「リソース上限」P0）。`create_engine(EngineKind::Boa)`
//! は `NotYetImplemented` を返し、子プロセス側も起動を拒否する。確保時に効く
//! 上限を macOS で掛けられるようになった時点で解除する（`JS-1`）。
//!
//! **合計確保量**: boa のホストフックは 1 回の確保の上限しか決められないため、
//! 128 MiB 未満のバッファを積み上げた合計は、Linux では boa 専用の `RLIMIT_DATA`
//! （1 GiB。`resource_limits::tighten_child_memory_limit_for_boa`）、Windows では
//! Job Object のコミット上限（確保時に効く）と、各 OS共通の親の RSS 監視で抑える
//!
//! boa の具象型（`Context`・`JsValue`）は [`super::boa_engine`] の中に閉じ、
//! 本モジュールは `JsEngine` トレイト越しにしか boa を扱わない。
//!
//! # 逆方向 RPC
//!
//! 親が登録するホスト関数は、子では「`NativeCall` フレームを親へ送って
//! `NativeReturn` を待つ」プロキシ関数として登録する（V8 版と同じプロトコル）。
//! プロキシは [`NativeFn`]（`Send`）として boa エンジンへ渡すため、transport は
//! `Arc<Mutex<_>>` で共有する。fatal（プロトコル違反・EOF・I/O エラー）が
//! 起きたら状態へ記録し、以降のプロキシ呼び出しは transport に触れず失敗させる。
//! 評価の後に [`BoaChildEngine::take_native_call_fatal`] を確認した子は、応答を
//! 送らずにプロセスを終了する（`worker::evaluate_and_respond`）。
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! - 読み取り専用プロパティ（`DomLikeMemberKind::Property`）の bind は未対応で、
//!   `BindingFailed` を返す（メソッドのみ。V8 版との差。将来仕様は `JS-1`
//!   「DOM 風オブジェクト」の getter 対応）
//! - 逆方向 RPC の `Rejected`（件数・サイズ超過）は V8 版では `RangeError`、boa 版
//!   では通常の `Error` として JS へ投げる（`NativeFn` の `Err` 経路に畳むため）
//! - fatal 後も、すでに走っている JS は `try/catch` で続行できる。ただし以降の
//!   プロキシ呼び出しは失敗し、評価終了後に子は終了する（親は EOF として検出し
//!   `EngineUnavailable` へ変換する）。続行中の無限ループは親の期限 kill で止まる

use std::sync::{Arc, Mutex};

use super::boa_engine::BoaEngine;
use super::engine_trait::{EvaluateOptions, JsEngine, JsEngineError, JsValue, NativeFn};
use super::shared::{NativeCallFailure, NativeCallTransport};
use super::worker::ChildEngine;
use super::worker_protocol::{self, DomLikeMemberKind, DomLikeObjectBinding, NativeReturn};

/// プロキシ関数が共有する逆方向 RPC の状態。
struct ProxyState {
    transport: Box<dyn NativeCallTransport + Send>,
    /// fatal を検出した際のメッセージ。`Some` の間はプロキシが transport に
    /// 触れず失敗する。
    fatal: Option<String>,
}

/// boa を [`ChildEngine`] として使うアダプタ。
pub(crate) struct BoaChildEngine {
    engine: BoaEngine,
    state: Arc<Mutex<ProxyState>>,
}

impl BoaChildEngine {
    /// `transport`（本番では stdio 越しの [`super::worker::StdioTransport`]）を
    /// 配線した boa エンジンを作る。boa の `Context` は最初の評価・登録まで
    /// 作らない（[`BoaEngine::new`] の契約）。
    pub(crate) fn new(transport: Box<dyn NativeCallTransport + Send>) -> Self {
        Self {
            engine: BoaEngine::new(),
            state: Arc::new(Mutex::new(ProxyState {
                transport,
                fatal: None,
            })),
        }
    }

    /// 親側関数 `id` へ転送する [`NativeFn`] を作る。
    fn proxy_fn(&self, id: u32) -> NativeFn {
        let state = Arc::clone(&self.state);
        Box::new(move |args: &[JsValue]| {
            let mut guard = state.lock().map_err(|_| {
                JsEngineError::EvaluationFailed("native call state is poisoned".to_string())
            })?;
            if guard.fatal.is_some() {
                return Err(JsEngineError::EvaluationFailed(
                    "native call transport has failed".to_string(),
                ));
            }
            match guard.transport.call(id, args) {
                Ok(NativeReturn::Ok(value)) => Ok(value),
                Ok(NativeReturn::Err(message)) => Err(JsEngineError::EvaluationFailed(message)),
                Err(NativeCallFailure::Rejected(message)) => {
                    Err(JsEngineError::EvaluationFailed(message))
                }
                Err(NativeCallFailure::Fatal(message)) => {
                    guard.fatal = Some(message.clone());
                    Err(JsEngineError::EvaluationFailed(message))
                }
            }
        })
    }
}

impl ChildEngine for BoaChildEngine {
    fn evaluate_script(
        &mut self,
        script: &str,
        options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        JsEngine::evaluate_script(&mut self.engine, script, options)
    }

    fn take_native_call_fatal(&mut self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|mut guard| guard.fatal.take())
    }

    fn install_native_proxy_global(&mut self, name: &str, id: u32) -> Result<(), JsEngineError> {
        let proxy = self.proxy_fn(id);
        self.engine.inject_global_function(name, proxy)
    }

    fn install_dom_like_object(
        &mut self,
        binding: &DomLikeObjectBinding,
    ) -> Result<(), JsEngineError> {
        if binding.members.len() > worker_protocol::MAX_DOM_LIKE_OBJECT_MEMBERS {
            return Err(JsEngineError::BindingFailed(format!(
                "DOM-like object has more than {} members",
                worker_protocol::MAX_DOM_LIKE_OBJECT_MEMBERS
            )));
        }
        let mut methods = Vec::with_capacity(binding.members.len());
        for member in &binding.members {
            match member.kind {
                DomLikeMemberKind::Method => {
                    methods.push((member.name.clone(), self.proxy_fn(member.native_call_id)));
                }
                DomLikeMemberKind::Property => {
                    return Err(JsEngineError::BindingFailed(format!(
                        "read-only property '{}' is not supported by the boa engine",
                        member.name
                    )));
                }
            }
        }
        self.engine.bind_dom_like_object(&binding.name, methods)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_protocol::DomLikeMember;
    use std::collections::VecDeque;

    type CallLog = Arc<Mutex<Vec<(u32, Vec<JsValue>)>>>;

    /// 台本どおりに応答する偽 transport。受け取った呼び出しを記録する。
    struct Scripted {
        calls: CallLog,
        replies: VecDeque<Result<NativeReturn, NativeCallFailure>>,
    }

    impl NativeCallTransport for Scripted {
        fn call(&mut self, id: u32, args: &[JsValue]) -> Result<NativeReturn, NativeCallFailure> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push((id, args.to_vec()));
            }
            self.replies
                .pop_front()
                .unwrap_or_else(|| Err(NativeCallFailure::Fatal("script exhausted".to_string())))
        }
    }

    fn engine_with(
        replies: Vec<Result<NativeReturn, NativeCallFailure>>,
    ) -> (BoaChildEngine, CallLog) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = Scripted {
            calls: Arc::clone(&calls),
            replies: replies.into(),
        };
        (BoaChildEngine::new(Box::new(transport)), calls)
    }

    /// JS-1・TASK-32.2: プロキシ関数の呼び出しが `NativeCall`（id・引数）として
    /// transport へ届き、`NativeReturn::Ok` の値が JS の戻り値になること。
    #[test]
    fn js_1_boa_proxy_forwards_the_call_to_the_transport() {
        let (mut engine, calls) = engine_with(vec![Ok(NativeReturn::Ok(JsValue::Number(42.0)))]);
        engine
            .install_native_proxy_global("hostAdd", 7)
            .expect("install must succeed");
        let value = engine
            .evaluate_script("hostAdd('x', 1)", &EvaluateOptions::default())
            .expect("evaluation must succeed");
        assert_eq!(value, JsValue::Number(42.0));
        assert_eq!(
            *calls.lock().expect("lock"),
            vec![(
                7,
                vec![JsValue::String("x".to_string()), JsValue::Number(1.0)]
            )]
        );
        assert_eq!(engine.take_native_call_fatal(), None);
    }

    /// JS-1・TASK-32.2: `NativeReturn::Err` は JS の例外（`catch` 可能）になること。
    #[test]
    fn js_1_boa_proxy_turns_a_native_error_into_a_catchable_exception() {
        let (mut engine, _calls) =
            engine_with(vec![Ok(NativeReturn::Err("host said no".to_string()))]);
        engine
            .install_native_proxy_global("hostFail", 1)
            .expect("install must succeed");
        let value = engine
            .evaluate_script(
                "try { hostFail(); 'unreachable' } catch (e) { e.message }",
                &EvaluateOptions::default(),
            )
            .expect("evaluation must succeed");
        match value {
            JsValue::String(message) => assert!(
                message == "host said no",
                "the host error must reach JS, got {message:?}"
            ),
            other => panic!("unexpected value {other:?}"),
        }
    }

    /// JS-1・TASK-32.2: fatal を記録し、以降のプロキシ呼び出しは transport に
    /// 触れず失敗すること（台本は 1 件だけなので、2 回目に触れれば呼び出し記録が
    /// 2 件になる）。
    #[test]
    fn js_1_boa_proxy_records_fatal_and_stops_touching_the_transport() {
        let (mut engine, calls) = engine_with(vec![Err(NativeCallFailure::Fatal(
            "parent closed".to_string(),
        ))]);
        engine
            .install_native_proxy_global("hostCall", 3)
            .expect("install must succeed");
        let _ = engine.evaluate_script(
            "try { hostCall() } catch (e) {} try { hostCall() } catch (e) {} 0",
            &EvaluateOptions::default(),
        );
        assert_eq!(calls.lock().expect("lock").len(), 1);
        assert_eq!(
            engine.take_native_call_fatal(),
            Some("parent closed".to_string())
        );
        assert_eq!(engine.take_native_call_fatal(), None);
    }

    /// JS-1・TASK-32.2: DOM 風オブジェクトのメソッドが `native_call_id` で
    /// 転送され、Property は `BindingFailed`（未対応を装わない）になること。
    #[test]
    fn js_1_boa_dom_like_object_binds_methods_and_rejects_properties() {
        let (mut engine, calls) = engine_with(vec![Ok(NativeReturn::Ok(JsValue::Bool(true)))]);
        let binding = DomLikeObjectBinding {
            handle: crate::ObjectHandle::from_raw(1),
            name: "dom".to_string(),
            members: vec![DomLikeMember {
                kind: DomLikeMemberKind::Method,
                name: "setText".to_string(),
                native_call_id: 9,
            }],
        };
        engine
            .install_dom_like_object(&binding)
            .expect("method binding must succeed");
        let value = engine
            .evaluate_script("dom.setText('hi')", &EvaluateOptions::default())
            .expect("evaluation must succeed");
        assert_eq!(value, JsValue::Bool(true));
        assert_eq!(calls.lock().expect("lock")[0].0, 9);

        let with_property = DomLikeObjectBinding {
            handle: crate::ObjectHandle::from_raw(2),
            name: "dom2".to_string(),
            members: vec![DomLikeMember {
                kind: DomLikeMemberKind::Property,
                name: "title".to_string(),
                native_call_id: 10,
            }],
        };
        assert!(matches!(
            engine.install_dom_like_object(&with_property),
            Err(JsEngineError::BindingFailed(_))
        ));
    }
}
