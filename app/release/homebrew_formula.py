"""Generate the Homebrew formula for Sonny, the terminal client, from one
release archive: Sonny and the daemon it starts, both universal binaries."""

import argparse
import hashlib
from pathlib import Path
import re

from homebrew_cask import VERSION


def archive_name(version: str) -> str:
    return f"sonny-{version}.tar.gz"


def render_formula(version: str, archive: Path) -> str:
    if not re.fullmatch(VERSION, version):
        raise ValueError("expected a release version such as 0.1.0 or 0.1.0-rc.1")
    if archive.name != archive_name(version):
        raise ValueError("Sonny archive name must match the release version")
    sha256 = hashlib.sha256(archive.read_bytes()).hexdigest()
    # Homebrew reads the version from the archive's name. A formula's
    # download is not quarantined, so the binaries need no notarization.
    return f'''class Sonny < Formula
  desc "Terminal coordinator for a daemon that hosts many durable AI agents"
  homepage "https://github.com/lydakis/agent"
  url "https://github.com/lydakis/agent/releases/download/v{version}/{archive_name(version)}"
  sha256 "{sha256}"
  license any_of: ["MIT", "Apache-2.0"]

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on :macos

  def install
    # Sonny starts this daemon when none runs. It stays off PATH, where
    # Agent.app's `agent` (or another program's) may already be. Sonny
    # starts it by its `opt` path: the daemon outlives Sonny and hands its
    # own path to agents as AGENT_BIN, and that path must outlast the
    # version an upgrade and `brew cleanup` remove.
    libexec.install "bin/sonny", "libexec/agent"
    (bin/"sonny").write_env_script libexec/"sonny", AGENT_BIN: opt_libexec/"agent"
  end

  test do
    assert_equal "sonny #{{version}}", shell_output("#{{bin}}/sonny --version").strip
    assert_equal "agent-runtime #{{version}}", shell_output("#{{libexec}}/agent --version").strip
  end
end
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("archive", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        formula = render_formula(args.version, args.archive)
    except (ValueError, OSError) as error:
        parser.error(str(error))
    args.output.write_text(formula)


if __name__ == "__main__":
    main()
