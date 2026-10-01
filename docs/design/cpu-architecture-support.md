# 対応 CPU アーキテクチャと rusty_v8 prebuilt 公開ターゲット

対応 `TASK-57`（57.1・57.2）/ `MS-3` / ビヘイビア `XOS-5`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的

`XOS-5` の前提である「`rusty_v8`（crates.io の `v8` crate）の prebuilt 静的ライブラリがどのターゲットで公開されているか」を整理し、
x86_64 優先方針と本リポジトリが採用する `v8` crate バージョンの関係を明確にする。
本ドキュメントは GitHub Releases の公開アセットを机上で確認したもので、各ターゲットでの実機ビルド・実行の実測ではない（実測は `TASK-58` 等）。
prebuilt がないターゲットの代替手段と musl の注意点（`TASK-57.2`）も末尾に記す。

## prebuilt 公開ターゲット一覧

確認日: 2026-10-01。出典:
[v150.0.0](https://github.com/denoland/rusty_v8/releases/tag/v150.0.0)・
[v150.2.0](https://github.com/denoland/rusty_v8/releases/tag/v150.2.0)・
[v152.2.0](https://github.com/denoland/rusty_v8/releases/tag/v152.2.0)
（いずれも `denoland/rusty_v8` の release アセット。公開日は v150.0.0 が 2026-06-08、v150.2.0 が 2026-07-16、v152.2.0 が 2026-08-20）。

表は feature 接尾辞なしの release アーカイブ（本リポジトリが取得する種別）の有無を示す。

| ターゲット triple | OS | アーキテクチャ | v150.0.0 | v150.2.0 | v152.2.0（採用版） | アーカイブ名 |
| ----------------- | -- | -------------- | -------- | -------- | ------------------ | ------------ |
| `x86_64-unknown-linux-gnu` | Linux（glibc） | x86_64 | あり | あり | あり | `librusty_v8_release_x86_64-unknown-linux-gnu.a.gz` |
| `aarch64-unknown-linux-gnu` | Linux（glibc） | aarch64 | あり | あり | あり | `librusty_v8_release_aarch64-unknown-linux-gnu.a.gz` |
| `x86_64-unknown-linux-musl` | Linux（musl） | x86_64 | なし | あり | あり | `librusty_v8_release_x86_64-unknown-linux-musl.a.gz` |
| `aarch64-unknown-linux-musl` | Linux（musl） | aarch64 | なし | あり | あり | `librusty_v8_release_aarch64-unknown-linux-musl.a.gz` |
| `x86_64-apple-darwin` | macOS | x86_64 | あり | あり | あり | `librusty_v8_release_x86_64-apple-darwin.a.gz` |
| `aarch64-apple-darwin` | macOS | aarch64（Apple Silicon） | あり | あり | あり | `librusty_v8_release_aarch64-apple-darwin.a.gz` |
| `x86_64-pc-windows-msvc` | Windows | x86_64 | あり | あり | あり | `rusty_v8_release_x86_64-pc-windows-msvc.lib.gz` |
| `aarch64-pc-windows-msvc` | Windows | aarch64 | あり | あり | あり | `rusty_v8_release_aarch64-pc-windows-msvc.lib.gz` |

要点は次のとおり。

- v150.0.0 は gnu・darwin・msvc の 6 ターゲットのみで、musl 向けはない
- v150.2.0 で `x86_64` / `aarch64-unknown-linux-musl` が追加され、`XOS-5` の 8 ターゲットがそろう
- v152.2.0 も 8 ターゲットすべてに release アーカイブがある

### XOS-5 の対象外として公開されているもの

- `riscv64gc-unknown-linux-gnu`（v150.2.0 以降）・`aarch64-apple-ios` / `aarch64-apple-ios-sim`（v152.2.0 の release アーカイブ）
- `_ptrcomp` 接尾辞版（x86_64 Linux gnu・darwin 2 種のみ）。v150.0.0・v150.2.0 には `_simdutf` 接尾辞版もある（v152.2.0 の release アーカイブ一覧には見当たらない）。本リポジトリは使わない
- debug アーカイブ。確認した範囲では Linux gnu・macOS・riscv64 向けのみで、musl・Windows 向けは見当たらない。本リポジトリは release のみを使う

## x86_64 優先方針と採用 v8 crate バージョンの対応

x86_64 優先は spec の対象 OS 方針（Linux は x86_64 優先、macOS は Apple Silicon、Windows 10/11）と `XOS-5` に基づく。
優先は実機検証の順序を意味し、prebuilt の有無とは別である。

| ターゲット | 本リポジトリでの位置づけ |
| ---------- | ------------------------ |
| `x86_64-unknown-linux-gnu` | 優先。開発・CI の基準 |
| `x86_64-unknown-linux-musl` | 優先。コンテナイメージで必須（`container-cloud.md` の決定 2） |
| `aarch64-apple-darwin` | 優先（macOS は Apple Silicon） |
| `x86_64-pc-windows-msvc` | 優先（Windows） |
| `aarch64-unknown-linux-gnu` | prebuilt でビルド可能。実機検証は x86_64 より後 |
| `aarch64-unknown-linux-musl` | prebuilt でビルド可能。コンテナでは任意（同決定 2） |
| `x86_64-apple-darwin`・`aarch64-pc-windows-msvc` | prebuilt でビルド可能。実機検証は後回し |

- 採用版は `=152.2.0`（ルート `Cargo.toml` の `[workspace.dependencies]`）で、150.2.0 以上のため上記 8 ターゲットすべてに prebuilt がある。musl 向け prebuilt を使うには 150.2.0 以上が必要
- `v8` は `fandhe-browser-js` の feature `js-v8` からのみ使われ、既定ビルドの依存グラフには入らない
- 取得の仕組み: `v8` の `build.rs`（`prebuilt_profile()`・`prebuilt_features_suffix()`・`static_lib_name()`）が Cargo の `TARGET` 名と feature 接尾辞からアーカイブ名を組み立てる。本リポジトリは既定 feature（`use_custom_libcxx`）だけを使うため、接尾辞なしの release アーカイブを取得する。`use_custom_libcxx` を外さない理由は `Cargo.toml` のコメントを参照
- バージョンを 150.2.0 未満へ下げると musl 向け prebuilt がなくなる。上げる場合は上記アセット一覧を確認し直す（依存の更新はユーザー承認制。[dependency-policy](../../.claude/rules/dependency-policy.md)）
- crates.io 上の最新版との差は今回確認していない

## CI ランナーとターゲットの対応

`.github/workflows/ci.yml` の 3 OS matrix のランナーと、そのホスト triple に対応する prebuilt 候補は次のとおり。ホスト triple は main の CI 実行（2026-10-01、`binary-size` ジョブのログの `host:` 行）で確認した値で、ホストの記録にすぎない。`fandhe-browser-cli` crate が未追加（#174）の間は同ジョブの実ビルドが休眠しており、`js-v8` 有効時の prebuilt 取得実績は確認していない（取得記録の確認は未実施）。

| ランナー | ホスト triple | 対応する prebuilt 候補 |
| -------- | ------------- | ----------------- |
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | 優先ターゲットに一致（取得実績は未確認） |
| `macos-latest` | `aarch64-apple-darwin` | 優先ターゲットに一致（取得実績は未確認） |
| `windows-latest` | `x86_64-pc-windows-msvc` | 優先ターゲットに一致（取得実績は未確認） |

musl ターゲットと aarch64 Linux は CI の matrix に含まれていない。追加の要否は本ドキュメントの範囲外。

## prebuilt が公開されていないターゲットの扱い

`XOS-5`（`TASK-57.2` / `MS-3`）。該当するのは、上記の公開ターゲット一覧に載っていないターゲット（`XOS-5` の 8 ターゲット以外。例: 32bit や BSD 系）と、採用版より古い `v8`（150.2.0 未満）で musl をビルドする場合である。
これらでは `v8` の `build.rs` が対応するアーカイブを取得できず、ビルドは失敗する想定である（取得の仕組みは前節の「取得の仕組み」を参照。失敗の細部は実測していない）。
代替手段は次の 2 つで、いずれも本リポジトリの CI では検証していない。

### 代替手段 1: ソースビルド（`V8_FROM_SOURCE`）

- 環境変数 `V8_FROM_SOURCE` を設定すると、`v8` の `build.rs` が V8 をソースからビルドする
- コストと前提（`rusty_v8` README「Build V8 from Source」。`container-cloud.md` の判断理由で要約済み）
  - ビルドに約 30 分かかる
  - Python 3・`curl`・libclang 21.1+ が必要
  - `gn`・`ninja`・`clang` は見つからなければ自動でダウンロードされる
- musl で使う場合は `RUSTY_V8_MUSL_SYSROOT` で sysroot を指定できる（`container-cloud.md` の代替案 C を参照）
- 本リポジトリの位置づけ: `XOS-5` の 8 ターゲットでは使わない（`container-cloud.md` の決定 3）。未検証である
- `RUSTY_V8_MIRROR` / `RUSTY_V8_ARCHIVE` で、自前で用意したアーカイブの取得元に差し替えることもできる。取得元は信頼できるものに限り、バージョンを固定する（チェックサム検証の有無は確認していない）

### 代替手段 2: 軽量ビルド（V8 を同梱しない）

- `boa` のみを同梱し、V8 の prebuilt をダウンロードしない構成である（`js-engine.md`「JS エンジンの切替方式」のビルド構成表。`JS-1` 系）
- `boa_engine` は純 Rust（`XOS-3`）なので、Rust のターゲットさえあればビルドできる見込みである（実機では確認していない）
- spec 上のビルドコマンドは `cargo build --release -p fandhe-browser-cli --no-default-features --features js-boa`、概算サイズは約 10.05MB（机上の概算）
- **現状、軽量ビルドはまだ利用できない**（`REPAIR-3`: 実装済みを装わない）
  - `fandhe-browser-js` の `js-boa` はプレースホルダで、`boa_engine` は未導入（`TASK-32`・#144 / #165 / #166）
  - `fandhe-browser-cli` には JS エンジン feature の転送と既定がない（`TASK-30.2`・#160）
  - 上記コマンドは spec の提案に基づく将来の構成である
- V8 の代替ターゲット対応ではなく、JS 実行エンジンが `boa` に変わる配布物である。互換性・性能の差が生じうる

### 選び方の目安

| 状況 | 推奨 |
| ---- | ---- |
| `XOS-5` の 8 ターゲット（採用版 `=152.2.0`） | prebuilt（既定） |
| prebuilt がなく V8 が必要（互換性重視） | `V8_FROM_SOURCE`（ビルド時間・ツールチェーンのコストを受け入れる。未検証） |
| prebuilt がなく、ビルドを軽く保ちたい・V8 が不要 | 軽量ビルド（`TASK-30`・`TASK-32` の完了後に利用可能） |

## musl 向け prebuilt の注意点

`XOS-5`・`container-cloud.md` の決定 4。

- musl 向け prebuilt を使うには `v8` 150.2.0 以上が必要（前掲の一覧を参照）
- `rusty_v8` 上流の CI は musl 向け prebuilt をテストしていない。glibc ランナー上で musl ターゲットをクロスビルドするだけで（iOS ターゲットと同じ build-only）、nextest・clippy は musl で実行されない。debug 用の musl アーカイブもない
  - 出典: `rusty_v8` の [ci.yml（コミット 9395618）](https://github.com/denoland/rusty_v8/blob/9395618fb3af7a697c2e9d447b24cd204d050691/.github/workflows/ci.yml)。確認は spec（`container-cloud.md`）の 2026-09-24 時点の記録による
- Deno 本体も Linux 向けは `*-unknown-linux-gnu` だけを配布している（musl 上での V8 の動作実績が gnu より少ないことの傍証。出典は spec を参照）
- 本リポジトリでの扱い: 既定コンテナ（musl・`scratch`）の採用は、`TASK-66` の `scratch` 内 JS スモークテストの成功を条件とする（fail-closed。`CTR-2`）。失敗した場合は代替案 A（glibc＋`distroless/cc`）への切替をオーナーが判断する
- 本リポジトリの CI（3 OS matrix）にも musl は含まれていない（前節を参照）。追加は本ドキュメントの範囲外（`TASK-58` 等）
