import { spawn } from "node:child_process";
import * as fs from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve, win32 } from "node:path";
import { randomBytes } from "node:crypto";

const MASTER_KEY_STORAGE_FIELD = "accountBridgeMasterKeyBase64Url";
const MASTER_KEY_BYTES = 32;
const COMMAND_TIMEOUT_MS = 10_000;
const COMMAND_MAX_OUTPUT_BYTES = 8 << 10;
const LINUX_SECRET_TOOL_CANDIDATES = [
  "/usr/bin/secret-tool",
  "/bin/secret-tool",
] as const;
const LINUX_SECRET_ATTRIBUTES = [
  "application",
  "CrabCode",
  "service",
  "account-bridge-master-key",
  "schema-version",
  "1",
] as const;

export class AccountBridgeMasterKeyError extends Error {
  constructor(public readonly code: string) {
    super(code);
    this.name = "AccountBridgeMasterKeyError";
  }
}

export type AccountBridgeMasterKeyRead =
  | { status: "data"; value: string }
  | { status: "absent" }
  | { status: "error" };

export interface AccountBridgeMasterKeyBackend {
  read(): Promise<AccountBridgeMasterKeyRead>;
  write(value: string): Promise<void>;
}

export interface AccountBridgeMasterKeyDeps {
  platform: NodeJS.Platform;
  randomBytes(size: number): Uint8Array;
  withLock<T>(operation: () => Promise<T>): Promise<T>;
  createBackend(
    platform: "darwin" | "win32" | "linux",
  ): Promise<AccountBridgeMasterKeyBackend>;
}

export interface AccountBridgeSecureCommand {
  command: string;
  args: readonly string[];
  input?: Uint8Array;
  env?: NodeJS.ProcessEnv;
  timeoutMs?: number;
}

export interface AccountBridgeSecureCommandResult {
  code: number | null;
  stdout: string;
}

export type AccountBridgeSecureCommandRunner = (
  input: AccountBridgeSecureCommand,
) => Promise<AccountBridgeSecureCommandResult>;

function configRoot(): string {
  const explicit = process.env.CRABCODE_CONFIG_DIR?.trim();
  if (explicit) return resolve(explicit);
  const home = process.env.CRABCODE_HOME?.trim();
  return home ? resolve(home, ".crabcode") : join(homedir(), ".crabcode");
}

function canonicalMasterKey(value: unknown): Uint8Array {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]{43}$/.test(value)) {
    throw new AccountBridgeMasterKeyError("master-key-invalid");
  }
  const bytes = Buffer.from(value, "base64url");
  if (
    bytes.length !== MASTER_KEY_BYTES ||
    Buffer.from(bytes).toString("base64url") !== value
  ) {
    bytes.fill(0);
    throw new AccountBridgeMasterKeyError("master-key-invalid");
  }
  return bytes;
}

async function productionLock<T>(
  platform: "darwin" | "win32" | "linux",
  operation: () => Promise<T>,
): Promise<T> {
  const { withCrossProcessResourceLock } = await import(
    "../../utils/crossProcessResourceLock.js"
  );
  // macOS shares one aggregate Keychain record with the existing credential
  // backend, so it must use the same lock. DPAPI and Secret Service own an
  // isolated item and use an Account Bridge-specific lock.
  return withCrossProcessResourceLock(
    platform === "darwin"
      ? "secure-storage-mutation"
      : "account-bridge-master-key",
    operation,
  );
}

