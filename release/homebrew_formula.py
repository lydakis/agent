"""Generate the Homebrew formulas for the daemon (`agent`) and Sonny, the
terminal client, from one release's source archive. Both build on the
person's machine, so nothing needs signing or notarizing."""

import argparse
import hashlib
from pathlib import Path
import re


VERSION = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?"


def archive_name(version: str) -> str:
    return f"agent-{version}.tar.gz"


# Each formula's description, what it needs besides Rust, how it is built,
# and its test.
FORMULAS = {
    "agent": (
        "Small daemon that hosts many durable AI agents",
        "",
        '    system "cargo", "install", *std_cargo_args\n',
        '    assert_equal "agent-runtime #{version}", shell_output("#{bin}/agent --version").strip\n',
    ),
    "sonny": (
        "Terminal coordinator for a daemon that hosts many durable AI agents",
        # Sonny starts this daemon when none runs, by its `opt` path, which
        # outlasts the versions upgrades remove.
        '  depends_on "lydakis/agent/agent"\n',
        '    system "cargo", "install", *std_cargo_args(path: "sonny")\n',
        '    assert_equal "sonny #{version}", shell_output("#{bin}/sonny --version").strip\n',
    ),
}


def render_formula(name: str, version: str, archive: Path) -> str:
    if not re.fullmatch(VERSION, version):
        raise ValueError("expected a release version such as 0.1.0 or 0.1.0-rc.1")
    if archive.name != archive_name(version):
        raise ValueError("source archive name must match the release version")
    desc, depends, install, test = FORMULAS[name]
    sha256 = hashlib.sha256(archive.read_bytes()).hexdigest()
    # Homebrew reads the version from the archive's name.
    return f'''class {name.capitalize()} < Formula
  desc "{desc}"
  homepage "https://github.com/lydakis/agent"
  url "https://github.com/lydakis/agent/releases/download/v{version}/{archive_name(version)}"
  sha256 "{sha256}"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/lydakis/agent.git", branch: "main"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on "rust" => :build
{depends}
  def install
{install}  end

  test do
{test}  end
end
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name", choices=sorted(FORMULAS))
    parser.add_argument("version")
    parser.add_argument("archive", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        formula = render_formula(args.name, args.version, args.archive)
    except (ValueError, OSError) as error:
        parser.error(str(error))
    args.output.write_text(formula)


if __name__ == "__main__":
    main()
