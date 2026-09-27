"""Generate the Homebrew cask for one release archive of the desktop app."""

import argparse
import hashlib
from pathlib import Path
import re


VERSION = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?"


def archive_name(version: str) -> str:
    return f"Agent_{version}_universal.zip"


def render_cask(version: str, archive: Path) -> str:
    if not re.fullmatch(VERSION, version):
        raise ValueError("expected a release version such as 0.1.0 or 0.1.0-rc.1")
    if archive.name != archive_name(version):
        raise ValueError("app archive name must match the release version")
    sha256 = hashlib.sha256(archive.read_bytes()).hexdigest()
    return f'''cask "agent" do
  version "{version}"
  sha256 "{sha256}"

  url "https://github.com/lydakis/agent/releases/download/v#{{version}}/Agent_#{{version}}_universal.zip"
  name "Agent"
  desc "Desktop client for a daemon that hosts many durable AI agents"
  homepage "https://github.com/lydakis/agent"

  livecheck do
    url :url
    strategy :github_latest
  end

  app "Agent.app"

  zap trash: [
    "~/Library/Caches/me.lydakis.agent",
    "~/Library/Saved Application State/me.lydakis.agent.savedState",
    "~/Library/WebKit/me.lydakis.agent",
  ]
end
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("archive", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        cask = render_cask(args.version, args.archive)
    except (ValueError, OSError) as error:
        parser.error(str(error))
    args.output.write_text(cask)


if __name__ == "__main__":
    main()
