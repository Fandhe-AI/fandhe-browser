# 操作モードの試作結果（Servo ホスト側のページでのクリック・文字入力・全ページ撮影）

対応 `TASK-117` / `MS-10` / ビヘイビア `RENDER-10` / `RENDER-11` / `SEC-5`（[spec-reference](../../.claude/rules/spec-reference.md)）。

- 実施日: 2026-11-16〜17（試作）、2026-11-18（現行 main `fd341de` で再実行）
- 環境: macOS（arm64）・Servo `=0.7.0`・隔離なしの同一プロセス（`servo-host` の一時テスト）。試験ページはローカルの `TcpListener` から出し、外部へは出ない
- 状態: 途中経過の記録。合格条件（★）3 件のうち ★1 は満たす、★2 は条件付きで読み替えて満たす（2026-11-18 のオーナー回答。条件は TASK-120 の完了条件）、★3 は通知が届くことをソースで確認し、`doc_seq` の起点は通知駆動に決定（2026-11-18。実装は TASK-121 以降）。★2 の条件は TASK-120（#1581）で実装した（後述）。項目 4・6〜11 の一部は未確認（後述「残り」）。操作モードはまだ公開しない
- 担当: Claude Code（試作・記録）。★ の合否と操作モードの公開可否はオーナー判断

## 目的と範囲

操作モード（ページを Servo ホスト側で保ち、クリック・文字入力をそこで行う方式。spec `rendering-layer.md`「Servo ホスト側のページの操作」節）を実装する前に、Servo 0.7.0 の公開 API で必要な挙動が取れるかを確かめる。本体側でページ JS を保つ案は、本体が読み込み後にページ JS のヒープを持たない（`crates/fandhe-browser-cdp/src/runtime/evaluate.rs`）ことと #1350 の判断から採らない。

- Servo のソースは改変しない（[licensing](../../.claude/rules/licensing.md)）。依存は追加していない
- 試作コードは使い捨てで、リポジトリには入れていない（一時モジュールを `#[test] #[ignore]` で動かし、終了後に戻した）

## 結果の要約

| 項目 | 結果 | 設計への影響 |
| ---- | ---- | ------------ |
| 座標クリック | 成立 | 入力は `WebView::notify_input_event` で足りる。Servo の改変は不要 |
| 文字入力（キー・IME） | 成立 | 同上。非 ASCII は IME の確定イベントで入る |
| 全ページ撮影 | 成立（`resize` 方式） | `take_screenshot` の矩形はビューポートに丸められるため、高さを広げてから撮る |
| ★1 `navigator.webdriver` | 満たす（主フレーム） | `UserContentManager::add_script` で登録できる。iframe は既知の制約 |
| ★2 `request_navigation` | 条件付きで満たす | script 起点の遷移は止められる。iframe の `src`・302 の移動先は通らないため、iframe は読み込み先の許可リスト（#1571）、リダイレクト先は Servo 内部の制限と組で scheme を絞る。回帰テストと現在 URL の検査を TASK-120 の完了条件にする |
| ★3 `pushState` の通知 | 前半は満たす（ソース根拠） | `WebView::url()` を書き換える唯一の経路 `set_history` が直後に必ず `notify_url_changed` を呼ぶ。`doc_seq` の起点を通知で取る場合だけ `url` の直接依存が要る |
| (5) 再パースの要素順 | パーサー由来は不変、DOM 操作由来は一部変化 | 書き戻し方式で順序が変わる場合があることを前提に、番号の対応表を本体で検査する（TASK-121 の確定条件 1） |
| (6) `file:` のサブリソース | 0.1.2 で拒否 | #1571 の許可リストで読み込み前に拒否（GHSA-pxmc-hqwc-jh66） |
| (12) 保存 HTML の再パースでの実行 | 経路なし | 書き戻した HTML を navigate の経路へ再投入しない限り、スクリプト・`meta refresh` は動かない |

## 入力と撮影（2026-11-16 試作・2026-11-18 再実行）

試験ページは 1280x800・倍率 1.0。2026-11-18 の再実行（`fd341de`）でも出力は同じだった。

### 座標クリック

- `getBoundingClientRect()` の中心（360, 270）へ `MouseMove` → `MouseButton` の Down / Up を送ると、`mousedown`・`click` のリスナーが走る
- 出力: `log=["mousedown trusted=true","click trusted=true"]`。完了通知 `notify_input_event_handled` も 3 件（Move・Down・Up）届く
- `isTrusted` が `true` になる点は SEC-2 の判断対象（TASK-118）

