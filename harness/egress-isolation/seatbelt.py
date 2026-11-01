#!/usr/bin/env python3
"""macOS Seatbelt（`sandbox-exec`）の隔離プロファイルと、拒否ログの解析（純粋関数中心）。

役割・責務境界:
    Issue #1387（TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`）。`measure.py` が macOS の `seatbelt` モードで
    内側ランナーを `sandbox-exec -p <profile>` から起動するための固定プロファイルを作り、
    サンドボックスの拒否ログ（`log show`）から「試行の観測」（宛先 ip:port と件数だけ）を取り出す。
    macOS には netns もプロセス単位の egress_guard も無く、共有 loopback のため内側の写し listener も立てられない。
    そのため試行の根拠は拒否ログに置く。ログが読めない・0 件なら試行は未観測（unobservable。合格にしない）。

呼び出し文脈: `measure.py`（プロファイル生成・ログ収集）、`test_seatbelt.py`（自己テスト）。
安全上の規則: プロファイルに置換するのは検証済みの int ポートだけ。`log` の述語は固定文字列。
URL・本文は扱わない（ip:port と件数のみ）。方式の採否は決めない（#1003 の判断）。
"""

from __future__ import annotations

import json
import re
import subprocess
from collections import Counter
from dataclasses import asdict, dataclass
from typing import Any

SANDBOX_EXEC = "/usr/bin/sandbox-exec"
LOG_BIN = "/usr/bin/log"
# Issue #1402: xos-measure の macOS ジョブ（使い捨て runner）に限り、opt-in で `sudo log show` / `sudo log config` を使う。
SUDO_BIN = "/usr/bin/sudo"
LOG_CONFIG_SUBSYSTEMS = ("com.apple.sandbox", "com.apple.sandbox.reporting")
LOG_CONFIG_ACTIONS = ("status", "raise", "restore_default", "restore_info")
# 上げる・戻す操作はレベルだけを変える（persist・signpost・oversize には触れない。Issue #1402）。値は定数に限る。
LOG_CONFIG_MODE = "level:debug"
LOG_CONFIG_RESTORE_DEFAULT_MODE = "level:default"
LOG_CONFIG_RESTORE_INFO_MODE = "level:info"
# `--reset` は persist・signpost・oversize などレベル以外のカスタム設定も消すため使わない（action に含めない）。
# 隔離と無関係な失敗（GPU・WindowServer 等）を切り分ける陽性対照用の全許可プロファイル。
BASELINE_PROFILE = "(version 1)(allow default)"
# 隔離プロファイル。許可するのはページ配信用の loopback 1 ポートだけ（ホストが許可オリジンを読む経路）。
_ISOLATED_TEMPLATE = (
    '(version 1)(allow default)(deny network-outbound (remote ip))'
    '(allow network-outbound (remote ip "localhost:{port}"))'
)
# macOS 26 で拒否が `sender == "Sandbox"` に出ない可能性があるため reporting subsystem も拾う（Issue #1395）。
# 最終的な絞り込みは正規表現と pid の帰属で行う。固定文字列で外部入力を連結しない。
LOG_PREDICATE = 'sender == "Sandbox" OR subsystem == "com.apple.sandbox.reporting"'
# サンドボックスが送信を拒否したときの errno 名（`in_sandbox.py` の UDP プローブと拒否ログの自己検査が共有する）。
SANDBOX_DENY_ERRNOS = frozenset({"EPERM", "EACCES"})
MAX_LOG_LINES = 20000
MAX_LINE_LEN = 4096
# `log show` を逐次読む際の打ち切り上限（読み取った行数・総バイト数）。超えたら収集を諦める（unobservable）。
MAX_SCAN_LINES = 500000
MAX_SCAN_BYTES = 64 * 1024 * 1024
_DENY_RE = re.compile(r"(?:\((\d+)\)\s+)?deny\(\d+\) network-outbound (\d{1,3}(?:\.\d{1,3}){3}):(\d{1,5})\b")


