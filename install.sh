#!/bin/sh
# Installs Sonny and its daemon on Linux from a GitHub release, for machines
# without Homebrew (Homebrew does not run on ARM Linux):
#
#   curl -fsSL https://raw.githubusercontent.com/lydakis/agent/main/install.sh | sh
#
# It downloads sonny-VERSION-linux-ARCH.tar.gz, which holds static builds of
# Sonny (bin/sonny) and the daemon (libexec/agent), checks it against the
# release's .sha256 and, when the GitHub CLI is signed in, against the
# release workflow's build attestation (the checksum alone only guards the
# download), unpacks it to PREFIX/opt/sonny-VERSION-linux-ARCH, points
# PREFIX/opt/sonny at it and links PREFIX/bin/sonny to its Sonny. Earlier
# versions stay where they are, since a running daemon may still use one.
#
# It also installs, with the system's package manager (through sudo when
# not root), what Sonny's own features need and the release cannot carry:
# bubblewrap, the sandbox Sonny's agents run their commands in by default,
# and ripgrep, which they search code with.
#
#   SONNY_VERSION  a release such as 0.4.1 (default: the latest)
#   PREFIX         where to install (default: ~/.local)
#   SONNY_PACKAGES set to 0 to leave the system's packages alone
set -eu

repo=https://github.com/lydakis/agent

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

# The folders the daemon looks for bubblewrap in (src/sandbox.rs, BWRAP):
# never PATH, which a sandboxed command could plant a `bwrap` on.
bwrap_found() {
    for bwrap in /usr/bin/bwrap /bin/bwrap /usr/local/bin/bwrap /run/current-system/sw/bin/bwrap; do
        [ -x "$bwrap" ] && return 0
    done
    return 1
}

packages() {
    missing=""
    bwrap_found || missing="bubblewrap"
    command -v rg >/dev/null || missing="$missing ripgrep"
    missing=${missing# }
    [ -n "$missing" ] || return 0
    hint="Sonny's sandbox and code search need $missing; install with your package manager"
    if [ "${SONNY_PACKAGES:-1}" = 0 ]; then
        echo "$hint"
        return 0
    fi
    if command -v apt-get >/dev/null; then
        install="apt-get install -y"
    elif command -v dnf >/dev/null; then
        install="dnf install -y"
    elif command -v pacman >/dev/null; then
        install="pacman -S --needed --noconfirm"
    elif command -v zypper >/dev/null; then
        install="zypper --non-interactive install"
    elif command -v apk >/dev/null; then
        install="apk add"
    else
        echo "$hint"
        return 0
    fi
    if [ "$(id -u)" = 0 ]; then
        sudo=""
    elif command -v sudo >/dev/null; then
        sudo="sudo "
    else
        echo "$hint"
        return 0
    fi
    echo "Installing $missing: $sudo$install $missing"
    # A fresh image's apt has no package lists yet.
    # shellcheck disable=SC2086
    if ! $sudo$install $missing; then
        case "$install" in
            apt-get*) $sudo apt-get update && $sudo$install $missing && return 0 ;;
        esac
        echo "install.sh: could not install $missing; $hint" >&2
    fi
}

main() {
    case "$(uname -s)" in
        Linux) ;;
        Darwin) fail "on macOS, install with Homebrew: brew install lydakis/agent/sonny" ;;
        *) fail "Sonny runs on Linux and macOS, not $(uname -s)" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) fail "no release build for $(uname -m); build from source (see $repo#install)" ;;
    esac
    command -v curl >/dev/null || fail "curl is needed"
    command -v tar >/dev/null || fail "tar is needed"
    if command -v sha256sum >/dev/null; then
        sha256="sha256sum"
    elif command -v shasum >/dev/null; then
        sha256="shasum -a 256"
    else
        fail "sha256sum or shasum is needed to check the download"
    fi

    version=${SONNY_VERSION:-}
    if [ -z "$version" ]; then
        # The latest release's page redirects to its tag.
        latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$repo/releases/latest") ||
            fail "could not find the latest release"
        version=${latest##*/v}
        case "$version" in
            [0-9]*) ;;
            *) fail "could not find the latest release (got $latest)" ;;
        esac
    fi
    version=${version#v}
    prefix=${PREFIX:-$HOME/.local}
    name="sonny-$version-linux-$arch"
    url="$repo/releases/download/v$version/$name.tar.gz"

    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    echo "Downloading $name"
    curl -fsSL -o "$work/$name.tar.gz" "$url" || fail "could not download $url"
    curl -fsSL -o "$work/$name.tar.gz.sha256" "$url.sha256" || fail "could not download $url.sha256"
    (cd "$work" && $sha256 -c "$name.tar.gz.sha256" >/dev/null) ||
        fail "$name.tar.gz does not match its published sha256"
    if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
        gh attestation verify "$work/$name.tar.gz" --repo lydakis/agent \
            --signer-workflow lydakis/agent/.github/workflows/release.yml \
            --deny-self-hosted-runners >/dev/null ||
            fail "$name.tar.gz has no build attestation from the release workflow"
    fi

    mkdir -p "$prefix/opt" "$prefix/bin"
    tar -xzf "$work/$name.tar.gz" -C "$work"
    [ -x "$work/$name/bin/sonny" ] && [ -x "$work/$name/libexec/agent" ] ||
        fail "$name.tar.gz is not laid out as bin/sonny and libexec/agent"
    rm -rf "${prefix:?}/opt/$name"
    mv "$work/$name" "$prefix/opt/$name"
    ln -sfn "$name" "$prefix/opt/sonny"
    ln -sfn "../opt/sonny/bin/sonny" "$prefix/bin/sonny"

    echo "Installed $("$prefix/bin/sonny" --version) at $prefix/bin/sonny"
    packages
    case ":$PATH:" in
        *":$prefix/bin:"*) ;;
        *) echo "Add $prefix/bin to your PATH, for example in your shell's profile:
  export PATH=\"$prefix/bin:\$PATH\"" ;;
    esac
    echo "Run sonny in a project folder; the first run asks for a model provider."
}

main "$@"
