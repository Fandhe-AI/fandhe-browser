# egress-isolation（Servo ホストの egress 隔離の実測。Linux・macOS・Windows）

Issue #1377（Linux）・#1387（macOS・Windows）・TASK-38・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`。親判断 #1003 の比較設計
`docs/design/servo-host-egress-isolation.md` §6（実測計画）のうち、**依存・`unsafe`・pip を使わずに測れる
Linux の範囲**を測るハーネス。**方式の採否は決めない**。結果は測定条件つきの事実として記録する。

## 何を測るか

許可外の宛先を loopback のダミー listener（127.0.0.2 / 127.0.0.3）に置き、Servo ホストに次の経路を試させる。

| 経路 | 内容 | 観測手段 |
| ---- | ---- | -------- |
| `redirect` | 許可外ホストへの 302 | 外側 listener の受信・内側 listener の受信 |
| `img` / `fetch` | 許可外ホストのサブリソース | 同上 |
| `ws` | `new WebSocket()` | 同上 |
| `internal` | 内部アドレスの代替宛先（127.0.0.3） | 同上 |
| `udp_direct` | 内側ランナー自身の UDP `sendto`（Servo ではなくハーネスのプローブ） | 外側 UDP listener の受信 |
| `internal_real` | 実在の内部アドレス（隔離下の内側だけで試す） | 経路表＋connect だけのプローブ（外部からは観測しない） |
| `dns` | hosts に無い名前の解決（ハーネス自身の `getaddrinfo`） | 経路表＋解決失敗（Servo 自身の DNS・UDP 送信は外から観測しない） |

モード: `baseline`（bwrap。`--unshare-net` だけ無し。陽性対照）・`bwrap-netns`（`--unshare-net`）・
`docker-none`（`docker run --network none`。イメージがあるときだけ）。

判定は `pass` / `fail` / `unobservable`（`judge.py`）。外側の受信が 1 件でもあれば `fail`。`pass` は「外側 0 件・
ベースラインで届いた・内側で試行を観測・guard が監視できた・`--selftest` と loopback の `load` が成功」の全部を要し、
欠ければ `unobservable`（合格とは扱わない。fail-closed）。

## 使い方

```bash
# 自己テスト（偽ホストのみ。bwrap・docker 不要）
make check-egress-isolation

# 実測（要 bwrap。docker-none は --docker-image を足す）
python3 -I -B harness/egress-isolation/measure.py \
  --host /abs/path/to/fandhe-browser-servo-host \
  --modes baseline,bwrap-netns [,docker-none --docker-image <image>] \
  --out "$(mktemp -d)"

# docker-none の準備（glibc をそろえたホストを Debian trixie のイメージ内でビルドして取り出す。#1386）
bash harness/egress-isolation/container/prepare.sh build fandhe-egress-isolation:local
bash harness/egress-isolation/container/prepare.sh extract fandhe-egress-isolation:local "$(mktemp -d)"
# 続けて、表示された取り出し先のバイナリを --host に渡す:
#   measure.py --host <取り出したバイナリ> --modes baseline,bwrap-netns,docker-none \
#     --docker-image fandhe-egress-isolation:local --out "$(mktemp -d)"

# Landlock ABI・netns の作り方ごとの可否・sysctl（読み取りのみ）
python3 -I -B harness/egress-isolation/platform_probe.py [--docker-image <image>]
```

`--out` に `result.json`・`summary.md`・モードごとの内側結果が出る。`make ci` と CI には含めない
（実測は人間の判断材料。後続の候補: `harness-ubuntu` への自己テストのステップ追加）。

### docker-none で glibc をそろえる理由

measure.py は全モードで同一バイナリ（sha256 一致。`binary_match`）を要求する。ホストの新しい glibc でビルドした
バイナリをイメージ（Debian trixie・glibc 2.41）へ COPY すると `GLIBC_2.43` 等を要求して起動できない
（#1377 の再実測 run 2）。そこで `container/Dockerfile` が trixie 内でホストをビルドし、`prepare.sh extract` が
同じファイルを取り出して全モードの `--host` に使う（glibc 2.41 でリンクしたバイナリは新しいホストでも動く）。
イメージは push・配布しない（MPL-2.0 部分を含む）。ベースイメージのダイジェストは
`harness/render-screenshot/container/Dockerfile` と同値で、`test_container.py` がドリフトを検査する。

## 安全上の規則

- 宛先は 127.0.0.0/8 のダミーだけ。実在の内部アドレスへの connect / fetch は、隔離下の内側で `lo` 以外の
  インターフェース・経路が無いことを確かめた後にだけ行う（満たさなければ exit 2 で中止し何も読み込まない）。
