//! ブロッキングする処理を専用のスレッドで動かし、async のハンドラから待つ（TASK-127）。
//!
//! 操作モードの読み込み（最大 60 秒）とホストの DOM の取得（最大 35 秒）は同期の呼び出しで、cli のサーバーは
//! current_thread のランタイムで動く。ハンドラの中で直接呼ぶと、その間サーバー全体（CDP を含む）が止まる。
//! そこで `/ai/navigate`・`/ai/snapshot` は [`run_blocking`] でスレッドへ移し、完了を future で待つ。
//! ランタイムの依存（`tokio::task::spawn_blocking` 等）を ai に足さないため、標準ライブラリだけで作る。
//!
//! # 上限（`ponytail:`）
//!
//! 要求ごとにスレッドを 1 つ作る。同時の要求数は描画のロックで直列になり、待つスレッドが増えるだけで、
//! 上限はサーバーの同時接続数に従う。スレッドプールにするのは、同時接続が増えて問題になってから。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

struct State<T> {
    value: Option<T>,
    /// スレッドが終わった（値を置いた、または panic した）。
    finished: bool,
    waker: Option<Waker>,
}

type Shared<T> = Arc<Mutex<State<T>>>;

/// スレッドの終わりに必ず `finished` を立てて起こす（panic でも待ち手を残さない）。
struct Finish<T>(Shared<T>);

impl<T> Drop for Finish<T> {
    fn drop(&mut self) {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        s.finished = true;
        if let Some(w) = s.waker.take() {
            w.wake();
        }
    }
}

/// [`run_blocking`] の完了を待つ future。スレッドを作れなかった・処理が panic した場合は `None`。
pub(crate) struct Blocking<T> {
    shared: Shared<T>,
}

impl<T> Future for Blocking<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(v) = s.value.take() {
            return Poll::Ready(Some(v));
        }
        if s.finished {
            return Poll::Ready(None);
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// `f` を新しいスレッドで動かし、結果を返す future を返す。
pub(crate) fn run_blocking<T, F>(f: F) -> Blocking<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let shared: Shared<T> = Arc::new(Mutex::new(State {
        value: None,
        finished: false,
        waker: None,
    }));
    let inner = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("fandhe-ai-blocking".to_owned())
        .spawn(move || {
            let finish = Finish(Arc::clone(&inner));
            let v = f();
            inner.lock().unwrap_or_else(PoisonError::into_inner).value = Some(v);
            drop(finish);
        });
    if spawned.is_err() {
        shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finished = true;
    }
    Blocking { shared }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Wake;

    struct Flag(std::sync::atomic::AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn block_on<T>(mut f: Blocking<T>) -> Option<T> {
        let flag = Arc::new(Flag(Default::default()));
        let waker = Waker::from(Arc::clone(&flag));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = Pin::new(&mut f).poll(&mut cx) {
                return v;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// 結果を返す。処理が別スレッドで動く（呼び出し元のスレッドを止めない）。
    #[test]
    fn runs_on_another_thread_and_returns_value() {
        let caller = std::thread::current().id();
        let got = block_on(run_blocking(move || std::thread::current().id() != caller));
        assert_eq!(got, Some(true));
    }

    /// 処理が panic しても待ち手は `None` で終わる（待ち続けない）。
    #[test]
    fn panic_resolves_to_none() {
        let got: Option<u8> = block_on(run_blocking(|| panic!("boom")));
        assert_eq!(got, None);
    }
}
