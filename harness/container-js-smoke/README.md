# container-js-smoke

既定コンテナ（V8 同梱・musl 静的・scratch）の中で JS が実際に実行できることを確認するハーネス（TASK-66.3・Issue #304・`CTR-2`・`JS-1`・MS-5。親 Issue #301）。rusty_v8 の musl ビルドは上流 CI でテストされておらず、`TASK-66` は最小イメージ上での動作を要求する。

**現状: 既定コンテナで 17/17 ケース成功**（結果は「実行結果」参照）。

## 方式

配布バイナリ `/fandhe-browser` を JS ワーカーモード（環境変数 `FANDHE_BROWSER_JS_WORKER=5`・`FANDHE_BROWSER_JS_WORKER_ENGINE=v8`）で `docker run -i` 起動し、ホスト側の `driver.mjs` が親プロセスの代わりに stdio 越しにワーカープロトコル v5（`crates/fandhe-browser-js/src/worker_protocol.rs`）を話す。本番の JS 評価もこの子プロセス経路（`run_js_worker_if_requested` → `worker_main`）で行われるため、配布物そのもの（同じイメージ・同じバイナリ・同じ入口）の V8 を直接動かせる。

検査範囲は `crates/fandhe-browser-js/tests/conformance.rs`（TASK-28.4）の次の 3 分類に相当する処理:

| 分類 | conformance.rs | 本ハーネスのケース |
| ---- | -------------- | ------------------ |
| スクリプト評価 | `check_script_evaluation` | `eval-arith`（`1 + 2 * 3` → 7）・`eval-string`・`eval-bool`・`eval-null`・`eval-undefined`・`eval-syntax-error`（ERROR kind=Evaluation）・`eval-throw`（ERROR・メッセージに `boom`） |
| グローバル関数注入 | `check_global_function_injection` | `inject-register-print/add/fails`（REGISTER_GLOBAL_FUNCTION）・`inject-print`（NATIVE_CALL で `["hello"]` を記録）・`inject-add`（`add(2, 3)` → 5）・`inject-fails`（ネイティブ関数の Err が ERROR kind=Evaluation になる） |
| DOM 風バインディング | `check_dom_like_binding` | `dom-bind`（BIND_DOM_LIKE_OBJECT）・`dom-roundtrip`（`setText`/`getText` → `Fandhe`）・`dom-count`（ストアが `title=Fandhe`・`body=x`、`count()` → 2）・`dom-missing`（未設定キーは空文字） |

ドライバ側が `conformance.rs` の `NativeFn` 相当（逆方向 RPC の応答役）を担う。

## 使い方

```bash
make check-container-js-smoke                                   # 自己テスト → 既定コンテナの確認（要 docker・node>=18。build を含む）
CONTAINER_JS_SMOKE_SKIP_BUILD=1 CONTAINER_JS_SMOKE_IMAGE=fandhe-browser:default-smoke303 make check-container-js-smoke   # 既存イメージを使う
bash harness/container-js-smoke/run.sh --skip-build --image fandhe-browser:default --case-timeout 20
bash harness/container-js-smoke/self-test.sh                    # docker 不要のオフライン自己テスト
```

環境変数: `CONTAINER_JS_SMOKE_IMAGE`・`CONTAINER_JS_SMOKE_PLATFORM`・`CONTAINER_JS_SMOKE_SKIP_BUILD`（非空で有効）・`CONTAINER_JS_SMOKE_CASE_TIMEOUT`（秒・1〜120・既定 20）。`make ci` と CI には含めない（docker build と実コンテナ起動を伴う。`container-smoke` と同じ扱い）。

## 出力契約

stdout には結果行だけを出し、診断・コンテナの stderr は stderr へ出す。

```text
container-js-smoke-env: role=default image=<ref> id=<sha256:...> platform=<os/arch> docker_server=<ver> commit=<sha|unknown> dirty=<0|1|unknown> built=<yes|no> protocol=5 engine=v8
container-js-smoke: case=<id> category=<eval|inject|dom> status=<ok|mismatch|error-frame|timeout|crash|protocol-error|not-run>
container-js-smoke-summary: role=default total=17 executed=<N> success=<N> result=<pass|fail|blocked> reason=<label|none>
```

| exit | 意味 |
| ---- | ---- |
| 0 | 全ケース成功 |
| 1 | 実行したが失敗あり（`reason` に分類。`handshake-*`・`timeout`・`crash`・`protocol-error`・`case-failures`・`shutdown-unclean`・`no-cases-executed`） |
| 2 | 使用エラー・前提不足（docker / node 不在、daemon 不達、`--skip-build` でイメージ不在）、または HELLO の版・エンジン不一致（`result=blocked`） |

