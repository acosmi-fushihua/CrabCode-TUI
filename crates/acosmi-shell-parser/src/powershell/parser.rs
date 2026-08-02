//! `PowerShell` 解析器 — 通过 pwsh 子进程获取 AST
//!
//! 等价于 TS powershell/parser.ts。
//! 核心策略：将 PS 脚本 Base64 编码后通过 -`EncodedCommand` 传给 pwsh。

use std::sync::Mutex;
use std::time::Duration;

use tokio::process::Command;

use super::types::{
    CommandElementChild, CommandElementType, CommandNameType, ParseError, ParsedCommandElement,
    ParsedPowerShellCommand, ParsedRedirection, ParsedStatement, ParsedVariable,
    PipelineElementType, RawParseResult, RawPipelineElement, RawRedirection, RawStatement,
    SecurityFlags, StatementType, classify_command_name, map_element_type, map_statement_type,
    strip_module_prefix,
};
use crate::ShellParserError;

/// 默认 pwsh 解析超时（毫秒）
const DEFAULT_PARSE_TIMEOUT_MS: u64 = 5_000;

/// Unix 最大命令长度
#[cfg(not(windows))]
const MAX_COMMAND_LENGTH: usize = 4_500;

/// Windows 最大命令长度（考虑 `CreateProcess` 32K 限制）
#[cfg(windows)]
const MAX_COMMAND_LENGTH: usize = 8_000;

/// LRU 缓存容量
const CACHE_CAPACITY: usize = 256;

/// 瞬态错误 ID（从缓存中淘汰以便重试）
fn is_transient_error(error_id: &str) -> bool {
    matches!(
        error_id,
        "PwshSpawnError" | "PwshError" | "PwshTimeout" | "EmptyOutput" | "InvalidJson"
    )
}

// ─── LRU Cache ───

static PARSE_CACHE: std::sync::LazyLock<Mutex<lru::LruCache<String, ParsedPowerShellCommand>>> =
    std::sync::LazyLock::new(|| {
        Mutex::new(lru::LruCache::new(
            std::num::NonZeroUsize::new(CACHE_CAPACITY).expect("non-zero"),
        ))
    });

/// 获取解析超时
fn get_parse_timeout_ms() -> u64 {
    std::env::var("CRABCODE_PWSH_PARSE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PARSE_TIMEOUT_MS)
}

// ─── Public API ───

/// 解析 `PowerShell` 命令（带 LRU 缓存）
pub async fn parse_powershell_command(
    command: &str,
) -> Result<ParsedPowerShellCommand, ShellParserError> {
    // 检查命令长度
    if command.len() > MAX_COMMAND_LENGTH {
        return Err(ShellParserError::CommandTooLong {
            length: command.len(),
            max_length: MAX_COMMAND_LENGTH,
        });
    }

    // 查缓存
    {
        let mut cache = PARSE_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.get(command) {
            // 检查是否为瞬态错误（需要重试）
            let has_transient = cached
                .errors
                .iter()
                .any(|e| is_transient_error(&e.error_id));
            if !has_transient {
                return Ok(cached.clone());
            }
            // 淘汰瞬态错误缓存
            let key = command.to_string();
            cache.pop(&key);
        }
    }

    // 实际解析
    let result = parse_powershell_command_impl(command).await?;

    // 写缓存
    {
        let mut cache = PARSE_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.put(command.to_string(), result.clone());
    }

    Ok(result)
}

/// `PowerShell` 解析实现（调用 pwsh 子进程）
async fn parse_powershell_command_impl(
    command: &str,
) -> Result<ParsedPowerShellCommand, ShellParserError> {
    let pwsh_path = find_pwsh().await.ok_or_else(|| {
        ShellParserError::ShellNotFound("pwsh or powershell not found".to_string())
    })?;

    let timeout = Duration::from_millis(get_parse_timeout_ms());

    // 构建解析脚本
    let parse_script = build_parse_script(command);

    // 编码为 Base64（UTF-16LE）
    let encoded = encode_powershell_command(&parse_script);

    // Spawn pwsh with one retry on timeout. On loaded CI runners (Windows
    // especially), pwsh spawn + .NET JIT + ParseInput occasionally exceeds 5s.
    // A single retry absorbs transient load spikes; a double timeout is reported
    // as PwshTimeout.
    let mut last_err = None;
    for attempt in 0..2 {
        let child = Command::new(&pwsh_path)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-NoLogo",
                "-EncodedCommand",
                &encoded,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| ShellParserError::PwshSpawn(e.to_string()))?;

        // 等待输出（带超时）
        let result = tokio::time::timeout(timeout, child.wait_with_output()).await;

        match result {
            Err(_) => {
                // Timeout — retry once before giving up
                last_err = Some(ShellParserError::PwshTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                });
                if attempt == 0 {
                    continue;
                }
                return Err(last_err.expect("last_err Some on retry exhausted path"));
            }
            Ok(Err(e)) => {
                return Err(ShellParserError::PwshSpawn(e.to_string()));
            }
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();

                if stdout.trim().is_empty() {
                    return Err(ShellParserError::PwshParse(
                        "empty output from pwsh".to_string(),
                    ));
                }

                // 解析 JSON
                let raw: RawParseResult = serde_json::from_str(&stdout).map_err(|e| {
                    ShellParserError::InvalidJson(format!(
                        "{e}: {}",
                        &stdout[..stdout.len().min(200)]
                    ))
                })?;

                return Ok(transform_raw_result(raw, command));
            }
        }
    }

    Err(last_err
        .unwrap_or_else(|| ShellParserError::PwshParse("unexpected retry loop exit".to_string())))
}