export async function loadOrCreateAccountBridgeMasterKey(
  deps: AccountBridgeMasterKeyDeps = productionMasterKeyDeps(),
): Promise<Uint8Array> {
  if (
    deps.platform !== "darwin" &&
    deps.platform !== "win32" &&
    deps.platform !== "linux"
  ) {
    throw new AccountBridgeMasterKeyError(
      "master-key-secure-storage-unavailable",
    );
  }
  const platform = deps.platform;
  try {
    return await deps.withLock(async () => {
      const backend = await deps.createBackend(platform);
      const existing = await backend.read();
      if (existing.status === "error") {
        throw new AccountBridgeMasterKeyError(
          "master-key-secure-storage-unavailable",
        );
      }
      if (existing.status === "data") {
        return canonicalMasterKey(existing.value);
      }

      const generated = Buffer.from(deps.randomBytes(MASTER_KEY_BYTES));
      if (generated.length !== MASTER_KEY_BYTES) {
        generated.fill(0);
        throw new AccountBridgeMasterKeyError("master-key-invalid");
      }
      const encoded = generated.toString("base64url");
      try {
        await backend.write(encoded);
        const committed = await backend.read();
        if (committed.status !== "data") {
          throw new AccountBridgeMasterKeyError(
            "master-key-secure-storage-unavailable",
          );
        }
        if (committed.value !== encoded) {
          throw new AccountBridgeMasterKeyError("master-key-invalid");
        }
        return Buffer.from(generated);
      } finally {
        generated.fill(0);
      }
    });
  } catch (error) {
    if (error instanceof AccountBridgeMasterKeyError) throw error;
    throw new AccountBridgeMasterKeyError(
      "master-key-secure-storage-unavailable",
    );
  }
}

async function createDarwinKeychainBackend(): Promise<AccountBridgeMasterKeyBackend> {
  const [keychain, helpers] = await Promise.all([
    import("../../utils/secureStorage/macOsKeychainStorage.js"),
    import("../../utils/secureStorage/macOsKeychainHelpers.js"),
  ]);
  return {
    async read() {
      helpers.clearKeychainCache();
      const result = await keychain.macOsKeychainStorage.readStatusAsync();
      if (result.status === "error") return { status: "error" };
      if (result.status === "absent") return { status: "absent" };
      const value = result.data[MASTER_KEY_STORAGE_FIELD];
      return value === undefined
        ? { status: "absent" }
        : { status: "data", value: typeof value === "string" ? value : "" };
    },
    async write(value) {
      const result = await keychain.macOsKeychainStorage.mutateAsync(current => {
        const existing = current[MASTER_KEY_STORAGE_FIELD];
        if (existing !== undefined) return current;
        return { ...current, [MASTER_KEY_STORAGE_FIELD]: value };
      });
      // This is the authoritative Keychain backend, never the generic storage selector. Any
      // warning still fails closed because warnings identify degraded storage in
      // the shared SecureStorage contract.
      if (!result.success || result.warning !== undefined) {
        throw new AccountBridgeMasterKeyError(
          "master-key-secure-storage-unavailable",
        );
      }
    },
  };
}

export async function runAccountBridgeSecureCommand(
  input: AccountBridgeSecureCommand,
): Promise<AccountBridgeSecureCommandResult> {
  return await new Promise(resolveResult => {
    let settled = false;
    let outputBytes = 0;
    const output: Buffer[] = [];
    const child = spawn(input.command, [...input.args], {
      env: input.env ?? process.env,
      shell: false,
      windowsHide: true,
      stdio: ["pipe", "pipe", "ignore"],
    });
    const finish = (code: number | null): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolveResult({ code, stdout: Buffer.concat(output).toString("utf8") });
      for (const chunk of output) chunk.fill(0);
    };
    const timer = setTimeout(() => {
      child.kill();
      finish(null);
    }, input.timeoutMs ?? COMMAND_TIMEOUT_MS);
    child.once("error", () => finish(null));
    child.once("close", code => finish(code));
    child.stdout.on("data", (chunk: Buffer | Uint8Array) => {
      const copy = Buffer.from(chunk);
      outputBytes += copy.length;
      if (outputBytes > COMMAND_MAX_OUTPUT_BYTES) {
        copy.fill(0);
        child.kill();
        finish(null);
        return;
      }
      output.push(copy);
    });
    child.stdin.once("error", () => {
      child.kill();
      finish(null);
    });
    child.stdin.end(input.input ? Buffer.from(input.input) : undefined);
  });
}