### 文字入力

- クリックで `input` にフォーカス（`document.activeElement` が `INPUT`）した後、`KeyboardEvent`（`Key::Character`）を Down / Up で送ると `value="ab"`。`keydown`・`keypress`・`input` が順に `isTrusted=true` で出る
- `ImeEvent::Composition`（`CompositionState::End` 単発・`"XY"`）で `value="abXY"`。`compositionend`・`input` が出る
- 日本語などの非 ASCII 文字は IME の確定イベントで入れる前提にできる。キーごとの非 ASCII 入力は未確認（項目 9）

### 全ページ撮影

- 高さ 3000px のページで、`take_screenshot(None)` も、矩形 1280x3000 を渡した場合も 1280x800（ビューポートに丸められる）
- `WebView::resize` で高さを 3000 に広げると `innerHeight=3000` になり、1280x3000 で撮れる。最下行の画素は期待どおり青（`Rgba([0, 0, 255, 255])`）
- 撮影後のサイズ復元・`vh` と `position: fixed` の見え方は未確認（項目 9）

## 合格条件（★）

### ★1 `navigator.webdriver`（SEC-5）: 満たす

`Navigator.prototype` に getter（`configurable: false`）を `add_script` で登録した。

- 主フレームの head 内の最初のインラインスクリプトから `[true,"boolean"]`。`<head>` の無い文書・`about:blank` も `true`。登録なしの対照は `undefined`
- 再定義は `TypeError`、`delete` は `false` を返し値は `true` のまま
- 制約（既知として記録）:
  - iframe（同一オリジンの `src`・`srcdoc`）には注入されない（子文書では `undefined`）
  - インスタンス側への上書き（`Object.defineProperty(navigator, ...)`）は止められない
  - getter の `toString()` が `function(){return true}` と見える

### ★2 `request_navigation`（RENDER-10）: 条件付きで満たす

呼ばれる（拒否すると遷移が止まる）:

- `location` 代入・フォームの GET / POST 送信・`a.click()`・`location.reload()`・iframe の `contentWindow.location` 代入
- 主フレームの遷移を拒否すると、サーバーへ要求は出ず、ページは元のまま

呼ばれない:

- 初回の `load_url`（ホストは `servo-host/src/page.rs` で http(s) に限定済み）
- 302 の移動先（移動先が http(s) 以外なら Servo の fetch が network error にする）
- iframe の `src` 属性の変更と、初期 HTML の `<iframe src>`
- `javascript:` の URL（スクリプトは実行されるが、scheme は変わらない）

呼び出しは constellation の `schedule_navigation` に限られる。iframe の `src` 経由の読み込みのうち許可外の scheme（`file:` 等）は、ホストの読み込み先の許可リスト（`servo-host/src/guard.rs`。#1571・0.1.2）が読み込み前に拒否する。ただし許可リストは `data:`・`blob:` を許可するため、iframe の `data:`・`blob:` は現状では拒否されない。また検査点に渡るのは最初の URL だけで、リダイレクト先は許可リストでは見えない（移動先が http(s) 以外なら Servo 内部の fetch が拒否する）。spec の ★2 は「リダイレクト・`javascript:` でも呼ばれる」ことを求めているため、文言どおりには満たさない。

2026-11-18 のオーナー回答で、目的（遷移先の scheme を絞る）を `request_navigation` と許可リストの組で満たすと読み替えた。2 つの検問は同等ではないため、次を条件とする（TASK-120 の完了条件。spec の RENDER-10 と ★2 の文言は spec リポ側で直す）。

- 書き分け: 主フレームは `request_navigation` と現在 URL の検査で止める。iframe の許可外 scheme は読み込み先の許可リストで止める。リダイレクト先の http(s) 以外は Servo 内部の制限に頼る（許可リストでは見えない）。iframe・サブリソースの `data:`・`blob:` は読み込みを許し、操作と DOM 取得の対象にしない。主フレームの `data:`・`blob:` への遷移は止める（2026-11-18 の判断。RENDER-10・#1581）
- 回帰テスト 2 本で固定する: 「302 の移動先が `file:`・`data:` なら失敗する」「`meta refresh` で `data:`・`file:` へ行けない」。現状は Servo 内部の挙動に頼っており、`servo` の版更新で黙って変わりうる
- 操作と DOM 取得の前に、現在 URL が http / https であることを必ず検査する（RENDER-10・RENDER-11 の `invalid_state`）
- 試していない経路（`meta refresh`・`Refresh` ヘッダ・`target=_blank`・`history.back()`）が `request_navigation` を通るかを確かめる