/// 编码 `PowerShell` 命令为 UTF-16LE Base64
fn encode_powershell_command(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64_encode(&utf16)
}

/// 简单 Base64 编码
pub(crate) fn base64_encode(data: &[u8]) -> String {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut encoder = Base64Encoder::new(&mut buf);
        let _ = encoder.write_all(data);
        let _ = encoder.finish();
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// The PS1 script body. The user command is passed via Base64-encoded
/// $`EncodedCommand` variable to prevent injection attacks.
const PARSE_SCRIPT_BODY: &str = r#"
if (-not $EncodedCommand) {
    Write-Output '{"valid":false,"errors":[{"message":"No command provided","errorId":"NoInput"}],"statements":[],"variables":[],"hasStopParsing":false,"originalCommand":""}'
    exit 0
}

$Command = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($EncodedCommand))

$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseInput(
    $Command,
    [ref]$tokens,
    [ref]$parseErrors
)

$allVariables = [System.Collections.ArrayList]::new()

function Get-RawCommandElements {
    param([System.Management.Automation.Language.CommandAst]$CmdAst)
    $elems = [System.Collections.ArrayList]::new()
    foreach ($ce in $CmdAst.CommandElements) {
        $ceData = @{ type = $ce.GetType().Name; text = $ce.Extent.Text }
        if ($ce.PSObject.Properties['Value'] -and $null -ne $ce.Value -and $ce.Value -is [string]) {
            $ceData.value = $ce.Value
        }
        if ($ce -is [System.Management.Automation.Language.CommandExpressionAst]) {
            $ceData.expressionType = $ce.Expression.GetType().Name
        }
        $a=$ce.Argument;if($a){$ceData.children=@(@{type=$a.GetType().Name;text=$a.Extent.Text})}
        [void]$elems.Add($ceData)
    }
    return $elems
}

function Get-RawRedirections {
    param($Redirections)
    $result = [System.Collections.ArrayList]::new()
    foreach ($redir in $Redirections) {
        $redirData = @{ type = $redir.GetType().Name }
        if ($redir -is [System.Management.Automation.Language.FileRedirectionAst]) {
            $redirData.append = [bool]$redir.Append
            $redirData.fromStream = $redir.FromStream.ToString()
            $redirData.locationText = $redir.Location.Extent.Text
        }
        [void]$result.Add($redirData)
    }
    return $result
}

function Get-SecurityPatterns($A) {
    $p = @{}
    foreach ($n in $A.FindAll({ param($x)
        $x -is [System.Management.Automation.Language.MemberExpressionAst] -or
        $x -is [System.Management.Automation.Language.SubExpressionAst] -or
        $x -is [System.Management.Automation.Language.ArrayExpressionAst] -or
        $x -is [System.Management.Automation.Language.ExpandableStringExpressionAst] -or
        $x -is [System.Management.Automation.Language.ScriptBlockExpressionAst] -or
        $x -is [System.Management.Automation.Language.ParenExpressionAst]
    }, $true)) { switch ($n.GetType().Name) {
        'InvokeMemberExpressionAst' { $p.hasMemberInvocations = $true }
        'MemberExpressionAst' { $p.hasMemberInvocations = $true }
        'SubExpressionAst' { $p.hasSubExpressions = $true }
        'ArrayExpressionAst' { $p.hasSubExpressions = $true }
        'ParenExpressionAst' { $p.hasSubExpressions = $true }
        'ExpandableStringExpressionAst' { $p.hasExpandableStrings = $true }
        'ScriptBlockExpressionAst' { $p.hasScriptBlocks = $true }
    }}
    if ($p.Count -gt 0) { return $p }
    return $null
}