def isolated_profile(page_port: object) -> str:
    """隔離プロファイル。`page_port` は 1024〜65535 の int のみ（それ以外は ValueError）。"""
    if isinstance(page_port, bool) or not isinstance(page_port, int) or not (1024 <= page_port <= 65535):
        raise ValueError("page_port must be an int in [1024, 65535]")
    return _ISOLATED_TEMPLATE.format(port=page_port)


def log_show_argv(start: str, *, sudo: bool = False) -> list[str]:
    """`log show` の固定 argv。`start` は `YYYY-MM-DD HH:MM:SS` 形式のみ受け付ける。

    `sudo=True`（opt-in。Issue #1402）は `sudo -n` つきで `--info --debug` も付ける。パスワード要求では止まらず失敗させる。
    """
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}", start):
        raise ValueError("start must be 'YYYY-MM-DD HH:MM:SS'")
    if sudo:
        return [SUDO_BIN, "-n", LOG_BIN, "show", "--style", "ndjson", "--info", "--debug",
                "--start", start, "--predicate", LOG_PREDICATE]
    return [LOG_BIN, "show", "--style", "ndjson", "--start", start, "--predicate", LOG_PREDICATE]


def log_config_argv(action: str, subsystem: str) -> list[str]:
    """`sudo log config` の固定 argv（Issue #1402 の承認範囲。許可リストの action・subsystem だけ）。"""
    if action not in LOG_CONFIG_ACTIONS:
        raise ValueError("unknown log config action")
    if subsystem not in LOG_CONFIG_SUBSYSTEMS:
        raise ValueError("unknown log config subsystem")
    base = [SUDO_BIN, "-n", LOG_BIN, "config"]
    if action == "status":
        return [*base, "--status", "--subsystem", subsystem]
    if action == "raise":
        return [*base, "--subsystem", subsystem, "--mode", LOG_CONFIG_MODE]
    if action == "restore_info":
        return [*base, "--subsystem", subsystem, "--mode", LOG_CONFIG_RESTORE_INFO_MODE]
    # 残りは restore_default（許可リストで検証済み）。
    return [*base, "--subsystem", subsystem, "--mode", LOG_CONFIG_RESTORE_DEFAULT_MODE]


def _default_runner(argv: list[str]) -> tuple[int, str]:
    """固定 argv を `shell=False`・stdin なし・timeout つきで実行する（出力は先頭の上限つき）。"""
    try:
        cp = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, timeout=60,  # noqa: S603
                            check=False)
    except (OSError, subprocess.SubprocessError):
        return 127, ""
    return cp.returncode, (cp.stdout + cp.stderr).decode("utf-8", errors="replace")[:400]


def _line_kind(ln: str) -> str | None:
    """小文字化済みの 1 行を "default" / "info" / None（変更不可・解釈不能）に分類する。"""
    pairs = re.findall(r"([a-z_-]+)\s*[=:]\s*([a-z_-]+)", ln)
    if pairs:
        return "default" if all(v == "default" for _, v in pairs) else None
    rest = re.sub(r"^mode\s+for\s+('[^']*'|\S+)\s*:?\s*", "", ln).strip()
    if rest == "default":
        return "default"
    tokens = rest.split()
    if "persist_default" not in tokens or any(t.startswith("persist_") and t != "persist_default" for t in tokens):
        return None
    levels = {t for t in tokens if t in ("default", "info", "debug", "off")}
    if levels == {"default"}:
        return "default"
    if levels == {"info"}:
        return "info"
    return None