- argv は定数リスト・`shell=False`。`--host` は実行可能ファイルの絶対パス、`--docker-image` は正規表現で検証。
- ログ・結果に URL のクエリ・本文・プロキシ系環境変数の値を載せない（有無だけ）。
- ホストへ渡す環境変数は `servo_bbox_driver.build_host_env` の許可リストのみ（`SEC-2`）。UA・ヘッダには触らない。

## 限界

- `dns` / `udp_direct` はハーネス自身のプローブで、Servo 自身の DNS・UDP 送信は外から観測していない
  （root か CAP_NET_ADMIN が要る。承認事項）。
- 送信先の監視は `monitor.py`（`egress_guard` の走査関数を流用し、ハーネスが意図的に接続させるダミー宛先を除外して継続する）。接続済みソケットを 25 ms 間隔で走査するため短命な接続は拾えず、経路ごとに走査が 1 回以上完了していなければ合格にしない。補助の観測手段。
- `dns` は `/etc/resolv.conf` の実体位置に固定の非ループバック nameserver（192.0.2.53）を重ね、その宛先への UDP connect が経路なしで失敗したときだけ試行を観測した扱いにする（`/run` の隠蔽による resolv.conf 欠落と netns 由来の失敗を区別する。区別できなければ unobservable）。docker-none は resolv.conf を差し替えないため、nameserver が得られなければ unobservable になる。
- docker-none の不採用案: render-screenshot のイメージの流用（Chromium の導入で遅く無関係な依存が入る）、
  測定機の OS にそろえたベースイメージへのローカルビルドの COPY（配布コンテナと同じ glibc という目的から外れる）。
- AppArmor の `apparmor_restrict_unprivileged_userns=1` の環境では `unshare -rn` が使えず、bwrap（setuid 相当の
  プロファイル）経由でのみ netns を作れる。ns 内に CAP_NET_ADMIN は無く、veth は作れない。
- 内側の `/etc/hosts` は bind マウント（docker は `--add-host`）。Servo が参照することはベースラインの受信で確認する。

## macOS・Windows（#1387）

`xos-measure.yml` の suite `egress-isolation`（`gh workflow run xos-measure.yml -f suite=egress-isolation`。
macOS・Windows の GitHub ホステッドランナー。結果は #1003 に記録する）か、手元の実機で `measure.py` を実行する。
判定語彙（`pass` / `fail` / `unobservable`）と fail-closed は Linux と同じ。OS ごとのモードは `judge.PLATFORM_MODES`。

| OS | モード | 内容 |
| -- | ------ | ---- |
| macOS | `baseline` / `seatbelt` | baseline は `sandbox-exec -p "(version 1)(allow default)"`（隔離と無関係な失敗の切り分け）。`seatbelt` は `(deny network-outbound (remote ip))` ＋ページ配信用 loopback 1 ポートの許可（`seatbelt.py`） |
| Windows | `baseline` / `appcontainer` | baseline は直接起動。`appcontainer` は capability なしの AppContainer で内側ランナーごと起動する（`appcontainer.py`。#1396。下記） |

観測手段が Linux と異なる点（`method` 列にも出る）:

- プロセス単位の監視（`egress_guard`）が無い。pass の条件から外す代わりに、外側 listener の受信 0 件・baseline の到達・
  試行の観測・`--selftest` と loopback の `load` の成功をすべて要する。
- 異常終了の診断（#1413）。内側の結果 JSON に次を残す。`selftest_rc` が 0 以外のときだけ `selftest_stderr_tail`
  （`--selftest` の stderr の末尾。段階マーカー `selftest: <stage>` と panic メッセージ）、`shutdown` 後にホストを待って
  `host_exit_rc`（待ちが時間切れなら `host_exit_timed_out: true`）、0 以外のときだけ `host_stderr_tail`。どれも上限つきで、
  URL・環境変数の値は載せない。Windows の `0xC0000409`（3221226505）は fail-fast の終了コードで、スタック溢れだけでなく
  Rust の `panic = "abort"` の panic も同じコードになる。判定（`positive_ok`）には使わない。
