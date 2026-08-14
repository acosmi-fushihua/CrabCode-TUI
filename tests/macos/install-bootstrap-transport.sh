#!/usr/bin/env bash
set -euo pipefail

INSTALLER="${1:?installer path is required}"
SCRATCH="$(mktemp -d)"
READY_FILE="${SCRATCH}/ready"
SERVER_LOG="${SCRATCH}/server.log"
REQUEST_LOG="${SCRATCH}/curl-requests.log"
SHIM_DIR="${SCRATCH}/bin"
SERVER_PID=""

cleanup() {
  if [[ -n "${SERVER_PID}" ]]; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -rf "${SCRATCH}"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

node tests/fixtures/bootstrap-http-server.mjs "${INSTALLER}" "${READY_FILE}" >"${SERVER_LOG}" 2>&1 &
SERVER_PID="$!"
for _ in {1..100}; do
  [[ -f "${READY_FILE}" ]] && break
  kill -0 "${SERVER_PID}" 2>/dev/null || {
    cat "${SERVER_LOG}" >&2
    exit 1
  }
  sleep 0.05
done
[[ -f "${READY_FILE}" ]] || {
  cat "${SERVER_LOG}" >&2
  echo "bootstrap transport server did not start" >&2
  exit 1
}

BASE_URL="$(<"${READY_FILE}")"
REAL_CURL="$(command -v curl)"
VERSION="$(node -p "require('./package.json').version")"
case "$(uname -m)" in
  arm64|aarch64) PLATFORM="arm64-darwin" ;;
  x86_64|amd64) PLATFORM="x64-darwin" ;;
  *) echo "unsupported test architecture: $(uname -m)" >&2; exit 1 ;;
esac
ARCHIVE="crabcode-${VERSION}-${PLATFORM}.tar.gz"
CHECKSUM_URL="https://github.com/acosmi/CrabCode-TUI/releases/latest/download/checksums-sha256.txt"
ARCHIVE_URL="https://github.com/acosmi/CrabCode-TUI/releases/download/v${VERSION}/${ARCHIVE}"

mkdir -p \
  "${SCRATCH}/home" \
  "${SCRATCH}/xdg/config" \
  "${SCRATCH}/xdg/data" \
  "${SCRATCH}/xdg/state" \
  "${SCRATCH}/xdg/cache" \
  "${SCRATCH}/install-bin" \
  "${SHIM_DIR}"
export HOME="${SCRATCH}/home"
export XDG_CONFIG_HOME="${SCRATCH}/xdg/config"
export XDG_DATA_HOME="${SCRATCH}/xdg/data"
export XDG_STATE_HOME="${SCRATCH}/xdg/state"
export XDG_CACHE_HOME="${SCRATCH}/xdg/cache"
export CRABCODE_BIN_DIR="${SCRATCH}/install-bin"
unset CRABCODE_VERSION CRABCODE_ASSET_DIR

export CRABCODE_TEST_CHECKSUM_URL="${CHECKSUM_URL}"
export CRABCODE_TEST_ARCHIVE_URL="${ARCHIVE_URL}"
export CRABCODE_TEST_ARCHIVE="${ARCHIVE}"
export CRABCODE_TEST_REQUEST_LOG="${REQUEST_LOG}"
cat >"${SHIM_DIR}/curl" <<'SHIM'
#!/usr/bin/env sh
set -eu

destination=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      shift
      [ "$#" -gt 0 ] || exit 64
      destination="$1"
      ;;
    http://*|https://*) url="$1" ;;
  esac
  shift
done

[ -n "$url" ] || exit 64
printf '%s\n' "$url" >> "$CRABCODE_TEST_REQUEST_LOG"
case "$url" in
  "$CRABCODE_TEST_CHECKSUM_URL")
    [ -n "$destination" ] || exit 64
    printf '%064d  %s\n' 0 "$CRABCODE_TEST_ARCHIVE" > "$destination"
    ;;
  "$CRABCODE_TEST_ARCHIVE_URL")
    # Stop after proving that latest checksum discovery produced the fixed-tag
    # archive URL. The package itself is covered by release-package-smoke.
    exit 22
    ;;
  *)
    printf 'unexpected installer curl URL: %s\n' "$url" >&2
    exit 65
    ;;
esac
SHIM
chmod 755 "${SHIM_DIR}/curl"

ORIGINAL_PATH="${PATH}"
export PATH="${SHIM_DIR}:${ORIGINAL_PATH}"
curl() {
  "${REAL_CURL}" "$@"
}
set +e
OUTPUT="$(curl -fsSL "${BASE_URL}/latest/download/install.sh" | sh 2>&1)"
STATUS="$?"
set -e
unset -f curl
export PATH="${ORIGINAL_PATH}"
[[ "${STATUS}" -ne 0 ]] || {
  echo "wire bootstrap unexpectedly completed" >&2
  exit 1
}
[[ "${OUTPUT}" == *"从 latest Release 的 SHA-256 清单解析版本"* ]] || {
  printf 'wire bootstrap did not execute latest checksum discovery:\n%s\n' "${OUTPUT}" >&2
  exit 1
}
[[ "${OUTPUT}" == *"下载 ${ARCHIVE}"* ]] || {
  printf 'wire bootstrap did not derive the expected archive:\n%s\n' "${OUTPUT}" >&2
  exit 1
}
[[ "${OUTPUT}" == *"未找到 v${VERSION} 的 ${PLATFORM} 完整包"* ]] || {
  printf 'wire bootstrap did not stop at the deliberate archive failure:\n%s\n' "${OUTPUT}" >&2
  exit 1
}
[[ "${OUTPUT}" != *"syntax error"* ]] || {
  printf 'wire bootstrap exposed a shell decoding error:\n%s\n' "${OUTPUT}" >&2
  exit 1
}

[[ -f "${REQUEST_LOG}" ]] || {
  echo "curl shim did not record installer requests" >&2
  exit 1
}
[[ "$(wc -l <"${REQUEST_LOG}" | tr -d ' ')" == "2" ]] || {
  printf 'curl shim recorded an unexpected request count:\n%s\n' "$(cat "${REQUEST_LOG}")" >&2
  exit 1
}
[[ "$(sed -n '1p' "${REQUEST_LOG}")" == "${CHECKSUM_URL}" ]] || {
  printf 'installer did not request the latest checksum URL first:\n%s\n' "$(cat "${REQUEST_LOG}")" >&2
  exit 1
}
[[ "$(sed -n '2p' "${REQUEST_LOG}")" == "${ARCHIVE_URL}" ]] || {
  printf 'installer did not request the derived fixed-tag archive URL:\n%s\n' "$(cat "${REQUEST_LOG}")" >&2
  exit 1
}
! grep -Fq 'api.github.com' "${REQUEST_LOG}" || {
  printf 'installer unexpectedly contacted api.github.com:\n%s\n' "$(cat "${REQUEST_LOG}")" >&2
  exit 1
}
