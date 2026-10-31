"""docker-none 用イメージ定義（container/Dockerfile・prepare.sh）のドリフト検査（Issue #1386・TASK-38・RENDER-1）。

docker は使わない静的検査。glibc をそろえる不変条件（build と runtime が同じ Debian 系統・
ダイジェスト固定・render-screenshot と同値）と、measure.py との契約（配置先・タグ正規表現）を確かめる。
"""
import re
import sys
import unittest
from pathlib import Path

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
import measure  # noqa: E402

_DOCKERFILE = _HERE / "container" / "Dockerfile"
_PREPARE = _HERE / "container" / "prepare.sh"
_RENDER_DOCKERFILE = _HERE.parent / "render-screenshot" / "container" / "Dockerfile"
_FROM_RE = re.compile(r"^FROM\s+(\S+)\s+AS\s+(\S+)\s*$", re.MULTILINE)


def _stages(path: Path) -> dict[str, str]:
    return {name: image for image, name in _FROM_RE.findall(path.read_text(encoding="utf-8"))}


class ContainerDriftTest(unittest.TestCase):
    def test_both_stages_pinned_by_digest(self) -> None:
        stages = _stages(_DOCKERFILE)
        self.assertEqual(set(stages), {"build", "runtime"})
        for image in stages.values():
            self.assertRegex(image, r"@sha256:[0-9a-f]{64}$")

    def test_stages_share_debian_trixie(self) -> None:
        stages = _stages(_DOCKERFILE)
        self.assertTrue(stages["build"].startswith("rust:1-slim-trixie@"))
        self.assertTrue(stages["runtime"].startswith("debian:trixie-slim@"))

    def test_digests_match_render_screenshot(self) -> None:
        render = _stages(_RENDER_DOCKERFILE)
        ours = _stages(_DOCKERFILE)
        self.assertEqual(ours["build"], render["build"])
        self.assertEqual(ours["runtime"], render["runtime"])

    def test_host_path_matches_measure(self) -> None:
        text = _DOCKERFILE.read_text(encoding="utf-8")
        self.assertIn(f"{measure.DOCKER_HOST_PATH}\n", text)
        self.assertIn(f'HOST_IN_IMAGE="{measure.DOCKER_HOST_PATH}"', _PREPARE.read_text(encoding="utf-8"))

    def test_tag_regex_matches_measure(self) -> None:
        m = re.search(r"^readonly TAG_RE='(.+)'$", _PREPARE.read_text(encoding="utf-8"), re.MULTILINE)
        self.assertIsNotNone(m)
        self.assertEqual(m.group(1), measure.DOCKER_IMAGE_RE.pattern)

    def test_host_comes_from_build_stage(self) -> None:
        copies = [ln for ln in _DOCKERFILE.read_text(encoding="utf-8").splitlines() if ln.startswith("COPY ")]
        host_copies = [ln for ln in copies if "fandhe-browser-servo-host" in ln]
        self.assertEqual(len(host_copies), 1)
        self.assertIn("--from=build", host_copies[0])


if __name__ == "__main__":
    unittest.main()
