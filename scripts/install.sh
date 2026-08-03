#!/usr/bin/env sh
# SECURITY NOTICE: piping this bootstrap from a mutable URL does not authenticate its source.
# CrabCode TUI fail-closed installer for macOS and Linux.
set -eu

REPOSITORY="acosmi/CrabCode-TUI"
API_URL="https://api.github.com/repos/${REPOSITORY}/releases/latest"
TEMP_ROOT=""
INCOMING=""

cleanup() {
  if [ -n "$TEMP_ROOT" ] && [ -d "$TEMP_ROOT" ]; then
    rm -rf "$TEMP_ROOT"
  fi
  if [ -n "$INCOMING" ] && [ -d "$INCOMING" ]; then
    rm -rf "$INCOMING"
  fi
}
trap cleanup EXIT HUP INT TERM

info() { printf 'info: %s\n' "$1"; }
die() { printf 'error: %s\n' "$1" >&2; exit 1; }

command -v tar >/dev/null 2>&1 || die "需要 tar 才能解压安装包"

if [ -n "${CRABCODE_ASSET_DIR:-}" ] && [ -z "${CRABCODE_VERSION:-}" ]; then
  die "CRABCODE_ASSET_DIR 本地模式必须同时固定 CRABCODE_VERSION"
fi
if [ -z "${CRABCODE_ASSET_DIR:-}" ]; then
  command -v curl >/dev/null 2>&1 || die "需要 curl 才能下载安装包"
  info "安全提示：当前 bootstrap 本身未验证来源；推荐按 README 先用 gh attestation verify 校验固定版本资产"
fi

case "$(uname -s)" in
  Darwin) OS="darwin" ;;
  Linux) OS="linux" ;;
  *) die "仅支持 macOS 和 Linux；Windows 请使用 install.ps1" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH="arm64" ;;
  x86_64|amd64) ARCH="x64" ;;
  *) die "不支持的 CPU 架构: $(uname -m)" ;;
esac
PLATFORM="${ARCH}-${OS}"

if [ -n "${CRABCODE_VERSION:-}" ]; then
  TAG="$CRABCODE_VERSION"
else
  info "查询最新 GitHub Release"
  TAG="$(curl --proto '=https' --tlsv1.2 -fsSL --retry 3 "$API_URL" \
    | sed -n -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' \
    | head -n 1)"
fi
case "$TAG" in v*) VERSION="${TAG#v}" ;; *) VERSION="$TAG"; TAG="v${TAG}" ;; esac
printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$' \
  || die "发布版本不是规范 SemVer: $TAG"