TASK-120 の実装結果（2026-11-18・#1581）:

- ホストの `request_navigation` は、http / https・`about:blank`・`about:srcdoc` だけを許し、それ以外を明示的に拒否する（`servo-host/src/guard.rs` の `navigation_allowed`。既定の drop は許可になるため必ず応答する）。主フレームか iframe かは区別できないため、iframe をスクリプトで `data:`・`blob:` へ移す遷移も止まる（`src` 属性での読み込みは許す）
- 検査を外して確かめた結果、Servo 0.7.0 自身は主フレームの `data:`・`blob:` への遷移を止めない。`location`・`window.open(url, '_self')`・`blob:` の iframe からの `top.location`・`meta refresh`・`Refresh` ヘッダのどれでも移れてしまうため、この検査が要る
- 未試験だった経路: `meta refresh` と `Refresh` ヘッダは `request_navigation` を通る。`target=_blank` は通らず、新しいウィンドウも作られない（主フレームは移らない）。別文書への `history.back()` は通らないが、戻れるのは検査を通った履歴だけ
- Servo 内部の制限に頼る経路: 302 の移動先は `request_navigation` を通らず、Servo の fetch が `data:`・`file:` への移動を止める。初回の `load` で 302 の移動先が `data:` のとき、Servo は失敗にせず元の URL の空のページで止まる。`file:` は読み込み先の許可リストも止める
- 回帰テスト: `servo-host/tests/stdio_server.rs` の `main_frame_navigation_to_disallowed_scheme_is_blocked` と `initial_load_redirect_to_disallowed_scheme_does_not_reach_target`
- 操作と DOM 取得の前の現在 URL の検査（`invalid_state`）と、`data:`・`blob:` の iframe を操作・取得の対象にしないことは、対象のメソッド（`dom_snapshot`・`act`）とエラー kind ができる TASK-122・TASK-126 で入れる（kind は TASK-121 で確定する）
- 内部アドレスへの接続は egress プロキシに頼るため、TASK-121 の確定条件 7（操作モードは `Isolated` のときだけ）を前提とする

### ★3 `pushState` の通知（RENDER-10）: 前半は満たす（ソース根拠）

- `WebViewDelegate::notify_url_changed` は引数が `url::Url` で、`servo` が `Url` を再輸出していない。`servo-host` は `url` に依存していないため、この通知を受け取る実装を書けなかった
- 代わりに `WebView::url()` のポーリングで、`pushState`・`replaceState`・`location.hash` の変更を検出できた。いずれも `load_status` の通知は出ない
- 通常の遷移の通知の順: `request_navigation` → `title None` → `Started` → `title` → `HeadParsed` → `Complete`。初回の読み込みは `Started` が来ない。`history.back()` は `title` の通知だけ
- ソースでの根拠（2026-11-18）: `servo-0.7.0/webview.rs` の `set_history` は、`WebView::url()` が読む履歴（`back_forward_list` と位置）を書き換えた直後に、無条件で `notify_url_changed` を呼ぶ。履歴を書き換えるのは `set_history` だけ（constellation の `HistoryChanged` から呼ばれる。`servo.rs`）。ポーリングで `url()` の変化を見た `pushState`・`replaceState`・`location.hash` では、通知も出ている
- よって ★3 の前半（通知が届く）は、依存を足さずにソースを根拠に満たすと記録する。`doc_seq` の起点を通知で取る（通知駆動にする）場合だけ、`url =2.5.8`（`servo-host/Cargo.lock` に既にある版）を `servo-host` の直接依存に足す。足すときは `=2.5.8` に固定し、`servo` の版更新と同時に上げる旨を `Cargo.toml` のコメントに書く。足す直前に侵害情報を確かめ直す（[dependency-policy](../../.claude/rules/dependency-policy.md)）

## その他の確認項目

### (5) 本体の直列化器で戻した HTML の再パース

- 直列化器は `crates/fandhe-browser-core/src/dom/serialize.rs` の `Document::serialize_html()`
- パーサーが作った木 15 試料: 直列化 → 再パースで要素の並び（前順のタグ名列）は全件一致。直列化結果は固定点
- DOM 操作で作った「パーサーが作れない形」9 試料のうち 5 件で変化（`p` 内の `div`・`table` 内の `div`・入れ子の `form`・`div` 内の `tr`・head の `noscript` 内の `p`）。2 回目以降は固定点
- 限界: 前順のタグ名列だけを比べたため、入れ子だけが変わる場合は検出できない。実サイトでの頻度は未測定