def before_state_kind(status: str) -> str | None:
    """`log config --status` の出力から、変更前の状態と戻し方を決める（fail-closed。Issue #1402）。

    - "default": 全行が既定と読める。戻しは `--mode level:default`（`--reset` は使わない）。key=value 形式は全値 default、実機（macOS 26）の
      空白区切りトークン形式 `Mode for '<s>'  DEFAULT PERSIST_DEFAULT ...` はレベルが DEFAULT のとき。
    - "info": 全行がトークン形式でレベル INFO かつ `PERSIST_DEFAULT`。戻しは `--mode level:info` でレベルだけ
      元へ戻す。いずれも `--reset` は使わない（persist・`SIGNPOST_*`・`OVERSIZE_*` などレベル以外のカスタム設定を
      消してしまうため）。レベルだけを変えるので、それらは残る。判定にも使わない。
    - None: DEBUG・OFF、`PERSIST_DEFAULT` 以外、行ごとに種別が異なる、空・解釈不能。変更しない。
    残る差: 明示設定の INFO と plist 由来の INFO は status 上同じに見える。どちらもレベルだけを INFO へ戻すため
    レベルは復元されるが、plist 由来のときの「設定なし」状態そのものとは区別できない。最終的な安全網は
    復元後の `--status` 全体を変更前と比べる確認（不一致は all_restored=False で終了コード 3。#1405）。
    """
    kinds = {_line_kind(ln.strip().lower()) for ln in status.splitlines() if ln.strip()}
    if len(kinds) != 1:
        return None
    return kinds.pop()


def status_is_all_default(status: str) -> bool:
    """全行が既定と読めるときだけ True（`before_state_kind` が "default"。INFO は含まない）。"""
    return before_state_kind(status) == "default"


class SandboxLogConfig:
    """sandbox 関連 subsystem のログレベルを上げ、終了時（例外時も）必ず元へ戻す context manager（Issue #1402）。

    `measure.py` の seatbelt 分岐が、内側の実行より前に enter し、`log show` の後に exit する。
    変更前が "default" か "info"（`before_state_kind`）と確認できないとき（カスタム設定・status 取得失敗）は
    何も変更せず中止する（`aborted`）。上げる操作も戻す操作もレベルだけを変える（`level:debug` /
    変更前が default なら `level:default`、info なら `level:info`）。`--reset` は使わない（レベル以外の
    カスタム設定を消すため）。いずれも戻した後に `--status` を取り、
    変更前と全体一致したときだけ restored=True にする。
    `runner` は自己テストで差し替える（argv -> (rc, 出力)）。
    """

    def __init__(self, runner: Any = None) -> None:
        self._run = runner or _default_runner
        self.state: dict[str, Any] = {"subsystems": {}, "all_restored": False}

    def __enter__(self) -> "SandboxLogConfig":
        for sub in LOG_CONFIG_SUBSYSTEMS:
            self.state["subsystems"][sub] = {}
        for sub in LOG_CONFIG_SUBSYSTEMS:
            try:
                rc, out = self._run(log_config_argv("status", sub))
            except Exception as err:  # noqa: BLE001 - 取得不能は変更せず中止する
                rc, out = None, ""
                self.state["subsystems"][sub]["status_before_error"] = type(err).__name__
            self.state["subsystems"][sub].update(status_before_rc=rc, status_before=out)
        # 変更前の設定を確実に既定と確認できたときだけ変更する。レベルだけを変える前提のため、
        # 既定以外・status 取得失敗は何も変更せず中止する（復元不能な変更をしない。Issue #1402）。
        bad = [sub for sub in LOG_CONFIG_SUBSYSTEMS
               if self.state["subsystems"][sub].get("status_before_rc") != 0
               or before_state_kind(str(self.state["subsystems"][sub].get("status_before", ""))) is None]
        for sub in LOG_CONFIG_SUBSYSTEMS:
            self.state["subsystems"][sub]["before_kind"] = before_state_kind(
                str(self.state["subsystems"][sub].get("status_before", "")))
        if bad:
            self.state["aborted"] = "log config before-state is not default or unreadable; no change made"
            return self
        for sub in LOG_CONFIG_SUBSYSTEMS:
            self.state["subsystems"][sub]["raise_attempted"] = True
            try:
                rc, _ = self._run(log_config_argv("raise", sub))
            except Exception as err:  # noqa: BLE001 - 残りは変更せず exit で戻す
                rc = None
                self.state["subsystems"][sub]["raise_error"] = type(err).__name__
            self.state["subsystems"][sub]["raise_rc"] = rc
        return self

    def __exit__(self, *exc: object) -> None:
        if self.state.get("aborted"):
            # 何も変更していないので戻す対象もない。ただし拒否ログは昇格できていないため all_restored は False のまま。
            return
        for sub in LOG_CONFIG_SUBSYSTEMS:
            entry = self.state["subsystems"].setdefault(sub, {})
            action = "restore_info" if entry.get("before_kind") == "info" else "restore_default"
            try:
                entry["restore_rc"] = self._run(log_config_argv(action, sub))[0]
            except Exception as err:  # noqa: BLE001 - 残りの subsystem の復元を続けるため
                entry["restore_rc"] = None
                entry["restore_error"] = type(err).__name__
        for sub in LOG_CONFIG_SUBSYSTEMS:
            entry = self.state["subsystems"][sub]
            try:
                rc, out = self._run(log_config_argv("status", sub))
            except Exception as err:  # noqa: BLE001
                rc, out = None, ""
                entry["status_after_error"] = type(err).__name__
            entry.update(status_after_rc=rc, status_after=out)
            entry["restored"] = (entry.get("restore_rc") == 0 and rc == 0 and out == entry.get("status_before")
                                 and entry.get("status_before_rc") == 0)
        self.state["all_restored"] = all(e.get("restored") is True for e in self.state["subsystems"].values())