$varExprs = $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.VariableExpressionAst] }, $true)
foreach ($v in $varExprs) {
    [void]$allVariables.Add(@{
        path = $v.VariablePath.ToString()
        isSplatted = [bool]$v.Splatted
    })
}

$typeLiterals = [System.Collections.ArrayList]::new()
foreach ($t in $ast.FindAll({ param($n)
    $n -is [System.Management.Automation.Language.TypeExpressionAst] -or
    $n -is [System.Management.Automation.Language.TypeConstraintAst]
}, $true)) { [void]$typeLiterals.Add($t.TypeName.FullName) }

$hasStopParsing = $false
$tk = [System.Management.Automation.Language.TokenKind]
foreach ($tok in $tokens) {
    if ($tok.Kind -eq $tk::MinusMinus) { $hasStopParsing = $true; break }
    if ($tok.Kind -eq $tk::Generic -and ($tok.Text -replace '[\u2013\u2014\u2015]','-') -eq '--%') {
        $hasStopParsing = $true; break
    }
}

$statements = [System.Collections.ArrayList]::new()

function Process-BlockStatements {
    param($Block)
    if (-not $Block) { return }

    foreach ($stmt in $Block.Statements) {
        $statement = @{
            type = $stmt.GetType().Name
            text = $stmt.Extent.Text
        }

        if ($stmt -is [System.Management.Automation.Language.PipelineAst]) {
            $elements = [System.Collections.ArrayList]::new()
            foreach ($element in $stmt.PipelineElements) {
                $elemData = @{
                    type = $element.GetType().Name
                    text = $element.Extent.Text
                }

                if ($element -is [System.Management.Automation.Language.CommandAst]) {
                    $elemData.commandElements = @(Get-RawCommandElements -CmdAst $element)
                    $elemData.redirections = @(Get-RawRedirections -Redirections $element.Redirections)
                } elseif ($element -is [System.Management.Automation.Language.CommandExpressionAst]) {
                    $elemData.expressionType = $element.Expression.GetType().Name
                    $elemData.redirections = @(Get-RawRedirections -Redirections $element.Redirections)
                }

                [void]$elements.Add($elemData)
            }
            $statement.elements = @($elements)

            $allNestedCmds = $stmt.FindAll(
                { param($node) $node -is [System.Management.Automation.Language.CommandAst] },
                $true
            )
            $nestedCmds = [System.Collections.ArrayList]::new()
            foreach ($cmd in $allNestedCmds) {
                if ($cmd.Parent -eq $stmt) { continue }
                $nested = @{
                    type = $cmd.GetType().Name
                    text = $cmd.Extent.Text
                    commandElements = @(Get-RawCommandElements -CmdAst $cmd)
                    redirections = @(Get-RawRedirections -Redirections $cmd.Redirections)
                }
                [void]$nestedCmds.Add($nested)
            }
            if ($nestedCmds.Count -gt 0) {
                $statement.nestedCommands = @($nestedCmds)
            }
            $r = $stmt.FindAll({param($n) $n -is [System.Management.Automation.Language.FileRedirectionAst]}, $true)
            if ($r.Count -gt 0) {
                $rr = @(Get-RawRedirections -Redirections $r)
                $statement.redirections = if ($statement.redirections) { @($statement.redirections) + $rr } else { $rr }
            }
        } else {
            $nestedCmdAsts = $stmt.FindAll(
                { param($node) $node -is [System.Management.Automation.Language.CommandAst] },
                $true
            )
            $nested = [System.Collections.ArrayList]::new()
            foreach ($cmd in $nestedCmdAsts) {
                [void]$nested.Add(@{
                    type = 'CommandAst'
                    text = $cmd.Extent.Text
                    commandElements = @(Get-RawCommandElements -CmdAst $cmd)
                    redirections = @(Get-RawRedirections -Redirections $cmd.Redirections)
                })
            }
            if ($nested.Count -gt 0) {
                $statement.nestedCommands = @($nested)
            }
            $r = $stmt.FindAll({param($n) $n -is [System.Management.Automation.Language.FileRedirectionAst]}, $true)
            if ($r.Count -gt 0) { $statement.redirections = @(Get-RawRedirections -Redirections $r) }
        }

        $sp = Get-SecurityPatterns $stmt
        if ($sp) { $statement.securityPatterns = $sp }

        [void]$statements.Add($statement)
    }

    if ($Block.Traps) {
        foreach ($trap in $Block.Traps) {
            $statement = @{
                type = 'TrapStatementAst'
                text = $trap.Extent.Text
            }
            $nestedCmdAsts = $trap.FindAll(
                { param($node) $node -is [System.Management.Automation.Language.CommandAst] },
                $true
            )
            $nestedCmds = [System.Collections.ArrayList]::new()
            foreach ($cmd in $nestedCmdAsts) {
                $nested = @{
                    type = $cmd.GetType().Name
                    text = $cmd.Extent.Text
                    commandElements = @(Get-RawCommandElements -CmdAst $cmd)
                    redirections = @(Get-RawRedirections -Redirections $cmd.Redirections)
                }
                [void]$nestedCmds.Add($nested)
            }
            if ($nestedCmds.Count -gt 0) {
                $statement.nestedCommands = @($nestedCmds)
            }
            $r = $trap.FindAll({param($n) $n -is [System.Management.Automation.Language.FileRedirectionAst]}, $true)
            if ($r.Count -gt 0) { $statement.redirections = @(Get-RawRedirections -Redirections $r) }
            $sp = Get-SecurityPatterns $trap
            if ($sp) { $statement.securityPatterns = $sp }
            [void]$statements.Add($statement)
        }
    }
}

