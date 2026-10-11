#!/usr/bin/env bash
# 配布コンテナ 1 フレーバー分のビルドと成果物配置を行う（TASK-64.1・64.2・64.3・CTR-1・CTR-4・MS-5）。
# docker/Dockerfile.release の build-default / build-lite ステージから呼ばれ、cargo build の後に
# /out へバイナリ・ライセンス文書・データディレクトリを揃え、同梱エンジンの過不足を検査する。
# ビルド結果は後続の verify-* ステージ（docker/verify-static.sh）が静的リンク検査し、
# 最終 scratch ステージが /out の内容だけを取り込む。
#
# 使い方: docker/release-build.sh <TARGETARCH> <cargo-feature-flags> <期待エンジン crate>
#   TARGETARCH: BuildKit が `--platform` から設定する amd64 / arm64 のみ許可し、
#     x86_64-unknown-linux-musl（必須）/ aarch64-unknown-linux-musl（任意。TASK-64.3）へ写像する。
#     ビルダーのホスト三つ組と一致しないクロスビルドは拒否する（--platform で arm64 ビルダーを選ぶ）。
#   cargo-feature-flags: 空なら既定 feature。許可するのは `--no-default-features` と
#     `--features <名前[,名前]>` だけ（scripts/gen-cargo-licenses.sh と同じ許可リスト）。
#   期待エンジン crate: v8（既定コンテナ）または boa_engine（軽量コンテナ）。
#     期待側が依存グラフに無い、または他方が混入していればビルドを失敗させる（CTR-1・fail-closed）。
set -euo pipefail

arch="${1:?usage: release-build.sh <amd64|arm64> <cargo-feature-flags> <v8|boa_engine>}"
flags="${2-}"
expect="${3:?expected engine crate required}"

# 任意文字列を cargo の --target やパスへ渡さないよう、許可リストで三つ組へ写像する。
case "${arch}" in
  amd64) target="x86_64-unknown-linux-musl" ;;
  arm64) target="aarch64-unknown-linux-musl" ;;
  *)
    echo "error: unsupported TARGETARCH: '${arch}' (amd64 or arm64 only)" >&2
    exit 2
    ;;
esac

case "${expect}" in
  v8) other="boa_engine" ;;
  boa_engine) other="v8" ;;
  *)
    echo "error: unknown engine crate: ${expect}" >&2
    exit 2
    ;;
esac

# feature フラグを許可リストで検証して配列化する（引数注入の防止）。
feat_args=()
read -r -a tokens <<<"${flags}"
i=0
while [ "${i}" -lt "${#tokens[@]}" ]; do
  case "${tokens[$i]}" in
    --no-default-features) feat_args+=(--no-default-features) ;;
    --features)
      i=$((i + 1))
      val="${tokens[$i]:-}"
      if ! [[ "${val}" =~ ^[A-Za-z0-9_-]+(,[A-Za-z0-9_-]+)*$ ]]; then
        echo "error: invalid feature list: '${val}'" >&2
        exit 2
      fi
      feat_args+=(--features "${val}")
      ;;
    *)
      echo "error: unsupported cargo flag: '${tokens[$i]}'" >&2
      exit 2
      ;;
  esac
  i=$((i + 1))
done

# ホスト三つ組＝ターゲットのネイティブビルドに限る（意図しないクロスビルドを拒否。fail-closed）。
host="$(rustc -vV | sed -n 's/^host: //p')"
if [ "${host}" != "${target}" ]; then
  echo "error: build host ${host} does not match target ${target}; build with docker buildx --platform" >&2
  exit 1
fi

# V8 prebuilt（.a.gz）の SHA-256 ピン。値は Cargo.lock の v8 =152.2.0 と連動し、v8 を更新するときは
# 両アーキの rusty_v8 リリース資産（librusty_v8_release_<target>.a.gz）の SHA-256 を同時に差し替える。
# v8 の build.rs は空値だと検証をスキップするため、未知ターゲットは必ずここで失敗させる（fail-closed）。
if [ "${expect}" = "v8" ]; then
  case "${target}" in
    x86_64-unknown-linux-musl)
      RUSTY_V8_ARCHIVE_SHA256=68f284fb8184b9f9e8d7e23b8f2675dbe464f43f77c0bf152f705bf7ead29558 ;;
    aarch64-unknown-linux-musl)
      RUSTY_V8_ARCHIVE_SHA256=419a242557833151cf9dde82c12c8272c3c00015bfe7e340e48164a600d34039 ;;
    *)
      echo "error: no pinned V8 prebuilt SHA-256 for ${target}" >&2
      exit 1
      ;;
  esac
  export RUSTY_V8_ARCHIVE_SHA256
fi

# musl の版は LICENSE-THIRD-PARTY 節 5 の COPYRIGHT 全文と連動する。
# apk-tools 3 では `apk info -v` が詳細表示になり版が取れないため、`apk list -I` の先頭語を使う。
musl_ver="$(apk list -I musl | awk '$1 ~ /^musl-[0-9]/ {print $1; exit}')"
case "${musl_ver}" in
  musl-1.2.6-*) ;;
  *)
    echo "error: musl ${musl_ver} does not match LICENSE-THIRD-PARTY section 5 (v1.2.6); update the license text" >&2
    exit 1
    ;;
esac

# 同梱エンジンの検査。check-js-engine-isolation.sh は --target を受けずホスト依存のため、
# 配布ターゲット指定の cargo tree で直接判定する（ヒアストリングで pipefail/SIGPIPE の fail-open を避ける）。
tree="$(cargo tree --locked -p fandhe-browser-cli -e normal --target "${target}" \
  ${feat_args[@]+"${feat_args[@]}"} --prefix none --format '{p}')"
if ! grep -q '^fandhe-browser-cli v' <<<"${tree}"; then
  echo "error: cargo tree output has no fandhe-browser-cli root" >&2
  exit 1
fi
if ! grep -q "^${expect} v" <<<"${tree}"; then
  echo "error: expected engine crate ${expect} is missing from the dependency graph" >&2
  exit 1
fi
if grep -q "^${other} v" <<<"${tree}"; then
  echo "error: engine crate ${other} must not be included in this image" >&2
  exit 1
fi
echo "engine check: ${expect} present, ${other} absent"

cargo build --release --locked -p fandhe-browser-cli --bin fandhe-browser \
  --target "${target}" ${feat_args[@]+"${feat_args[@]}"}

mkdir -p /out
cp "target/${target}/release/fandhe-browser" /out/fandhe-browser
mkdir -p /out/data /out/licenses
chown 65532:65532 /out/data
cp NOTICE LICENSE-THIRD-PARTY LICENSE-MIT LICENSE-APACHE /out/licenses/
scripts/gen-cargo-licenses.sh /out/licenses/LICENSE-CARGO-DEPENDENCIES "${target}" "${flags}"