- 試行の観測（#1395）。macOS・Windows の TCP 経路の試行は次の順で根拠にする。いずれも ip:port と件数だけを記録する。
  - 失敗経路のビーコン（補助情報のみ。img・fetch・ws・internal）: 同じページが、許可外宛先への要求が失敗したとき
    （`onerror` / `catch`）だけ許可オリジンへ `/beacon/<route>` を送り、結果の `beacons` に件数を記録する。
    ブラウザ内部が要求を拒否した場合も発火するため、接続 syscall の試行や Seatbelt による遮断の証拠にならず、
    `attempts` の根拠にも判定にも使わない（ビーコンだけでは `unobservable`）。Linux のページは変えない。
  - サンドボックス拒否ログ（TCP 経路の唯一の根拠。`log show`）: 取りこぼしの原因を切り分けるため、診断カウンタ（読んだ行・JSON 行・拒否行・
    宛先を取れた行・pid の帰属で除外した行・帰属した行。`deny_log.diagnostics`）を記録する。あわせて形式の確認用に、対象 pid に帰属する `deny(` 行で正規表現に一致しなかったものの「形」だけを最大 16 件（プロセス名・pid・パス・外部 IP・クエリは `<proc>`・`<pid>`・`<path>`・`<ipv4>`・`<redacted>` に置換。127.0.0.0/8 の宛先は実値）と、pid 帰属で除外した行の `{pid, dest}` を最大 8 件（本文は保存しない。127.0.0.0/8 以外の dest は `<ipv4>:port`）記録する。無関係なプロセスの不一致行は件数（`unmatched_deny_unattributed`）だけ。内側ランナー自身の UDP
    プローブが拒否されたのに拒否ログに無いときは自己検査が不成立（`deny_log.available=false`）となり、拒否ログを根拠にしない。
    macOS 26 の拒否ログは宛先アドレスを `*` で出す形式（`deny(1) network-outbound <語>:*:<port>`。実測 xos-measure run 41305443526）もある。
    既存の IPv4 形式は変えず、この形式は「pid が対象 pid に含まれ、かつポートがこの run の既知の宛先ポート集合で一意」のときだけその宛先へ帰属させる
    （IPv4 形式より厳しい結合条件。判定ロジックは変えない）。既知集合に無いポートは `wildcard_unknown_port`、複数の宛先が共有するポートは帰属させず
    （unobservable 側）`wildcard_ambiguous_port`・`wildcard_ambiguous_ports` に残す。宛先ごとの一致形式は `deny_log.diagnostics.match_kind`
    （`ip:port` / `*:port` の件数）、この形式で観測した経路は `deny_log.wildcard_routes` と verdict の `method` に出る。
    形のサンプルは `network-outbound` 直後の宛先トークン末尾のポート数字（1〜5 桁）と `*` を保持する（ポートは記録範囲の ip:port の内）。英字語は `a` 形のまま。
  - 拒否ログが使えない（自己検査の不成立・取りこぼし）TCP 経路は、ビーコンがあっても `unobservable`（理由は結果の `reason`）。
    したがって「Servo の事前ブロックではなく遮断された」ことは拒否ログの試行でのみ示す。
  - `udp_direct` は `sendto` が拒否された errno。
  - Servo ホスト側の接続試行ログ（`egress_guard` 相当）は採らない。`egress_guard` は `/proc/net` を走査する Linux 専用で、
    macOS では connect が syscall で拒否されソケットが現れない。`servo-host/src` に接続試行のログも無く、追加には Servo の
    net スタックの改変か servo-host の改修が要る。
  - macOS は sudo なしの `log show --style ndjson`（非特権）で拒否行を取る（#1402。xos-measure の実測 run 41309744793・41310673642・41313641135 で、
    sudo・`log config` なしでも上記 macOS 26 のワイルドカード形式の拒否行が取れ、TCP 4 経路（img・fetch・ws・internal）が pass〔redirect は unobservable〕・自己検査成立。オーナー判断 2026-11-01 で
    sudo つき拒否ログの opt-in と `log config` の操作を削除した）。UDP の自己照合が成り立てば TCP 5 経路の deny 行が試行の根拠になり、
    成り立たなければ理由（`deny log unavailable: ...`）つきの `unobservable`。`log stream`・`log erase` など管理者操作は実装しない。
  - 後続の候補（今回は入れない）: SBPL の `(with message ...)` 等の修飾子、`log stream` の並行収集。
- 共有 loopback で内側の写し listener を立てられない。hosts も変えないため、ページは IP リテラル（127.0.0.x）で配る。
- `internal_real` / `dns` は netns 前提の構造検査で、Seatbelt・AppContainer では成り立たないため `unobservable`
  （実在の内部アドレスにも向けない）。
- macOS は lo0 に 127.0.0.2 / 127.0.0.3 の alias が要る（`sudo ifconfig lo0 alias 127.0.0.2 up`）。ハーネスは sudo を使わず、
  bind できなければ exit 2 で案内する。workflow は使い捨て runner で固定 argv の alias 追加を行う。
