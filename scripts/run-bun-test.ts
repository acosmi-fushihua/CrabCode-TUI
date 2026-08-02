/**
 * Cross-platform launcher for `bun test` that injects NO_PROXY/no_proxy with
 * the localhost hosts BEFORE Bun starts.
 *
 * Why a launcher (and not a JS-side env mutation): Bun reads HTTP_PROXY /
 * HTTPS_PROXY / NO_PROXY at process startup and caches them. Mutating
 * `process.env.NO_PROXY` from a test file or a side-effect import is too
 * late — the global `fetch()` used to talk to the in-process mock API
 * server will still be routed through the developer's `http_proxy` /
 * `all_proxy`, returning 502 instead of the 401/403 the mock would emit.
 *
 * Usage (driven by `package.json` scripts):
 *
 *   bun scripts/run-bun-test.ts ./tests/
 *   bun scripts/run-bun-test.ts ./tests/unit/direct-tui-runtime.test.ts
 *
 * The launcher forwards every CLI arg verbatim to `bun test`.
 */

const LOCALHOST_HOSTS = ['127.0.0.1', 'localhost', '::1'] as const;

function mergeNoProxy(existing: string | undefined): string {
  const parts = (existing ?? '')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean);
  for (const host of LOCALHOST_HOSTS) {
    if (!parts.some((p) => p.toLowerCase() === host.toLowerCase())) {
      parts.push(host);
    }
  }
  return parts.join(',');
}

const childEnv: Record<string, string> = { ...(process.env as Record<string, string>) };
const merged = mergeNoProxy(childEnv.NO_PROXY ?? childEnv.no_proxy);
childEnv.NO_PROXY = merged;
childEnv.no_proxy = merged;

const proc = Bun.spawn(['bun', 'test', ...process.argv.slice(2)], {
  stdio: ['inherit', 'inherit', 'inherit'],
  env: childEnv,
});

const exitCode = await proc.exited;
process.exit(exitCode);