ARCHIVE="crabcode-${VERSION}-${PLATFORM}.tar.gz"
BASE_URL="https://github.com/${REPOSITORY}/releases/download/${TAG}"
TEMP_ROOT="$(mktemp -d)"
if [ -n "${CRABCODE_ASSET_DIR:-}" ]; then
  case "$CRABCODE_ASSET_DIR" in /*) ;; *) die "CRABCODE_ASSET_DIR 必须是绝对路径" ;; esac
  [ -d "$CRABCODE_ASSET_DIR" ] || die "CRABCODE_ASSET_DIR 不是目录: $CRABCODE_ASSET_DIR"
  ARCHIVE_PATH="${CRABCODE_ASSET_DIR}/${ARCHIVE}"
  CHECKSUM_PATH="${CRABCODE_ASSET_DIR}/checksums-sha256.txt"
  [ -f "$ARCHIVE_PATH" ] || die "本地资产目录缺少 ${ARCHIVE}"
  [ -f "$CHECKSUM_PATH" ] || die "本地资产目录缺少 checksums-sha256.txt"
  info "使用已下载的固定版本本地资产；安装器不会访问网络"
else
  ARCHIVE_PATH="${TEMP_ROOT}/${ARCHIVE}"
  CHECKSUM_PATH="${TEMP_ROOT}/checksums-sha256.txt"
  info "下载 ${ARCHIVE}"
  curl --proto '=https' --tlsv1.2 -fL --retry 3 --connect-timeout 15 --max-time 600 \
    -o "$ARCHIVE_PATH" "${BASE_URL}/${ARCHIVE}" \
    || die "未找到 ${TAG} 的 ${PLATFORM} 完整包"
  curl --proto '=https' --tlsv1.2 -fL --retry 3 --connect-timeout 15 --max-time 120 \
    -o "$CHECKSUM_PATH" "${BASE_URL}/checksums-sha256.txt" \
    || die "无法下载同一 Release 的 SHA-256 清单，拒绝安装"
fi

EXPECTED_LINES="$(awk -v name="$ARCHIVE" '$2 == name { print $1 }' "$CHECKSUM_PATH")"
[ "$(printf '%s\n' "$EXPECTED_LINES" | sed '/^$/d' | wc -l | tr -d ' ')" = "1" ] \
  || die "SHA-256 清单必须且只能包含一条 ${ARCHIVE} 记录"
EXPECTED="$(printf '%s' "$EXPECTED_LINES" | tr 'A-F' 'a-f')"
printf '%s\n' "$EXPECTED" | grep -Eq '^[a-f0-9]{64}$' || die "SHA-256 清单格式无效"
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$ARCHIVE_PATH" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "$ARCHIVE_PATH" | awk '{print $1}')"
else
  die "系统缺少 sha256sum/shasum，拒绝安装未校验文件"
fi
[ "$ACTUAL" = "$EXPECTED" ] || die "发布包 SHA-256 不匹配，拒绝安装"
info "发布级 SHA-256 校验通过"

PACKAGE_ROOT="crabcode-${VERSION}-${PLATFORM}"
MEMBERS="${TEMP_ROOT}/members.txt"
tar -tzf "$ARCHIVE_PATH" > "$MEMBERS" || die "无法读取发布包目录"
[ -s "$MEMBERS" ] || die "发布包为空"
while IFS= read -r member; do
  case "$member" in
    "$PACKAGE_ROOT"|"$PACKAGE_ROOT/"|"$PACKAGE_ROOT/"*) ;;
    *) die "发布包存在越界成员: $member" ;;
  esac
  case "/$member/" in *"/../"*|*"/./"*) die "发布包存在不安全路径: $member" ;; esac
done < "$MEMBERS"
tar -xzf "$ARCHIVE_PATH" -C "$TEMP_ROOT" || die "发布包解压失败"
SOURCE="${TEMP_ROOT}/${PACKAGE_ROOT}"
[ -d "$SOURCE" ] || die "解压后缺少唯一包根 ${PACKAGE_ROOT}"
[ -z "$(find "$SOURCE" -type l -print -quit)" ] || die "发布包含符号链接，拒绝安装"
[ -z "$(find "$SOURCE" ! -type d ! -type f -print -quit)" ] || die "发布包含特殊文件，拒绝安装"

verify_manifest() {
  root="$1"
  bun_bin="${root}/bun"
  [ -x "$bun_bin" ] || die "发布包缺少可执行的内置 Bun"
  # The JavaScript program is intentionally single-quoted so shell expansion
  # cannot rewrite template literals or package paths.
  # shellcheck disable=SC2016
  "$bun_bin" -e '
    import { createHash } from "node:crypto";
    import { lstatSync, readFileSync, readdirSync } from "node:fs";
    import { join, relative } from "node:path";
    const root = process.argv[1];
    const hash = path => createHash("sha256").update(readFileSync(path)).digest("hex");
    const manifestPath = join(root, "release-manifest.json");
    const digestPath = join(root, "release-manifest.digest.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    const digest = JSON.parse(readFileSync(digestPath, "utf8"));
    if (digest.schemaVersion !== 1 || digest.scheme !== "sha256" || digest.manifestSha256 !== hash(manifestPath)) throw new Error("manifest digest binding is invalid");
    const actual = [];
    const visit = dir => { for (const name of readdirSync(dir).sort()) { const path = join(dir, name); const stat = lstatSync(path); if (stat.isSymbolicLink()) throw new Error(`symlink: ${path}`); if (stat.isDirectory()) visit(path); else if (stat.isFile() && stat.size > 0) { const rel = relative(root, path).replaceAll("\\", "/"); if (rel !== "release-manifest.json" && rel !== "release-manifest.digest.json") actual.push({ path: rel, sha256: hash(path), size: stat.size }); } else throw new Error(`special/empty file: ${path}`); } };
    visit(root);
    actual.sort((a, b) => a.path < b.path ? -1 : a.path > b.path ? 1 : 0);
    if (JSON.stringify(actual) !== JSON.stringify(manifest.files)) throw new Error("package inventory differs from manifest");
  ' "$root" || die "包内逐文件清单校验失败"
}

verify_manifest "$SOURCE"
info "包内逐文件清单校验通过"

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="${CRABCODE_BIN_DIR:-$HOME/.local/bin}"
case "$DATA_HOME" in /*) ;; *) die "XDG_DATA_HOME 必须是绝对路径" ;; esac
case "$BIN_DIR" in /*) ;; *) die "CRABCODE_BIN_DIR 必须是绝对路径" ;; esac
VERSIONS="${DATA_HOME}/crabcode/versions"
DESTINATION="${VERSIONS}/${VERSION}"
mkdir -p "$VERSIONS" "$BIN_DIR"

if [ -e "$DESTINATION" ]; then
  [ -d "$DESTINATION" ] || die "已有版本路径不是目录: $DESTINATION"
  verify_manifest "$DESTINATION"
  info "复用已校验的不可变版本 ${VERSION}"
else
  INCOMING="${VERSIONS}/.install-${VERSION}-$$"
  [ ! -e "$INCOMING" ] || die "临时安装目录已存在: $INCOMING"
  mkdir "$INCOMING"
  cp -R "${SOURCE}/." "$INCOMING/"
  verify_manifest "$INCOMING"
  mv "$INCOMING" "$DESTINATION"
  INCOMING=""
fi

DESTINATION="$(cd "$DESTINATION" && pwd -P)"
STABLE="${BIN_DIR}/crabcode"
STABLE_TMP="${BIN_DIR}/.crabcode-install-$$"
cp "${DESTINATION}/crabcode" "$STABLE_TMP"
chmod 755 "$STABLE_TMP"

CURRENT_TMP="${VERSIONS}/.current.tmp.$$"
printf '%s\n' "$DESTINATION" > "$CURRENT_TMP"
mv "$CURRENT_TMP" "${VERSIONS}/.current"
mv "$STABLE_TMP" "$STABLE"
MARKER_TMP="${VERSIONS}/.launcher-v1.tmp.$$"
printf '%s\n' "$STABLE" > "$MARKER_TMP"
mv "$MARKER_TMP" "${VERSIONS}/.launcher-v1"

info "CrabCode TUI ${VERSION} 已安装到 ${DESTINATION}"
case ":${PATH}:" in
  *":${BIN_DIR}:"*) printf '运行: crabcode\n' ;;
  *) printf '请将以下目录加入 PATH 后运行 crabcode:\n  %s\n' "$BIN_DIR" ;;
esac