// Fixed script: no secret, path, username, or release input is interpolated
// into argv. The target path and (for writes) key travel only over stdin.
export const WINDOWS_DPAPI_SCRIPT = String.raw`
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Security
$rawInput = [Console]::In.ReadToEnd()
$parts = $rawInput -split [char]10, 3
$operation = $parts[0].TrimEnd([char]13)
$target = $parts[1].TrimEnd([char]13)
if ([String]::IsNullOrWhiteSpace($target)) { throw 'invalid target' }
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
$entropy = [Text.Encoding]::UTF8.GetBytes('CrabCode.AccountBridge.MasterKey.v1')

function Assert-NoReparse([string] $path) {
  $attributes = [IO.File]::GetAttributes($path)
  if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
    throw 'reparse point rejected'
  }
}

function New-PrivateDirectoryAcl {
  $acl = New-Object Security.AccessControl.DirectorySecurity
  $acl.SetOwner($sid)
  $acl.SetAccessRuleProtection($true, $false)
  $rule = New-Object Security.AccessControl.FileSystemAccessRule(
    $sid,
    [Security.AccessControl.FileSystemRights]::FullControl,
    [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit',
    [Security.AccessControl.PropagationFlags]::None,
    [Security.AccessControl.AccessControlType]::Allow
  )
  [void] $acl.AddAccessRule($rule)
  return $acl
}

function New-PrivateFileAcl {
  $acl = New-Object Security.AccessControl.FileSecurity
  $acl.SetOwner($sid)
  $acl.SetAccessRuleProtection($true, $false)
  $rule = New-Object Security.AccessControl.FileSystemAccessRule(
    $sid,
    [Security.AccessControl.FileSystemRights]::FullControl,
    [Security.AccessControl.AccessControlType]::Allow
  )
  [void] $acl.AddAccessRule($rule)
  return $acl
}

function Assert-PrivateAcl([string] $path, [bool] $directory) {
  $acl = if ($directory) {
    [IO.Directory]::GetAccessControl($path)
  } else {
    [IO.File]::GetAccessControl($path)
  }
  $owner = (New-Object Security.Principal.NTAccount($acl.Owner)).Translate(
    [Security.Principal.SecurityIdentifier]
  )
  if ($owner.Value -ne $sid.Value -or -not $acl.AreAccessRulesProtected) {
    throw 'non-private owner or inheritance'
  }
  $rules = $acl.GetAccessRules(
    $true,
    $true,
    [Security.Principal.SecurityIdentifier]
  )
  if ($rules.Count -ne 1) { throw 'unexpected acl rule count' }
  foreach ($rule in $rules) {
    if (
      $rule.IdentityReference.Value -ne $sid.Value -or
      $rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
      (($rule.FileSystemRights -band [Security.AccessControl.FileSystemRights]::FullControl) -ne [Security.AccessControl.FileSystemRights]::FullControl)
    ) { throw 'non-private acl rule' }
  }
}

if ($operation -eq 'read') {
  if (-not [IO.File]::Exists($target)) {
    [Console]::Out.Write('ABSENT')
    exit 0
  }
  $directory = [IO.Path]::GetDirectoryName($target)
  Assert-NoReparse $directory
  Assert-NoReparse $target
  Assert-PrivateAcl $directory $true
  Assert-PrivateAcl $target $false
  $ciphertext = [IO.File]::ReadAllBytes($target)
  if ($ciphertext.Length -lt 16 -or $ciphertext.Length -gt 65536) {
    throw 'invalid ciphertext length'
  }
  $plaintext = $null
  try {
    $plaintext = [Security.Cryptography.ProtectedData]::Unprotect(
      $ciphertext,
      $entropy,
      [Security.Cryptography.DataProtectionScope]::CurrentUser
    )
    if ($plaintext.Length -ne 32) { throw 'invalid plaintext length' }
    [Console]::Out.Write('KEY:' + [Convert]::ToBase64String($plaintext))
  } finally {
    if ($plaintext -ne $null) { [Array]::Clear($plaintext, 0, $plaintext.Length) }
    [Array]::Clear($ciphertext, 0, $ciphertext.Length)
    [Array]::Clear($entropy, 0, $entropy.Length)
  }
  exit 0
}

if ($operation -ne 'write' -or $parts.Count -ne 3) { throw 'invalid operation' }
$secret = $parts[2].Trim()
$standardBase64 = $secret.Replace('-', '+').Replace('_', '/')
switch ($standardBase64.Length % 4) {
  0 { break }
  2 { $standardBase64 += '==' ; break }
  3 { $standardBase64 += '=' ; break }
  default { throw 'invalid base64url length' }
}
$plaintext = [Convert]::FromBase64String($standardBase64)
if ($plaintext.Length -ne 32) { throw 'invalid plaintext length' }
$directory = [IO.Path]::GetDirectoryName($target)
$temporary = $target + '.tmp.' + [Guid]::NewGuid().ToString('N')
$ciphertext = $null
try {
  if (-not [IO.Directory]::Exists($directory)) {
    [void] [IO.Directory]::CreateDirectory($directory)
  }
  Assert-NoReparse $directory
  [IO.Directory]::SetAccessControl($directory, (New-PrivateDirectoryAcl))
  Assert-PrivateAcl $directory $true
  if ([IO.File]::Exists($target) -or [IO.Directory]::Exists($target)) {
    throw 'target already exists'
  }
  $ciphertext = [Security.Cryptography.ProtectedData]::Protect(
    $plaintext,
    $entropy,
    [Security.Cryptography.DataProtectionScope]::CurrentUser
  )
  $stream = New-Object IO.FileStream(
    $temporary,
    [IO.FileMode]::CreateNew,
    [IO.FileAccess]::Write,
    [IO.FileShare]::None,
    4096,
    [IO.FileOptions]::WriteThrough
  )
  try {
    $stream.Write($ciphertext, 0, $ciphertext.Length)
    $stream.Flush($true)
  } finally {
    $stream.Dispose()
  }
  [IO.File]::SetAccessControl($temporary, (New-PrivateFileAcl))
  Assert-PrivateAcl $temporary $false
  [IO.File]::Move($temporary, $target)
  Assert-PrivateAcl $target $false
  [Console]::Out.Write('STORED')
} finally {
  if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
  [Array]::Clear($plaintext, 0, $plaintext.Length)
  if ($ciphertext -ne $null) { [Array]::Clear($ciphertext, 0, $ciphertext.Length) }
  [Array]::Clear($entropy, 0, $entropy.Length)
}
`;

