#!/usr/bin/env bash
# The C toolchain for the static Linux builds, as Codex sets it up
# (openai/codex .github/scripts/install-musl-build-tools.sh): Zig's clang
# compiles the C dependencies (jemalloc, SQLite, aws-lc, ring) for the musl
# target. With Ubuntu's musl-gcc on the arm runner, jemalloc's configure found
# no usable atomics and its build failed (#1192).
#
# Needs zig on PATH and TARGET set to x86_64-unknown-linux-musl or
# aarch64-unknown-linux-musl. Writes the compiler settings to GITHUB_ENV.
set -euo pipefail

: "${TARGET:?TARGET environment variable is required}"
: "${GITHUB_ENV:?GITHUB_ENV environment variable is required}"

case "${TARGET}" in
  x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
  *)
    echo "Unexpected musl target: ${TARGET}" >&2
    exit 1
    ;;
esac

zig_bin="$(command -v zig)"
zig_target="${TARGET/-unknown-linux-musl/-linux-musl}"
tool_root="${RUNNER_TEMP:-/tmp}/musl-tools-${TARGET}"
mkdir -p "${tool_root}"
cc="${tool_root}/zigcc"

# Codex's wrapper: zig takes its own -target, and the host's glibc headers
# must not win over musl's.
cat >"${cc}" <<EOF
#!/usr/bin/env bash
set -euo pipefail

args=()
skip_next=0
pending_include=0
for arg in "\$@"; do
  if [[ "\${pending_include}" -eq 1 ]]; then
    pending_include=0
    if [[ "\${arg}" == /usr/include || "\${arg}" == /usr/include/* ]]; then
      args+=("-idirafter" "\${arg}")
    else
      args+=("-I" "\${arg}")
    fi
    continue
  fi

  if [[ "\${skip_next}" -eq 1 ]]; then
    skip_next=0
    continue
  fi
  case "\${arg}" in
    --target)
      skip_next=1
      continue
      ;;
    --target=*|-target=*|-target)
      if [[ "\${arg}" == "-target" ]]; then
        skip_next=1
      fi
      continue
      ;;
    -I)
      pending_include=1
      continue
      ;;
    -I/usr/include|-I/usr/include/*)
      args+=("-idirafter" "\${arg#-I}")
      continue
      ;;
    -Wp,-U_FORTIFY_SOURCE)
      args+=("-U_FORTIFY_SOURCE")
      continue
      ;;
  esac
  args+=("\${arg}")
done

# Rust links these objects without Zig's sanitizer runtime.
exec "${zig_bin}" cc -target "${zig_target}" "\${args[@]}" -fno-sanitize=undefined
EOF
chmod +x "${cc}"

target_var="${TARGET//-/_}"
{
  echo "CC_${target_var}=${cc}"
  echo "CFLAGS_${target_var}=-pthread"
  # As Codex: aws-lc's jitter entropy source misbehaves on musl builders.
  echo "AWS_LC_SYS_NO_JITTER_ENTROPY_${target_var}=1"
} >>"${GITHUB_ENV}"