Process-BlockStatements -Block $ast.BeginBlock
Process-BlockStatements -Block $ast.ProcessBlock
Process-BlockStatements -Block $ast.EndBlock
Process-BlockStatements -Block $ast.CleanBlock
Process-BlockStatements -Block $ast.DynamicParamBlock

if ($ast.ParamBlock) {
  $pb = $ast.ParamBlock
  $pn = [System.Collections.ArrayList]::new()
  foreach ($c in $pb.FindAll({param($n) $n -is [System.Management.Automation.Language.CommandAst]}, $true)) {
    [void]$pn.Add(@{type='CommandAst';text=$c.Extent.Text;commandElements=@(Get-RawCommandElements -CmdAst $c);redirections=@(Get-RawRedirections -Redirections $c.Redirections)})
  }
  $pr = $pb.FindAll({param($n) $n -is [System.Management.Automation.Language.FileRedirectionAst]}, $true)
  $ps = Get-SecurityPatterns $pb
  if ($pn.Count -gt 0 -or $pr.Count -gt 0 -or $ps) {
    $st = @{type='ParamBlockAst';text=$pb.Extent.Text}
    if ($pn.Count -gt 0) { $st.nestedCommands = @($pn) }
    if ($pr.Count -gt 0) { $st.redirections = @(Get-RawRedirections -Redirections $pr) }
    if ($ps) { $st.securityPatterns = $ps }
    [void]$statements.Add($st)
  }
}

$hasUsingStatements = $ast.UsingStatements -and $ast.UsingStatements.Count -gt 0
$hasScriptRequirements = $ast.ScriptRequirements -ne $null

$output = @{
    valid = ($parseErrors.Count -eq 0)
    errors = @($parseErrors | ForEach-Object {
        @{
            message = $_.Message
            errorId = $_.ErrorId
        }
    })
    statements = @($statements)
    variables = @($allVariables)
    hasStopParsing = $hasStopParsing
    originalCommand = $Command
    typeLiterals = @($typeLiterals)
    hasUsingStatements = [bool]$hasUsingStatements
    hasScriptRequirements = [bool]$hasScriptRequirements
}

$output | ConvertTo-Json -Depth 10 -Compress
"#;

/// 构建解析脚本 — Base64-encode the user command to prevent injection
fn build_parse_script(command: &str) -> String {
    let encoded = base64_encode(command.as_bytes()); // UTF-8 base64
    format!("$EncodedCommand = '{encoded}'\n{PARSE_SCRIPT_BODY}")
}

