# v8 prebuilt 同梱部品の照合記録

- **対応ビヘイビア**: `OSS-4`（関連: `OSS-2`）
- **対応タスク**: TASK-78（TASK-78.3・#485）。照合先は TASK-76（#63・PR #801）の `NOTICE` 第 1 節
- **マイルストーン**: MS-4
- **実施日**: 2026-10-11
- **関連 Issue**: #483（TASK-78.1）・#484（TASK-78.2）・#486（TASK-78.4）
- **状態**: 実測済み。`NOTICE` と `LICENSE-THIRD-PARTY` を更新した。ライセンス判断事項は末尾「要判断事項」のとおりオーナーへ報告する（Agent は決めていない）

## 目的

採用している `v8 = "=152.2.0"` の prebuilt 静的ライブラリに実際に含まれる部品を列挙し、TASK-76 が机上で作った `NOTICE` 第 1 節の一覧（V8・libc++・libc++abi・ICU）と照合する（`OSS-4`）。

## 調べた成果物

取得元はすべて `https://github.com/denoland/rusty_v8/releases/download/v152.2.0/`（公開 Release）。ダウンロード物は実行・リンクせず、`strings`・`nm` による静的解析だけを行った。

| ターゲット | ファイル | 展開前 SHA256 | 展開後 SHA256 |
| ---------- | -------- | ------------- | ------------- |
| x86_64-unknown-linux-gnu | `librusty_v8_release_x86_64-unknown-linux-gnu.a.gz` | `b6683e9afcb77fbd8cb2c3acf45d34d52029ab216b332522c95179c12f0fed3c` | `8b4ff8c1813442a8dc84c849a088f32891b7a911e9d259d938f92907fd8cc225` |
| x86_64-unknown-linux-musl | `librusty_v8_release_x86_64-unknown-linux-musl.a.gz` | `68f284fb8184b9f9e8d7e23b8f2675dbe464f43f77c0bf152f705bf7ead29558` | `fb89ba94eb80ebeddb2828341f345d0196455a1da4fc120553abcdefb2532841` |
| x86_64-pc-windows-msvc | `rusty_v8_release_x86_64-pc-windows-msvc.lib.gz` | `0b17ca072bae37dd4ff00e6014d2b413becb031c9342ee11cb8226a5881f62b2` | `b62b3d08cc4e1350275a97136efaffd5232fefdccd2597f193b817b80d58d627` |

macOS（aarch64-apple-darwin）の prebuilt は実測していない（`NOTICE` にも明記）。macOS で別の部品が入る可能性は未確認で、#486 などで最終バイナリの確認時に補う。

## 手法

固定コマンドだけを使う（外部入力を連結しない）。

```sh
# third_party のパス（デバッグ情報に残る文字列）
strings -n 6 <archive> | grep -oE '(third_party|v8/third_party)/[A-Za-z0-9_+.-]+' | sort | uniq -c | sort -rn
# 定義済みシンボル / 未定義シンボル（ELF）
nm --defined-only <archive> | awk '{print $3}' | grep -c '<Itanium 修飾名の接頭辞>'
nm -u <archive> | awk '{print $2}' | grep -c 'temporal_rs_'
# Windows (COFF) は GNU nm で読めないため strings の MSVC 修飾名（例 @absl@@）を数える
```

リリースビルドではパス文字列が落ちるため、パス 0 件だけでは「非同梱」の根拠にならない。そこでシンボルの有無（Itanium は `_ZN<長さ><名前空間>`、MSVC は `@<名前空間>@@`）も併記して判定した。

## 列挙結果

### 区分 A: TASK-76 の一覧に載っていて、同梱を確認したもの

| 部品 | 根拠（gnu） | 備考 |
| ---- | ----------- | ---- |
| V8 | `_ZN2v8` 定義 82086 個 | |
| libc++ | `third_party/libc++` 2689 パス・`_ZNSt4__Cr` 定義 8549 個 | gnu・musl・MSVC のすべてで確認（MSVC は `@__Cr@std@@` 71016 件） |
| libc++abi | `_ZN10__cxxabiv1` 定義 53 個 | gnu・musl のみ。MSVC は 0 件で同梱されない |
| ICU | `_ZN6icu_78` 定義 7978 個 | ICU 78。3 ターゲットとも確認 |

### 区分 B: 同梱されているが TASK-76 の一覧に無かったもの（差分。`NOTICE` へ追記した）

| 部品 | gnu のシンボル | MSVC | ライセンス（取得した上流の LICENSE に基づく） |
| ---- | -------------- | ---- | ------------------------------------------- |
| abseil-cpp | `_ZN4absl` 定義 4291 個 | 確認 | Apache-2.0 |
| llvm-libc | `__llvm_libc_` を含むシンボル 93 個 | 確認 | Apache-2.0 WITH LLVM-exception |
| highway | `_ZN3hwy` 定義 92 個 | 確認 | Apache-2.0 OR BSD-3-Clause（`random-inl.h` のみ CC0-1.0。同ファイルの文字列は 3 ターゲットとも 0 件） |
| simdutf | `_ZN7simdutf` 定義 247 個 | 確認 | MIT（Chromium 同梱コピーの `README.chromium`。上流は Apache-2.0 OR MIT）。版は `SIMDUTF_VERSION` が 7.7.0 |
| inspector_protocol (crdtp) | `_ZN8v8_crdtp` 定義 326 個 | 確認 | BSD-3-Clause（V8 の LICENSE は「サブディレクトリの LICENSE に従う」とするだけで V8 の行では覆えない） |
| fast_float | `_ZN10fast_float` 定義 17 個 | 確認 | Apache-2.0 OR BSL-1.0 OR MIT |
| dragonbox | `_ZN3jkj` 定義 2 個 | 確認 | Apache-2.0 WITH LLVM-exception OR BSL-1.0 |
| siphash (halfsiphash) | `halfsiphash` 定義 1 個 | 確認 | CC0-1.0（ソースのヘッダと LICENSE で確認） |
| rapidhash-v8 | `third_party/rapidhash-v8` パス 1 件 | 確認 | BSD-2-Clause（V8 ツリーの LICENSE で確認） |