def parse_deny_line(line: str, pids: frozenset[int] | None = None) -> str | None:
    """1 行から拒否の宛先 "ip:port" を取り出す。該当しない・長すぎる行は None。

    `pids` を渡すと、拒否ログの `proc(<pid>)` が集合に含まれる行だけを採る（対象の Servo ホストとその子孫に
    帰属する拒否に限る。pid が読めない行も除外する）。None は帰属を問わない（自己テスト・後方互換）。
    """
    if len(line) > MAX_LINE_LEN:
        return None
    m = _DENY_RE.search(line)
    if m and int(m.group(3)) <= 65535:
        if pids is not None and (m.group(1) is None or int(m.group(1)) not in pids):
            return None
        return f"{m.group(2)}:{int(m.group(3))}"
    return None


def parse_denies(text: str, pids: frozenset[int] | None = None) -> Counter[str]:
    """ndjson 本文から network-outbound の拒否を数え、`"ip:port" -> 件数` を返す。

    行数・1 行の長さに上限を設ける。不正な行・拒否以外の行は無視する。ip:port 以外は記録しない
    （プロセス名・パス・URL を結果に載せない）。
    """
    counts: Counter[str] = Counter()
    for i, line in enumerate(text.splitlines()):
        if i >= MAX_LOG_LINES:
            break
        dest = parse_deny_line(line, pids)
        if dest:
            counts[dest] += 1
    return counts


def attempts_from_denies(denies: Counter[str], dest_by_route: dict[str, str]) -> dict[str, bool]:
    """経路ごとの宛先 ("ip:port") が拒否ログに 1 件以上あれば試行ありとする。"""
    return {route: denies.get(dest, 0) >= 1 for route, dest in dest_by_route.items()}


@dataclass
class DenyDiagnostics:
    """拒否ログの取りこぼしの原因を段ごとに切り分ける診断カウンタ（件数だけ。Issue #1395）。

    `scanned_lines` 読んだ行、`json_lines` JSON として解析できた行、`sandbox_lines` 拒否（`deny(`）を含む行、
    `network_outbound_lines` うち宛先まで取り出せた行、`deny_excluded_by_pid` pid の帰属で除外した行、
    `deny_attributed` 対象 pid に帰属して数えた行。プロセス名・パス・URL は保持しない。
    """

    scanned_lines: int = 0
    json_lines: int = 0
    sandbox_lines: int = 0
    network_outbound_lines: int = 0
    deny_excluded_by_pid: int = 0
    deny_attributed: int = 0

    def as_dict(self) -> dict[str, int]:
        return asdict(self)


