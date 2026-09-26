//! compat_tasks: CORE-1（代表タスク成功率 70% 以上）を測定するための CLI。
//!
//! `local` サブコマンドはリポジトリ内蔵のフィクスチャ（`tasks::LOCAL_TASKS`）
//! を実行し、決定的な結果を出力する（ネットワーク不要。CI 対象。
//! `tests/compat_fixtures.rs` が同じロジックを結合テストとして検証する）。
//! `real` サブコマンドは実サイトへ到達して非空判定のみ行う（手動実行専用。
//! ネットワーク依存で決定性が無いため `cargo test`・CI からは実行しない）。
//!
//! TASK-26（26.1）・Issue #136・MS-3 の道具立て。測定結果を
//! `docs/design/core1-success-rate.md` へレポートする作業は TASK-26.2
//! （Issue #137）が本 CLI の出力を用いて行う（本 Issue の範囲外）。
//!
//! 実行方法は `harness/compat_fixtures/README.md` を参照。

#[path = "tasks.rs"]
mod tasks;

use std::process::ExitCode;

use fandhe_browser_core::{Error, FetchOptions, Fetcher, ParseOptions, parse_document_bytes};
use tasks::{
    Category, Expected, Status, Summary, TARGET_RATE, TaskError, TaskKind, TaskResult, judge,
    run_task, sample_of, summarize,
};

/// 実サイトモード（`real`）専用のタスク定義。ネットワーク先の URL は定数
/// でのみ与え、CLI 引数からは任意の URL を受け付けない
/// （security.md「SSRF」対策）。
struct RemoteTask {
    id: &'static str,
    category: Category,
    url: &'static str,
    kind: TaskKind,
    selector: &'static str,
}

