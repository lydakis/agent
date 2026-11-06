"""Pin Sonny and its daemon to one main commit, without a release tag."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re


VERSION = r"[1-9][0-9]*\.[1-9][0-9]*"


def render_formula(commit: str, version: str) -> str:
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("expected a full commit SHA")
    if not re.fullmatch(VERSION, version):
        raise ValueError("expected a nightly workflow version such as 12.1")
    return f'''class SonnyNightly < Formula
  desc "Nightly terminal coordinator with its matching agent daemon"
  homepage "https://github.com/lydakis/agent"
  url "https://github.com/lydakis/agent.git", revision: "{commit}"
  version "{version}"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/lydakis/agent.git", branch: "main"

  livecheck do
    skip "Updated by the nightly workflow"
  end

  depends_on "rust" => :build
  depends_on "agent-browser"
  depends_on "ripgrep"

  on_linux do
    depends_on "bubblewrap"
  end

  conflicts_with "sonny", because: "both install the sonny command"

  def install
    ENV["AGENT_BWRAP"] = (formula_opt_bin("bubblewrap")/"bwrap").to_s if OS.linux?
    system "cargo", "install", *std_cargo_args(root: libexec)
    libexec.install libexec/"bin/agent"
    system "cargo", "install", *std_cargo_args(path: "sonny")
  end

  test do
    sonny = shell_output("#{{bin}}/sonny --version").strip
    agent = shell_output("#{{bin}}/sonny agent --version").strip
    assert_match(/^sonny [0-9][0-9A-Za-z.+-]* \\([0-9a-f]{{9,40}}\\)$/, sonny)
    assert_match(/^agent-runtime [0-9][0-9A-Za-z.+-]* \\([0-9a-f]{{9,40}}\\)$/, agent)
    assert_equal sonny[/\\(([0-9a-f]+)\\)$/, 1], agent[/\\(([0-9a-f]+)\\)$/, 1]
    assert_match "({commit[:9]})", sonny unless build.head?
  end
end
'''


def prepare_update(commit: str, version: str, current: Path):
    formula = render_formula(commit, version).encode()
    request = {
        "message": f"Update sonny-nightly to {commit[:9]} ({version})",
        "branch": "main",
        "content": base64.b64encode(formula).decode(),
    }
    if current.exists():
        previous = current.read_bytes()
        versions = re.findall(r'^  version "(' + VERSION + r')"$', previous.decode(), re.MULTILINE)
        commits = re.findall(r'revision: "([0-9a-f]{40})"$', previous.decode(), re.MULTILINE)
        if len(versions) != 1 or len(commits) != 1:
            raise ValueError("current nightly must have one pinned commit and version")
        before, after = (tuple(map(int, value.split("."))) for value in (versions[0], version))
        if before > after or commits[0] == commit:
            return None
        if before == after:
            raise ValueError("refusing to replace a different commit with the same version")
        request["sha"] = hashlib.sha1(
            b"blob " + str(len(previous)).encode() + b"\0" + previous
        ).hexdigest()
    current.parent.mkdir(parents=True, exist_ok=True)
    current.write_bytes(formula)
    return request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("commit")
    parser.add_argument("version")
    parser.add_argument("output", type=Path)
    parser.add_argument("--request", type=Path, help="prepare a tap update instead of only rendering")
    args = parser.parse_args()
    try:
        if args.request:
            args.request.unlink(missing_ok=True)
            request = prepare_update(args.commit, args.version, args.output)
            if request:
                args.request.write_text(json.dumps(request) + "\n")
        else:
            args.output.write_text(render_formula(args.commit, args.version))
    except (ValueError, OSError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
