// Release manifests are signed data. Their ordering must not depend on the
// runner's locale, ICU version, or operating system.
export function comparePortablePaths(left, right) {
  return left < right ? -1 : left > right ? 1 : 0
}