- Windows の `appcontainer`（#1396。オーナー承認 2026-11-01: `harness/egress-isolation/` の Python 計測ハーネスに限る ctypes の Win32 FFI）:
  - `appcontainer.py` が `CreateAppContainerProfile`・`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`・`CreateProcessW`・Job
    （`KILL_ON_JOB_CLOSE`）で内側ランナー（`in_sandbox.py`。Servo ホストは子プロセス）を起動する。標準ライブラリの ctypes のみで、
    本体 crate（Rust）への FFI・`unsafe`・依存の追加は行わない。管理者権限が要る操作は、次項の #1401 の承認範囲（`xos-measure.yml` の Windows ジョブでの opt-in）を除き行わない。
  - 容器から読めるのは AppContainer 専用フォルダだけなので、Python ランタイム・ハーネス・ホスト（exe と同階層の DLL）をそこへ複製する
    （toolcache・チェックアウト・`D:\sr` の ACL は変えない）。専用フォルダが取れないときだけ一時ディレクトリに `icacls` で SID を付与し、
    終了時に除去する。複製したホストの SHA256 を記録し、baseline と同一性を比べる。
  - プロファイル・ハンドル・属性リスト・SID・複製先は成功・失敗・タイムアウトのいずれでも `try/finally` で片付け、各段の成否を
    結果の `appcontainer.cleanup` に記録する。片付けが不完全なら pass にしない。環境変数は許可リスト（`SystemRoot`・`TMP`・`TEMP`・
    `PYTHONIOENCODING`）のみ。
  - loopback の切り分け: AppContainer は既定で loopback に出られない。外側 listener の受信 0 件が egress 遮断か loopback 遮断かを区別するため、
    Servo を使わない preflight（`appcontainer_probe.py`）を capability なし（`nocap`）とネットワーク capability あり（`netcap`:
    `internetClient`＋`privateNetworkClientServer`）で実行し、容器内の loopback 自己接続と、preflight 専用の外側 listener（127.0.0.2）への
    到達を記録する。`netcap` でも外側に届かなければ loopback 遮断と egress 遮断を区別できず、listener 経路は理由つきの `unobservable`
    （`judge.appcontainer_confound`）。preflight が無い・不正な場合も同じ（fail-closed）。
  - 宛先は 127.0.0.0/8 のダミーだけ。TCP 5 経路は試行の根拠（写し listener・拒否ログ）が無いため `unobservable` のまま。`udp_direct` の
    errno は生の値を `probes.udp_direct` に記録するだけで、Windows の拒否 errno は実測前に判定へ入れない
    （`in_sandbox.APPCONTAINER_DENY_ERRNOS` は空。観測した値を根拠に追加する）。
  - ループバック免除（#1401。オーナー承認 2026-11-01: `xos-measure.yml` の Windows ジョブ〔使い捨て runner〕に限り、ハーネスが作った
    AppContainer の SID だけに `CheckNetIsolation LoopbackExempt` を付ける。opt-in で、計測後に必ず解除）: `measure.py --appcontainer-loopback-exempt`
    （Windows の `appcontainer` モードのときだけ受け付ける。既定・ローカル・`ci.yml` では呼ばない）。プロファイル作成の直後・最初の起動の前に
    付与し、片付けではプロファイル削除より前に解除する（付与に失敗しても解除を試みる）。解除の成否と、解除後に一覧へ SID が無いことを
    `appcontainer.cleanup` の `loopback_exempt_removed`・`loopback_exempt_absent_after_remove` に記録し、解除に失敗すれば pass にしない。
    交絡の判定は 3 通り: (1) `nocap` が外側に届いた場合は loopback 宛ての listener では egress を測れないとして listener 経路を理由つきの
    `unobservable`（外側の受信由来の fail も `unobservable` に変え、件数は `outer_count` に残す）、(2) `nocap` 0 件・`netcap` 1 件以上なら交絡なし、
    (3) 両方 0 件なら免除ありでも区別できず `unobservable`。
  - 免除で測れない場合の代替案（未検証。採否はオーナー判断）:

    | 案 | 内容 | 懸念 |
    | -- | ---- | ---- |
    | (a) runner 自身の NIC アドレス | loopback 以外の自機アドレスの listener | 宛先を 127.0.0.0/8 以外に向けない安全規則に反する。隔離が自機の全アドレスに及ぶ可能性も未確認 |
    | (b) 別ホスト・別 VM の listener | ジョブ間のネットワーク設計が要る | 実在アドレスへのプローブになる |
    | (c) WFP の監査（`auditpol` の Filtering Platform Connection 等） | 試行そのものを観測する | 本承認の範囲外の管理者操作 |

- Windows の runner は管理者で動くため、「非管理者で loopback 例外を作れるか」には答えられない（設計 §6.2）。

## 承認事項（この issue では実装しない）

Landlock ルールの適用（`landlock` クレートまたは `unsafe`）・seccomp-bpf・許可リスト付きプロキシと netns 内への中継・
Servo 自身の DNS/UDP の外部観測・Windows の TCP 経路の試行の観測手段（WFP の監査等）・古いカーネルでの再測定。
（`CheckNetIsolation LoopbackExempt` は #1401 で承認済みの opt-in〔xos-measure の Windows のみ〕として実装済み。Windows の AppContainer ランチャー〔ctypes〕は #1396 でオーナー承認のうえ実装済み。範囲はこのディレクトリの Python ハーネスに限る。）