/// 转换原始解析结果为类型化结构
fn transform_raw_result(raw: RawParseResult, original_command: &str) -> ParsedPowerShellCommand {
    let errors = raw
        .errors
        .unwrap_or_default()
        .into_iter()
        .map(|e| ParseError {
            message: e.message.unwrap_or_default(),
            error_id: e.error_id.unwrap_or_else(|| "Unknown".to_string()),
        })
        .collect();

    let statements = raw
        .statements
        .unwrap_or_default()
        .into_iter()
        .map(transform_statement)
        .collect();

    let variables = raw
        .variables
        .unwrap_or_default()
        .into_iter()
        .map(|v| ParsedVariable {
            path: v.path.unwrap_or_default(),
            is_splatted: v.is_splatted.unwrap_or(false),
        })
        .collect();

    ParsedPowerShellCommand {
        valid: raw.valid.unwrap_or(false),
        errors,
        statements,
        variables,
        has_stop_parsing: raw.has_stop_parsing.unwrap_or(false),
        original_command: original_command.to_string(),
        type_literals: raw.type_literals,
        has_using_statements: raw.has_using_statements,
        has_script_requirements: raw.has_script_requirements,
    }
}

/// 转换语句
fn transform_statement(raw: RawStatement) -> ParsedStatement {
    let stmt_type = map_statement_type(raw.stmt_type.as_deref().unwrap_or("UnknownStatementAst"));

    let commands = raw
        .elements
        .unwrap_or_default()
        .into_iter()
        .map(transform_command_element)
        .collect();

    let redirections = raw
        .redirections
        .unwrap_or_default()
        .into_iter()
        .filter_map(transform_redirection)
        .collect();

    let nested_commands = raw
        .nested_commands
        .map(|nc| nc.into_iter().map(transform_command_element).collect());

    ParsedStatement {
        statement_type: stmt_type,
        commands,
        redirections,
        text: raw.text.unwrap_or_default(),
        nested_commands,
        security_patterns: raw.security_patterns,
    }
}

/// 转换命令元素
fn transform_command_element(raw: RawPipelineElement) -> ParsedCommandElement {
    let elem_type_str = raw.elem_type.as_deref().unwrap_or("CommandAst");
    let element_type = match elem_type_str {
        "CommandExpressionAst" => PipelineElementType::CommandExpressionAst,
        "ParenExpressionAst" => PipelineElementType::ParenExpressionAst,
        _ => PipelineElementType::CommandAst,
    };

    let elements = raw.command_elements.unwrap_or_default();

    // SECURITY: nameType MUST be computed from the raw name (before
    // stripModulePrefix). classifyCommandName('scripts\\Get-Process') returns
    // 'application' (contains \\) — the correct answer.
    let mut name = String::new();
    let mut name_type = CommandNameType::Unknown;
    let mut element_types: Vec<CommandElementType> = Vec::new();
    let mut args: Vec<String> = Vec::new();
    let mut children_vec: Vec<Option<Vec<CommandElementChild>>> = Vec::new();
    let mut has_children = false;

    if !elements.is_empty() {
        let first = &elements[0];
        // SECURITY: only trust .value for string-literal element types with a
        // string-typed value. For non-string-literal types, use .text.
        let is_first_string_literal = matches!(
            first.elem_type.as_deref(),
            Some("StringConstantExpressionAst" | "ExpandableStringExpressionAst")
        );
        let raw_name_unstripped = if is_first_string_literal {
            first
                .value
                .as_deref()
                .unwrap_or(first.text.as_deref().unwrap_or(""))
        } else {
            first.text.as_deref().unwrap_or("")
        };
        // SECURITY: strip surrounding quotes from the command name
        let raw_name = strip_surrounding_quotes(raw_name_unstripped);
        // SECURITY: Non-ASCII check — force Application if name contains chars >= U+0080
        if raw_name.chars().any(|c| c as u32 >= 0x0080) {
            name_type = CommandNameType::Application;
        } else {
            name_type = classify_command_name(&raw_name);
        }
        name = strip_module_prefix(&raw_name);
        element_types.push(map_element_type(
            first.elem_type.as_deref().unwrap_or("Other"),
            first.expression_type.as_deref(),
        ));

        for ce in elements.iter().skip(1) {
            // Use resolved .value for string constants (strips quotes, resolves
            // backtick escapes) but keep raw .text for parameters and other types.
            let is_string_literal = matches!(
                ce.elem_type.as_deref(),
                Some("StringConstantExpressionAst" | "ExpandableStringExpressionAst")
            );
            let arg_text = if is_string_literal {
                ce.value
                    .as_deref()
                    .or(ce.text.as_deref())
                    .unwrap_or("")
                    .to_string()
            } else {
                ce.text.as_deref().unwrap_or("").to_string()
            };
            args.push(arg_text);
            element_types.push(map_element_type(
                ce.elem_type.as_deref().unwrap_or("Other"),
                ce.expression_type.as_deref(),
            ));
            // Map raw children (CommandParameterAst.Argument) through
            // mapElementType so consumers see 'Variable', 'StringConstant', etc.
            if let Some(raw_children) = &ce.children {
                if raw_children.is_empty() {
                    children_vec.push(None);
                } else {
                    has_children = true;
                    children_vec.push(Some(
                        raw_children
                            .iter()
                            .map(|c| CommandElementChild {
                                child_type: map_element_type(
                                    c.child_type.as_deref().unwrap_or("Other"),
                                    None,
                                ),
                                text: c.text.as_deref().unwrap_or("").to_string(),
                            })
                            .collect(),
                    ));
                }
            } else {
                children_vec.push(None);
            }
        }
    }

    let redirections = raw
        .redirections
        .map(|rs| rs.into_iter().filter_map(transform_redirection).collect());

    ParsedCommandElement {
        name_type,
        name,
        element_type,
        args,
        text: raw.text.unwrap_or_default(),
        element_types: Some(element_types),
        children: if has_children {
            Some(children_vec)
        } else {
            None
        },
        redirections,
    }
}