musl の prebuilt は gnu とほぼ同じ集合（名前空間ごとのシンボル数が ±数十個の範囲）だった。ヘッダのインライン展開で入る部品（fast_float・dragonbox・rapidhash・halfsiphash）も、同梱コードとして一覧に含めた。

### 区分 C: Cargo 依存として入るもの（`NOTICE` 第 1 節の対象外）

- prebuilt には未定義シンボル `temporal_rs_*` が 307 個あり、定義は 0 個。v8 crate の Cargo 依存 `temporal_capi` が満たす。prebuilt 内のパス `third_party/rust/chromium_crates_io/vendor/temporal_capi-v0_2/bindings/cpp` は C++ バインディングヘッダのもの。
- 推移的に入る `temporal_rs`・`icu_*`（ICU4X）・`timezone_provider`・`zoneinfo64` は `Cargo.lock` に載り、cargo-deny / cargo-about の棚卸し（#483・#484・#486）で扱う。二重計上と漏れを避けるため `NOTICE` 第 1 節には書かない。

### 区分 D: 調べたが同梱されていないもの

パス・シンボルとも 0 件（gnu・musl・MSVC で確認。ただし utf8-decoder・valgrind・wasm-api はパスのみ確認）。

| 部品 | 確認したシンボル |
| ---- | ---------------- |
| libunwind | `__unw_`（定義 0）。`_Unwind_RaiseException` は未定義参照 1 個のみ（libgcc などリンク時に別途解決される） |
| zlib | `inflate`・`deflate`（0） |
| partition_alloc | `_ZN15partition_alloc`（0） |
| re2 | `_ZN3re2`（0） |
| googletest | `_ZN7testing`（0） |
| jsoncpp | `_ZN4Json`（0） |
| fp16 | `fp16_ieee_`（0） |
| utf8-decoder・valgrind・wasm-api | パス 0 件 |

## TASK-76 の一覧との照合結論

| 項目 | 件数 |
| ---- | ---- |
| 一致（区分 A） | 4（V8・libc++・libc++abi・ICU）。ただし libc++abi は MSVC では同梱されない |
| 差分（区分 B。`NOTICE` へ追記） | 9 |
| Cargo 側で扱う（区分 C） | `temporal_rs` を中心とする Rust 製部品（#483・#484・#486） |
| 非同梱（区分 D） | 10 |

## TASK-76 の「実リビジョンとの一致は未検証」の解消範囲

DEPS が固定するリビジョンと v8 crate 内蔵コピーを、代表ファイルの全内容比較（`diff`）で突き合わせた。

| 部品 | 比較ファイル | 結果 |
| ---- | ------------ | ---- |
| libc++ | `third_party/libc++/src/include/__config`（`b16984ce99c7...`） | 完全一致。`_LIBCPP_VERSION` は 230000 |
| ICU | `third_party/icu/source/common/unicode/uvernum.h`（`d578f2e8b7bd...`） | 完全一致。major 78 |
| libc++abi・その他 | 未比較 | 「未検証」のまま据え置く |

一致を確認できたのは代表 1 ファイルで、リビジョン全体の一致を意味しない。`LICENSE-THIRD-PARTY` の該当節の文言はこの範囲に限って更新した。

## 更新した成果物

- `NOTICE` 第 1 節: 区分 B の 9 部品を追加。libc++abi の同梱条件を実測に合わせた。区分 C・macOS 未実測の注記を追加
- `LICENSE-THIRD-PARTY`: 節 5〜13 に上流の LICENSE 本文を無改変で追加（`NOTICE` の各行と 1 対 1）。節 2・4 の未検証文言を上表の範囲に更新

## 要判断事項（オーナーへ報告。承認は得ていない）

1. **CC0-1.0**（siphash。highway の `random-inl.h` は prebuilt に入っていない）は `.claude/rules/licensing.md` の許可ライセンス列挙に無い。許可するか。CC0 は法的に帰属表示を要しないが、安全側として載せている。
2. **複数ライセンスから選べる部品**（highway・simdutf〔Chromium 同梱コピーは MIT 宣言〕・fast_float・dragonbox）でどの選択肢を適用するか。ここでは選ばず、SPDX 式と上流の LICENSE 全文をそのまま載せた。
3. **帰属表示を必須としないライセンス**（BSL-1.0・LLVM-exception・CC0）の部品も載せるか。安全側として載せている。
4. **ヘッダのインライン展開だけで入る部品**（fast_float・dragonbox・rapidhash）の扱い。安全側として載せている。
5. `Cargo.toml` の v8 まわりのコメント（「prebuilt に内包される V8・libc++・ICU」「既定ビルド（`js-v8` 無効）」）が古い。cli の既定 feature は `js-v8` で、V8 は既定構成に入る。本 PR の関心事外のため未修正。
6. macOS の prebuilt は未実測。最終バイナリでの確認とあわせて #486 に申し送る。

## 制約

- 静的アーカイブの中身は、リンクされ得る部品の上限であり、最終バイナリに実際に残る部品とは一致しないことがある。
- シンボルが 0 件でも、リンカがまとめて解決する未定義参照（libunwind など）までは否定していない。