function windowsPowerShellPath(): string {
  const systemRoot = process.env.SystemRoot?.trim() ?? process.env.WINDIR?.trim();
  if (
    !systemRoot ||
    !win32.isAbsolute(systemRoot) ||
    /[\0\r\n]/.test(systemRoot)
  ) {
    throw new AccountBridgeMasterKeyError(
      "master-key-secure-storage-unavailable",
    );
  }
  return win32.join(
    systemRoot,
    "System32",
    "WindowsPowerShell",
    "v1.0",
    "powershell.exe",
  );
}

export function createWindowsDPAPIBackend(input: {
  targetPath: string;
  powershellPath: string;
  run?: AccountBridgeSecureCommandRunner;
}): AccountBridgeMasterKeyBackend {
  if (
    !win32.isAbsolute(input.targetPath) ||
    /[\0\r\n]/.test(input.targetPath) ||
    !win32.isAbsolute(input.powershellPath) ||
    /[\0\r\n]/.test(input.powershellPath)
  ) {
    throw new AccountBridgeMasterKeyError(
      "master-key-secure-storage-unavailable",
    );
  }
  const run = input.run ?? runAccountBridgeSecureCommand;
  const encodedScript = Buffer.from(WINDOWS_DPAPI_SCRIPT, "utf16le").toString(
    "base64",
  );
  const execute = async (
    operation: "read" | "write",
    value?: string,
  ): Promise<AccountBridgeSecureCommandResult> => {
    const payload = Buffer.from(
      `${operation}\n${input.targetPath}\n${value ?? ""}`,
      "utf8",
    );
    try {
      return await run({
        command: input.powershellPath,
        args: [
          "-NoLogo",
          "-NoProfile",
          "-NonInteractive",
          "-ExecutionPolicy",
          "Bypass",
          "-EncodedCommand",
          encodedScript,
        ],
        input: payload,
        timeoutMs: COMMAND_TIMEOUT_MS,
      });
    } finally {
      payload.fill(0);
    }
  };
  return {
    async read() {
      const result = await execute("read");
      const output = result.stdout.trim();
      if (result.code !== 0) return { status: "error" };
      if (output === "ABSENT") return { status: "absent" };
      if (/^KEY:[A-Za-z0-9+/=]+$/.test(output)) {
        const raw = Buffer.from(output.slice(4), "base64");
        try {
          return raw.length === MASTER_KEY_BYTES
            ? { status: "data", value: raw.toString("base64url") }
            : { status: "error" };
        } finally {
          raw.fill(0);
        }
      }
      return { status: "error" };
    },
    async write(value) {
      canonicalMasterKey(value).fill(0);
      const result = await execute("write", value);
      if (result.code !== 0 || result.stdout.trim() !== "STORED") {
        throw new AccountBridgeMasterKeyError(
          "master-key-secure-storage-unavailable",
        );
      }
    },
  };
}