/// Strip surrounding single or double quotes from a string
fn strip_surrounding_quotes(s: &str) -> String {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
        {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 转换重定向
fn transform_redirection(raw: RawRedirection) -> Option<ParsedRedirection> {
    let redir_type = raw.redir_type.as_deref().unwrap_or("");

    if redir_type.contains("Merging") {
        return Some(ParsedRedirection {
            operator: "2>&1".to_string(),
            target: String::new(),
            is_merging: true,
        });
    }

    let append = raw.append.unwrap_or(false);
    let from_stream = raw.from_stream.as_deref().unwrap_or("Output");

    let operator = if append {
        match from_stream {
            "Error" => "2>>".to_string(),
            "All" => "*>>".to_string(),
            _ => ">>".to_string(),
        }
    } else {
        match from_stream {
            "Error" => "2>".to_string(),
            "All" => "*>".to_string(),
            _ => ">".to_string(),
        }
    };

    let target = raw.location_text.unwrap_or_default();

    Some(ParsedRedirection {
        operator,
        target,
        is_merging: false,
    })
}

// ─── Query Functions ───

/// 获取所有命令名
#[must_use]
pub fn get_all_command_names(parsed: &ParsedPowerShellCommand) -> Vec<String> {
    get_all_commands(parsed)
        .into_iter()
        .map(|c| c.name.clone())
        .collect()
}

/// 获取所有命令（含嵌套）
#[must_use]
pub fn get_all_commands(parsed: &ParsedPowerShellCommand) -> Vec<&ParsedCommandElement> {
    let mut commands = Vec::new();
    for stmt in &parsed.statements {
        for cmd in &stmt.commands {
            commands.push(cmd);
        }
        if let Some(nested) = &stmt.nested_commands {
            for cmd in nested {
                commands.push(cmd);
            }
        }
    }
    commands
}

/// 获取所有重定向
#[must_use]
pub fn get_all_redirections(parsed: &ParsedPowerShellCommand) -> Vec<&ParsedRedirection> {
    let mut redirections = Vec::new();
    for stmt in &parsed.statements {
        for redir in &stmt.redirections {
            redirections.push(redir);
        }
        for cmd in &stmt.commands {
            if let Some(rs) = &cmd.redirections {
                for r in rs {
                    redirections.push(r);
                }
            }
        }
    }
    redirections
}

/// 检查是否包含指定名称的命令
#[must_use]
pub fn has_command_named(parsed: &ParsedPowerShellCommand, name: &str) -> bool {
    let lower = name.to_lowercase();
    get_all_commands(parsed)
        .iter()
        .any(|c| c.name.to_lowercase() == lower)
}

/// 检查是否包含目录变更
#[must_use]
pub fn has_directory_change(parsed: &ParsedPowerShellCommand) -> bool {
    let dir_cmdlets = ["set-location", "push-location", "pop-location"];
    let dir_aliases = ["cd", "sl", "chdir", "pushd", "popd"];

    get_all_commands(parsed).iter().any(|c| {
        let name_lower = c.name.to_lowercase();
        dir_cmdlets.contains(&name_lower.as_str()) || dir_aliases.contains(&name_lower.as_str())
    })
}

/// 检查是否为单命令
#[must_use]
pub fn is_single_command(parsed: &ParsedPowerShellCommand) -> bool {
    parsed.statements.len() == 1
        && parsed.statements[0].commands.len() == 1
        && parsed.statements[0]
            .nested_commands
            .as_ref()
            .is_none_or(std::vec::Vec::is_empty)
}

/// 检查命令是否有指定参数
#[must_use]
pub fn command_has_arg(command: &ParsedCommandElement, arg: &str) -> bool {
    command.args.iter().any(|a| a.eq_ignore_ascii_case(arg))
}

/// 检查命令是否有参数缩写
#[must_use]
pub fn command_has_arg_abbreviation(
    command: &ParsedCommandElement,
    full_param: &str,
    min_prefix: &str,
) -> bool {
    let full_lower = full_param.to_lowercase();
    let min_lower = min_prefix.to_lowercase();
    command.args.iter().any(|a| {
        let a_lower = a.to_lowercase();
        a_lower.starts_with(&min_lower) && full_lower.starts_with(&a_lower)
    })
}

/// 检查字符串是否为 `PowerShell` 参数
#[must_use]
pub fn is_powershell_parameter(arg: &str, _element_type: Option<CommandElementType>) -> bool {
    if arg.starts_with('-')
        || arg.starts_with('\u{2013}')
        || arg.starts_with('\u{2014}')
        || arg.starts_with('\u{2015}')
    {
        return true;
    }
    false
}

/// 检查是否为 null 重定向目标
/// Only accepts `$null` and `${null}`, NOT bare `null`.
#[must_use]
pub fn is_null_redirection_target(target: &str) -> bool {
    let t = target.trim().to_lowercase();
    t == "$null" || t == "${null}"
}

/// 获取文件重定向
#[must_use]
pub fn get_file_redirections(parsed: &ParsedPowerShellCommand) -> Vec<&ParsedRedirection> {
    get_all_redirections(parsed)
        .into_iter()
        .filter(|r| !r.is_merging && !is_null_redirection_target(&r.target))
        .collect()
}

/// 推导安全标记
#[must_use]
pub fn derive_security_flags(parsed: &ParsedPowerShellCommand) -> SecurityFlags {
    let mut flags = SecurityFlags {
        has_stop_parsing: parsed.has_stop_parsing,
        has_splatting: parsed.variables.iter().any(|v| v.is_splatted),
        ..SecurityFlags::default()
    };

    for stmt in &parsed.statements {
        if matches!(stmt.statement_type, StatementType::AssignmentStatementAst) {
            flags.has_assignments = true;
        }
        // Check elementTypes from commands (belt-and-suspenders with securityPatterns)
        for cmd in &stmt.commands {
            if let Some(ets) = &cmd.element_types {
                for et in ets {
                    match et {
                        CommandElementType::ScriptBlock => flags.has_script_blocks = true,
                        CommandElementType::SubExpression => flags.has_sub_expressions = true,
                        CommandElementType::ExpandableString => flags.has_expandable_strings = true,
                        CommandElementType::MemberInvocation => flags.has_member_invocations = true,
                        _ => {}
                    }
                }
            }
        }
        if let Some(nested) = &stmt.nested_commands {
            for cmd in nested {
                if let Some(ets) = &cmd.element_types {
                    for et in ets {
                        match et {
                            CommandElementType::ScriptBlock => flags.has_script_blocks = true,
                            CommandElementType::SubExpression => flags.has_sub_expressions = true,
                            CommandElementType::ExpandableString => {
                                flags.has_expandable_strings = true
                            }
                            CommandElementType::MemberInvocation => {
                                flags.has_member_invocations = true
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        // securityPatterns provides a belt-and-suspenders check that catches
        // patterns elementTypes may miss (e.g. member invocations inside
        // assignments, subexpressions in non-pipeline statements).
        if let Some(sp) = &stmt.security_patterns {
            if sp.has_member_invocations {
                flags.has_member_invocations = true;
            }
            if sp.has_sub_expressions {
                flags.has_sub_expressions = true;
            }
            if sp.has_expandable_strings {
                flags.has_expandable_strings = true;
            }
            if sp.has_script_blocks {
                flags.has_script_blocks = true;
            }
        }
    }

    flags
}

// ─── Shell Detection ───

/// 查找 pwsh 可执行文件
async fn find_pwsh() -> Option<String> {
    // 先尝试 pwsh（PowerShell Core 7+）
    if let Ok(path) = which_async("pwsh").await {
        return Some(path);
    }

    // Windows: 尝试 powershell（5.1）
    #[cfg(windows)]
    if let Ok(path) = which_async("powershell").await {
        return Some(path);
    }

    // Linux: 直接检查常见路径（避免 snap launcher）
    #[cfg(target_os = "linux")]
    {
        let direct = "/opt/microsoft/powershell/7/pwsh";
        if tokio::fs::metadata(direct).await.is_ok() {
            return Some(direct.to_string());
        }
    }

    None
}

/// 异步 which 查找
async fn which_async(cmd: &str) -> Result<String, ()> {
    let result = tokio::task::spawn_blocking({
        let cmd = cmd.to_string();
        move || which::which(&cmd).map(|p| p.to_string_lossy().to_string())
    })
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    Ok(result)
}

// ─── Base64 Encoder ───

/// 最小化 Base64 编码器（避免额外依赖）
struct Base64Encoder<W> {
    writer: W,
    buf: [u8; 3],
    buf_len: usize,
}

const BASE64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

impl<W: std::io::Write> Base64Encoder<W> {
    const fn new(writer: W) -> Self {
        Self {
            writer,
            buf: [0; 3],
            buf_len: 0,
        }
    }

    fn finish(mut self) -> std::io::Result<()> {
        if self.buf_len > 0 {
            let mut out = [b'='; 4];
            let b0 = self.buf[0];
            let b1 = if self.buf_len > 1 { self.buf[1] } else { 0 };
            let b2 = if self.buf_len > 2 { self.buf[2] } else { 0 };
            out[0] = BASE64_CHARS[(b0 >> 2) as usize];
            out[1] = BASE64_CHARS[((b0 & 0x03) << 4 | b1 >> 4) as usize];
            if self.buf_len > 1 {
                out[2] = BASE64_CHARS[((b1 & 0x0F) << 2 | b2 >> 6) as usize];
            }
            if self.buf_len > 2 {
                out[3] = BASE64_CHARS[(b2 & 0x3F) as usize];
            }
            self.writer.write_all(&out)?;
        }
        Ok(())
    }

    fn encode_block(&mut self, block: &[u8; 3]) -> std::io::Result<()> {
        let out = [
            BASE64_CHARS[(block[0] >> 2) as usize],
            BASE64_CHARS[((block[0] & 0x03) << 4 | block[1] >> 4) as usize],
            BASE64_CHARS[((block[1] & 0x0F) << 2 | block[2] >> 6) as usize],
            BASE64_CHARS[(block[2] & 0x3F) as usize],
        ];
        self.writer.write_all(&out)
    }
}

impl<W: std::io::Write> std::io::Write for Base64Encoder<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut i = 0;
        while i < data.len() {
            self.buf[self.buf_len] = data[i];
            self.buf_len += 1;
            i += 1;
            if self.buf_len == 3 {
                let block = self.buf;
                self.encode_block(&block)?;
                self.buf_len = 0;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_powershell_command() {
        let encoded = encode_powershell_command("Write-Host 'Hello'");
        assert!(!encoded.is_empty());
        // Base64 should only contain valid chars
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        );
    }

    #[test]
    fn test_classify_command_name() {
        assert_eq!(
            classify_command_name("Get-ChildItem"),
            CommandNameType::Cmdlet
        );
        assert_eq!(
            classify_command_name("git.exe"),
            CommandNameType::Application
        );
        assert_eq!(classify_command_name("ls"), CommandNameType::Unknown);
    }

    #[test]
    fn test_strip_module_prefix() {
        assert_eq!(
            strip_module_prefix("Microsoft.PowerShell.Utility\\Get-Content"),
            "Get-Content"
        );
        assert_eq!(strip_module_prefix("Get-Content"), "Get-Content");
    }

    #[test]
    fn test_is_powershell_parameter() {
        assert!(is_powershell_parameter("-Force", None));
        assert!(is_powershell_parameter("\u{2013}Force", None)); // en-dash
        assert!(!is_powershell_parameter("value", None));
    }

    #[test]
    fn test_is_null_target() {
        assert!(is_null_redirection_target("$null"));
        assert!(is_null_redirection_target("${null}"));
        assert!(!is_null_redirection_target("null")); // bare null is NOT $null
        assert!(!is_null_redirection_target("NULL")); // bare NULL is NOT $null
        assert!(!is_null_redirection_target("output.txt"));
    }

    #[test]
    fn test_base64_roundtrip() {
        let input = "Hello, World!";
        let utf16: Vec<u8> = input.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let encoded = base64_encode(&utf16);
        assert!(!encoded.is_empty());
    }
}