/// 実サイトのタスク表（`harness/compat_fixtures/README.md`「実サイトのタスク表を
/// 差し替える場合」参照）。到達性は手動実行時に確認する。
const REMOTE_TASKS: &[RemoteTask] = &[
    RemoteTask {
        id: "R-S01",
        category: Category::Static,
        url: "https://example.com/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-S02",
        category: Category::Static,
        url: "https://en.wikipedia.org/wiki/Rust_(programming_language)",
        kind: TaskKind::Texts,
        selector: "h1#firstHeading",
    },
    RemoteTask {
        id: "R-S03",
        category: Category::Static,
        url: "https://developer.mozilla.org/en-US/docs/Web/JavaScript",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-S04",
        category: Category::Static,
        url: "https://doc.rust-lang.org/book/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-S05",
        category: Category::Static,
        url: "https://docs.python.org/3/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-S06",
        category: Category::Static,
        url: "https://news.ycombinator.com/",
        kind: TaskKind::Attr("href"),
        selector: ".titleline > a",
    },
    RemoteTask {
        id: "R-S07",
        category: Category::Static,
        url: "https://the-internet.herokuapp.com/login",
        kind: TaskKind::Form,
        selector: "form#login",
    },
    RemoteTask {
        id: "R-S08",
        category: Category::Static,
        url: "https://datatables.net/examples/basic_init/zero_configuration.html",
        kind: TaskKind::Texts,
        selector: "#example tbody tr",
    },
    RemoteTask {
        id: "R-P01",
        category: Category::SsrSpaStatic,
        url: "https://react.dev/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-P02",
        category: Category::SsrSpaStatic,
        url: "https://vuejs.org/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-P03",
        category: Category::SsrSpaStatic,
        url: "https://svelte.dev/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-P04",
        category: Category::SsrSpaStatic,
        url: "https://nextjs.org/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
    RemoteTask {
        id: "R-P05",
        category: Category::SsrSpaStatic,
        url: "https://nuxt.com/",
        kind: TaskKind::Texts,
        selector: "h1",
    },
];

fn print_usage() {
    eprintln!("usage: compat_tasks <local|real>");
    eprintln!("  local: run against bundled local fixtures (no network, deterministic)");
    eprintln!("  real:  run against real sites (manual use only, requires network)");
}

fn print_task_result(result: &TaskResult) {
    println!(
        "{}\t{}\t{:?}\t{}\t{}",
        result.id,
        result.category.label(),
        result.kind,
        result.status.label(),
        result.sample
    );
}

fn print_summary_line(label: &str, summary: Summary) {
    let met = if summary.attempted == 0 {
        "no-tasks"
    } else if summary.rate() >= TARGET_RATE {
        "met"
    } else {
        "not-met"
    };
    println!(
        "{label}\t{}/{}\t{}/{}\trate={:.1}%\ttarget={:.0}%\t{met}",
        summary.success,
        summary.attempted,
        summary.success,
        summary.reachable,
        summary.rate() * 100.0,
        TARGET_RATE * 100.0,
    );
}

fn run_local_mode() -> ExitCode {
    let results = tasks::run_local();
    for result in &results {
        print_task_result(result);
    }
    let (static_summary, ssr_spa_summary) = summarize(&results);
    print_summary_line("static", static_summary);
    print_summary_line("ssr_spa_static", ssr_spa_summary);

    let excluded = results
        .iter()
        .filter(|r| r.category == Category::Excluded)
        .count();
    println!("excluded\t{excluded} task(s) not counted toward success rate");

    let met = |s: Summary| s.attempted == 0 || s.rate() >= TARGET_RATE;
    if met(static_summary) && met(ssr_spa_summary) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[tokio::main(flavor = "current_thread")]
async fn run_real_mode() -> ExitCode {
    // 既定の `FetchOptions`（内部アドレス拒否・本文上限・タイムアウト・
    // リダイレクト上限は既定値のまま）を使う。User-Agent も core 既定の
    // ままで、UA 偽装・anti-bot 回避は行わない（security.md「偽装機能禁止」）。
    let fetcher = match Fetcher::new(FetchOptions::default()) {
        Ok(fetcher) => fetcher,
        Err(err) => {
            eprintln!("failed to build fetcher: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut results = Vec::new();
    for task in REMOTE_TASKS {
        let status_and_sample = fetch_and_run(&fetcher, task).await;
        results.push(TaskResult {
            id: task.id,
            category: task.category,
            kind: task.kind,
            status: status_and_sample.0,
            sample: status_and_sample.1,
        });
    }

    for result in &results {
        print_task_result(result);
    }
    let (static_summary, ssr_spa_summary) = summarize(&results);
    print_summary_line("static", static_summary);
    print_summary_line("ssr_spa_static", ssr_spa_summary);
    // 実サイトモードは参考値の記録用であり、終了コードで CI を左右しない
    // （ネットワーク依存で非決定的なため）。常に成功で終える。
    ExitCode::SUCCESS
}

/// `fetcher.get` が返す [`Error`] を、出力用の短い種別名へ写像する
/// （詳細メッセージ自体は埋め込まない。security.md 秘密情報混入防止・
/// 出力肥大化の回避。26.2 が原因切り分けをしやすいよう種別だけは残す）。
fn fetch_error_label(err: &Error) -> &'static str {
    match err {
        Error::Timeout { .. } => "timeout",
        Error::TooManyRedirects { .. } => "too-many-redirects",
        Error::ResponseTooLarge { .. } => "response-too-large",
        Error::DisallowedScheme { .. } => "disallowed-scheme",
        Error::DisallowedAddress { .. } => "disallowed-address",
        Error::TooManyConcurrentDnsResolutions { .. } => "too-many-dns-resolutions",
        Error::Network { .. } => "network",
        _ => "other",
    }
}

async fn fetch_and_run(fetcher: &Fetcher, task: &RemoteTask) -> (Status, String) {
    let response = match fetcher.get(task.url).await {
        Ok(response) => response,
        Err(err) => return (Status::FetchError(fetch_error_label(&err)), String::new()),
    };
    if !(200..300).contains(&response.status()) {
        return (Status::Http(response.status()), String::new());
    }
    let parsed = match parse_document_bytes(response.body(), &ParseOptions::default()) {
        Ok(parsed) => parsed,
        Err(_) => return (Status::ParseError, String::new()),
    };
    match run_task(&parsed.document, task.kind, task.selector) {
        Ok(outcome) => {
            let sample = sample_of(&outcome);
            (judge(&outcome, Expected::NonEmpty), sample)
        }
        Err(TaskError::SelectorUnsupported) => (Status::SelectorUnsupported, String::new()),
        Err(TaskError::Core) => (Status::QueryError, String::new()),
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("local") => run_local_mode(),
        Some("real") => run_real_mode(),
        _ => {
            print_usage();
            ExitCode::from(2)
        }
    }
}
