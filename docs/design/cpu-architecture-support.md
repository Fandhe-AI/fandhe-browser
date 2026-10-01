# 対応 CPU アーキテクチャと rusty_v8 prebuilt 公開ターゲット

対応 `TASK-57`（57.1）/ `MS-3` / ビヘイビア `XOS-5`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的

`XOS-5` の前提である「`rusty_v8`（crates.io の `v8` crate）の prebuilt 静的ライブラリがどのターゲットで公開されているか」を整理し、
x86_64 優先方針と本リポジトリが採用する `v8` crate バージョンの関係を明確にする。
本ドキュメントは GitHub Releases の公開アセットを机上で確認したもので、各ターゲットでの実機ビルド・実行の実測ではない（実測は `TASK-58` 等）。

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

`.github/workflows/ci.yml` の 3 OS matrix について、main の CI 実行（2026-10-01、`binary-size` ジョブのログの `host:` 行）で確認したホスト triple と、そこで取得される prebuilt は次のとおり。

| ランナー | ホスト triple | 対応する prebuilt |
| -------- | ------------- | ----------------- |
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | 優先ターゲットに一致 |
| `macos-latest` | `aarch64-apple-darwin` | 優先ターゲットに一致 |
| `windows-latest` | `x86_64-pc-windows-msvc` | 優先ターゲットに一致 |

musl ターゲットと aarch64 Linux は CI の matrix に含まれていない。追加の要否は本ドキュメントの範囲外。

## prebuilt が公開されていないターゲットの扱い

`TASK-57.2`（#472）で記載する。