fail-closed: 未知の tag・不正な値タグ・不正 UTF-8・過大なフレーム長（確保前に拒否）・想定外のフレーム順・タイムアウト・EOF（クラッシュ）はすべて失敗にする。致命的な失敗の後続ケースは `status=not-run` とし、成功にはしない。実行ケース 0 件も失敗。EOF・クラッシュ時は worker の終了コードと stderr の末尾（300 文字に無害化）を stderr に出す。

## セキュリティ

- コンテナは `--network none`・`--cap-drop ALL`・`no-new-privileges`・`--read-only`・イメージ既定の非 root（UID 65532）。ボリュームマウント・ポート公開・`--privileged` は使わない。`-e` は 2 つの固定変数のみで、ホストの環境変数は継承しない。
- 評価するスクリプトは `driver.mjs` 内の固定定数。argv・環境変数からは受け取らない。docker はシェルを介さず引数配列で起動し、image ref・platform・timeout は正規表現で検証する。
- 終了時と異常時は `docker rm -f` で後始末する（docker CLI を kill しただけではコンテナが残るため）。npm・Cargo 依存は追加していない（Node の組み込みモジュールのみ）。

## 既知の限界・対象外

- 親プロセス（`V8ProcessEngine`）がコンテナ内で `/proc/self/exe` を自己再実行する経路は検証しない。親の役はホストのドライバが担う。検証するのは V8 の musl 実行と、ワーカー入口・逆方向 RPC・DOM 風バインドまで。
- ワイヤ形式は `driver.mjs` に v5 をハードコードした写し（二重実装）。`PROTOCOL_VERSION` が上がると HELLO で `blocked` になり検出される。Rust 側との契約テスト（`cdp3_stages_mjs_result_line_satisfies_rust_contract` に倣う形）は後続候補。
- `JS-6`（タイムアウト・結果サイズ上限）、軽量コンテナ（boa）、arm64（TASK-64.3 で任意）、CI ジョブ化は対象外。
- 失敗時の扱い（spec 決定 4）: スモークが失敗した場合は既定コンテナを配布せず、代替案 A（glibc＋`distroless/cc`）へ切り替えるかをオーナーが判断する（owner:human）。本ハーネスは記録までで、切り替えは行わない。

## 実行結果

2026-10-16、既存イメージ `fandhe-browser:default-smoke303`（ビルド元コミット `7abedd8001c8`。実行時の HEAD は `8b569aa21d85` で、差分は harness・CLI の追加のみ。JS ワーカー経路は同一）、amd64、docker server 29.8.1、`CONTAINER_JS_SMOKE_SKIP_BUILD=1` で実行（env 行が `built=no` なのはこのため）:

```text
$ CONTAINER_JS_SMOKE_SKIP_BUILD=1 CONTAINER_JS_SMOKE_IMAGE=fandhe-browser:default-smoke303 make check-container-js-smoke
container-js-smoke self-test: pass=36 fail=0
container-js-smoke-env: role=default image=fandhe-browser:default-smoke303 id=sha256:779f250c... platform=linux/amd64 docker_server=29.8.1 commit=8b569aa21d85 dirty=1 built=no protocol=5 engine=v8
info: worker stderr head: fandhe-browser-js worker: v8 platform initialized as Protected
container-js-smoke: case=eval-arith category=eval status=ok
  （eval-* 7 件・inject-* 6 件・dom-* 4 件、すべて status=ok）
container-js-smoke: case=dom-missing category=dom status=ok
container-js-smoke-summary: role=default total=17 executed=17 success=17 result=pass reason=none
(exit 0)
```

- V8（musl 静的・scratch）でスクリプト評価・グローバル関数注入・DOM 風バインディングが動作した。クラッシュ・プロトコル違反なし。ワーカーは SHUTDOWN 後に exit 0。
- ネイティブ関数の Err は `ERROR kind=Evaluation` になる（`Uncaught Error: always fails`）。構文エラーは `Uncaught SyntaxError: Unexpected end of input`。
- 後始末確認: 実行後に `fandhe-js-smoke*` のコンテナは残らない。
- arm64: 任意（TASK-64.3）。本ホストは x86_64 のため未実施。

### 失敗の詳細

なし（全ケース成功のため、代替案 A への切り替え判断は不要）。