def _record_message(line: str, diag: DenyDiagnostics) -> tuple[str, int]:
    """ndjson 1 行から (拒否メッセージ本文, レコードの processID)。JSON でなければ行そのものと 0。

    kernel 経由のサンドボックスメッセージではレコードの `processID` が 0 になりうるため、pid は本文の
    `name(pid)` を優先し、レコード側は 0 以外のときだけ補助に使う。
    """
    if line.startswith("{"):
        try:
            obj: Any = json.loads(line)
        except ValueError:
            obj = None
        if isinstance(obj, dict):
            diag.json_lines += 1
            msg = obj.get("eventMessage")
            pid = obj.get("processID")
            return (msg if isinstance(msg, str) else "",
                    pid if isinstance(pid, int) and not isinstance(pid, bool) and pid > 0 else 0)
    return line, 0


def scan_line(line: str, pids: frozenset[int] | None, diag: DenyDiagnostics) -> str | None:
    """1 行を解析して診断を更新し、対象 pid に帰属する拒否の "ip:port" を返す（該当しなければ None）。

    `measure.collect_denies` が逐次読みの各行に対して呼ぶ。`pids` が None なら帰属を問わない（自己テスト）。
    """
    diag.scanned_lines += 1
    if len(line) > MAX_LINE_LEN:
        return None
    msg, record_pid = _record_message(line, diag)
    if "deny(" in msg:
        diag.sandbox_lines += 1
    m = _DENY_RE.search(msg)
    if not m or int(m.group(3)) > 65535:
        return None
    diag.network_outbound_lines += 1
    pid = int(m.group(1)) if m.group(1) is not None else record_pid or None
    if pids is not None and (pid is None or pid not in pids):
        diag.deny_excluded_by_pid += 1
        return None
    diag.deny_attributed += 1
    return f"{m.group(2)}:{int(m.group(3))}"


def self_check(denies: dict[str, int] | Counter[str], udp_probe: object, udp_dest: str) -> tuple[bool, str]:
    """内側ランナー自身の UDP プローブが拒否された（確実に拒否が起きた）のに、その宛先の拒否が 0 件なら不成立。

    拒否ログの収集・解析が、正解の分かっている拒否も取りこぼしていることを示す。不成立のときは拒否ログを
    TCP 経路の試行の根拠にしない（fail-closed）。UDP が拒否されていなければ検査できないので成立扱い。
    """
    if udp_probe in SANDBOX_DENY_ERRNOS and denies.get(udp_dest, 0) < 1:
        return False, "self-probe denial missing from deny log"
    return True, "ok"


def compose_attempts(
    attempts: dict[str, bool], evidence: dict[str, str], denies: dict[str, int] | Counter[str],
    dest_by_route: dict[str, str], udp_dest: str,
) -> tuple[dict[str, bool], dict[str, str]]:
    """内側ランナーの試行（errno）と拒否ログの試行を OR で合成する。

    拒否ログは既存の根拠を上書きしない（`False` で消さない）。`denies` は根拠に使える場合だけ渡す
    （自己検査が不成立なら空を渡す）。根拠の出どころは `evidence`（経路 -> "errno" / "deny_log" の `+` 連結）。
    """
    out_attempts = dict(attempts)
    out_evidence = dict(evidence)

    def mark(route: str) -> None:
        out_attempts[route] = True
        out_evidence[route] = "+".join(filter(None, [out_evidence.get(route), "deny_log"]))

    for route, seen in attempts_from_denies(Counter(denies), dest_by_route).items():
        out_attempts[route] = bool(out_attempts.get(route))
        if seen:
            mark(route)
    if denies.get(udp_dest, 0) >= 1:
        mark("udp_direct")
    out_attempts["udp_direct"] = bool(out_attempts.get("udp_direct"))
    return out_attempts, out_evidence
