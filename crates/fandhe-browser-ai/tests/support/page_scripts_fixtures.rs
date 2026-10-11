//! JS 実行後 DOM の snapshot 検証（TASK-113・Issue #787・`JS-4`/`AISNAP-6`/`PLUG-4`）で共有する
//! フィクスチャ。`page_scripts_default_off.rs`（既定オフ）と `page_scripts_snapshot_cases.rs`
//! （実エンジン）が `#[path]` で取り込み、同じ SPA 風ページを有効 / 無効の両方で通せるようにする。
//!
//! スクリプトは `JS-5` の最小 API（`getElementById`・`createElement`・`appendChild`・
//! `setAttribute`・`textContent`）だけを使う。診断の文言（例外メッセージ・console 出力）は
//! 実行時に連結して作るため、ソース上に完成形が現れない。これにより「スクリプト本文」と
//! 「診断の文言」を別々に、AI 向け出力への非混入として検証できる。
//!
//! 各スクリプトは末尾を `undefined;` で終える。最後の式がオブジェクト（`appendChild` の戻り値）だと
//! 完了値が JsValue に変換できず `Exception` として記録されるため（診断の検証を濁さないための回避）。

/// SPA 風ページの URL（loopback。inline スクリプトのみなので接続は発生しない）。
pub const SPA_URL: &str = "http://127.0.0.1:8899/spa.html";

/// 静的要素（`Home` リンク・`Static action` ボタン）と、JS が描画する要素
/// （見出し・リスト 3 件・検索フォーム・`After error` ボタン）を持つページ。
pub const SPA_HTML: &str = r#"<!doctype html>
<html><head><title>Spa</title></head><body>
<a id="home" href="/">Home</a>
<button id="static-btn">Static action</button>
<div id="app"></div>
<script>/* SRC_MARKER_7c1e */
var app = document.getElementById('app');
var h = document.createElement('h1');
h.textContent = 'Rendered dashboard';
app.appendChild(h);
var ul = document.createElement('ul');
for (var i = 1; i <= 3; i++) {
  var li = document.createElement('li');
  li.textContent = 'Item ' + i;
  ul.appendChild(li);
}
app.appendChild(ul);
var f = document.createElement('form');
var inp = document.createElement('input');
inp.setAttribute('type', 'text');
inp.setAttribute('aria-label', 'Search query');
f.appendChild(inp);
var b = document.createElement('button');
b.textContent = 'Run search';
f.appendChild(b);
app.appendChild(f);
undefined;
</script>
<script>
console.log('CON' + 'SOLE_MARKER_' + 7);
throw new Error('DIAG_' + 'MARKER_' + 42);
</script>
<script type="module">/* MODULE_MARKER_91aa */</script>
<script>
var after = document.createElement('button');
after.textContent = 'After error';
document.body.appendChild(after);
var dyn = document.createElement('script');
dyn.textContent = '/* DYN_MARKER_55d0 */';
document.body.appendChild(dyn);
undefined;
</script>
</body></html>
"#;

/// JS が描画する名前（既定オフの snapshot には現れてはならない）。
pub const RENDERED_NAMES: [&str; 3] = ["Rendered dashboard", "Run search", "After error"];

/// AI 向け出力へ混入してはならない文字列（診断の文言・スクリプト本文・スクリプト由来の語）。
pub const FORBIDDEN_IN_AI_OUTPUT: [&str; 7] = [
    "DIAG_MARKER_42",
    "CONSOLE_MARKER_7",
    "SRC_MARKER_7c1e",
    "MODULE_MARKER_91aa",
    "DYN_MARKER_55d0",
    "createElement",
    "throw new Error",
];