export function createLinuxSecretServiceBackend(input: {
  command: string;
  run?: AccountBridgeSecureCommandRunner;
}): AccountBridgeMasterKeyBackend {
  if (
    !input.command.startsWith("/") ||
    /[\0\r\n]/.test(input.command)
  ) {
    throw new AccountBridgeMasterKeyError(
      "master-key-secure-storage-unavailable",
    );
  }
  const run = input.run ?? runAccountBridgeSecureCommand;
  return {
    async read() {
      const result = await run({
        command: input.command,
        args: ["lookup", ...LINUX_SECRET_ATTRIBUTES],
        timeoutMs: COMMAND_TIMEOUT_MS,
      });
      if (result.code === 1) return { status: "absent" };
      if (result.code !== 0) return { status: "error" };
      const value = result.stdout.trim();
      return value.length > 0
        ? { status: "data", value }
        : { status: "error" };
    },
    async write(value) {
      canonicalMasterKey(value).fill(0);
      const payload = Buffer.from(`${value}\n`, "utf8");
      try {
        const result = await run({
          command: input.command,
          args: [
            "store",
            "--label=CrabCode Account Bridge master key",
            ...LINUX_SECRET_ATTRIBUTES,
          ],
          input: payload,
          timeoutMs: COMMAND_TIMEOUT_MS,
        });
        if (result.code !== 0) {
          throw new AccountBridgeMasterKeyError(
            "master-key-secure-storage-unavailable",
          );
        }
      } finally {
        payload.fill(0);
      }
    },
  };
}

async function resolveLinuxSecretTool(): Promise<string> {
  for (const candidate of LINUX_SECRET_TOOL_CANDIDATES) {
    try {
      const canonical = await fs.realpath(candidate);
      if (
        canonical !== "/usr/bin/secret-tool" &&
        canonical !== "/bin/secret-tool"
      ) {
        continue;
      }
      const stat = await fs.stat(canonical);
      if (
        !stat.isFile() ||
        (typeof stat.uid === "number" && stat.uid !== 0) ||
        (stat.mode & 0o022) !== 0
      ) {
        continue;
      }
      return canonical;
    } catch {
      // Try the next fixed system path. PATH discovery is intentionally absent.
    }
  }
  throw new AccountBridgeMasterKeyError(
    "master-key-secure-storage-unavailable",
  );
}

async function createProductionBackend(
  platform: "darwin" | "win32" | "linux",
): Promise<AccountBridgeMasterKeyBackend> {
  switch (platform) {
    case "darwin":
      return createDarwinKeychainBackend();
    case "win32":
      return createWindowsDPAPIBackend({
        targetPath: win32.join(
          configRoot(),
          "account-bridge",
          "master-key.dpapi",
        ),
        powershellPath: windowsPowerShellPath(),
      });
    case "linux":
      return createLinuxSecretServiceBackend({
        command: await resolveLinuxSecretTool(),
      });
  }
}

function productionMasterKeyDeps(): AccountBridgeMasterKeyDeps {
  const platform = process.platform;
  return {
    platform,
    randomBytes: size => randomBytes(size),
    withLock: operation => {
      if (platform !== "darwin" && platform !== "win32" && platform !== "linux") {
        throw new AccountBridgeMasterKeyError(
          "master-key-secure-storage-unavailable",
        );
      }
      return productionLock(platform, operation);
    },
    createBackend: createProductionBackend,
  };
}