### (6) `file:` のサブリソース

- 試作時（0.1.1）は、ページ内の iframe から `file:` の内容が読み込まれ、撮影に写った。Servo の埋め込み API では `file:` のハンドラを外せない
- 0.1.2（#1571）で、読み込み先の許可リスト（http・https・data・blob・`about:blank`・`about:srcdoc`）を全ての読み込みの前に検査するようにした。回帰テストは `servo-host/tests/stdio_server.rs` の `load_allowlist_rejects_disallowed_scheme`・`load_allowlist_rejects_disallowed_main_frame`
- 経緯は公開済みの Security Advisory GHSA-pxmc-hqwc-jh66 を参照

### (12) 本体が保存した HTML の再パースからの実行・遷移

- 経路なし。`/ai/snapshot`（ai の `api.rs`）・`Runtime.evaluate`（core の `script_context.rs`）は再パースだけで、スクリプトを評価しない
- ページスクリプトの実行は `Page.navigate` で取得した HTML だけ（cdp の `page.rs`。既定は無効）
- `on*` 属性・`meta refresh` を実行する箇所は無い
- 未確認: js crate のエンジン内部に DOM 経由の遷移 API があるか、cli の `query --file` などの別の入口

## 残り（未確認の項目）

追跡は MS-10 のトラッキング #1589。項目 4・7〜11 は #1579、★2 の条件は #1581（TASK-120）、★3 後半は #1582（TASK-121）。

| 項目 | 内容 | 依存する TASK |
| ---- | ---- | ------------- |
| ★3 後半 | 通知駆動に決定（2026-11-18。URL 変更の通知と新しい文書の読み込みの両方で上げる）。`url =2.5.8` を足して実際に受け取る | TASK-121 |
| ★2 の条件 | 遷移の検査・回帰テスト・未試験の経路は TASK-120 で済み。現在 URL の検査と `data:`・`blob:` の iframe の除外は対象のメソッドと同時に入れる | TASK-122・TASK-126 |
| (4) | 矩形・ヒットテストを Rust 側で取れるか | TASK-121（取れなければ iframe 上の拒否・スクロール上限 3 回を最終形にする） |
| (7) | `JSValue::Element` を埋め込み API から再利用できるか | TASK-122・TASK-126 |
| (8) | `select`・ダイアログ・新しいウィンドウ・ファイル選択の delegate の API と既定の動き | TASK-126（`window.open` は既定で `null` を返すことだけ確認済み） |
| (9) | キーごとの非 ASCII 入力、hidpi の座標変換、resize 後の `vh`・`fixed` とサイズ復元、初回フレーム前のクリック | TASK-126・撮影の強化（RENDER-9） |
| (10) | 8 MiB 級の DOM の直列化の時間とメモリ | TASK-121（メモリの見積もり） |
| (11) | Cookie を一時領域の外へ書かないか。console の出力・フォームの値が stderr に出るか | TASK-126・TASK-130 |

## オーナー判断

決まったこと（2026-11-18）:

1. ★2 は条件付きで読み替える（条件は上の ★2 の節。spec の文言は spec リポ側で直す）
2. ★3 の `url =2.5.8` の追加は認める。ただし本書の PR には入れず、通知駆動を選ぶ場合に TASK-121 以降で足す
3. iframe・サブリソースの `data:`・`blob:` は読み込みを許し、操作と DOM 取得の対象にしない。主フレームの `data:`・`blob:` への遷移は止める（#1581）
4. ★3 は通知駆動にする。URL 変更の通知と新しい文書の読み込みの両方で `doc_seq` を上げる（#1582）

残っていること:

- ★1 の制約（iframe には注入されない・インスタンス側の上書きは止められない）を既知の制約として TASK-118 の判断記録に載せるか

## 再現方法

試作コードは使い捨てでリポジトリに無い。再現する場合は、`servo-host/src/` に一時モジュールを置き `main.rs` に `#[cfg(test)] mod ...;` を足して、`cargo test --manifest-path servo-host/Cargo.toml <名前> -- --ignored --nocapture --test-threads=1` で実行する。`Engine` は 1 プロセスに 1 つしか作れないため、試作テストは 1 本ずつ実行する（2 本目は `AlreadyCreated` で失敗する）。終わったら一時の変更を戻す。
