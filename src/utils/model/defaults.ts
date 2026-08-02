export const DEFAULT_MAIN_LOOP_MODEL = 'deepseek-v4-flash'

// MAIN_LOOP_FALLBACK_MODEL 已删除（2026-07-27）。它长期指向 'qwen3.6-plus'，而
// 目录升版到 'qwen3.7-plus' 后该 slug 在生产 catalog 的 model_id 与 name 中都不
// 再出现 —— 两处消费点（getDefaultMainLoopModelSetting / getDefaultFallbackModel）
// 的匹配恒不命中，常量只剩「解析失败时返回一个网关不服务的 slug」这一个效果。
// 换成新字面量只是把同一颗雷推到下次升版：第二个 slug 字面量本身就是漂移面，
// 且 §硬约束 #1 只认可 DEFAULT_MAIN_LOOP_MODEL / DEFAULT_SMALL_FAST_MODEL 这类
// 唯一落点。fallback 改由目录 / SDK 默认解析，漂移面归零。
// 同源存量见审计 doc §4（服务端两个同因死桶）。

// 指定的小/快模型默认值（W-SMALL-MODEL-UNIFY 2026-06-19）。
// 上游 SDK 网关始终提供 `deepseek-v4-flash`，故作为 getSmallFastModel 的
// 默认主项；当 catalog 校验未命中（企业 allowlist 禁用 / 网关下架）时由
// getSmallFastModel 回落到用户当前主会话模型。与 DEFAULT_MAIN_LOOP_MODEL
// 同范式 —— 这是 §1 反 hardcode 闸门唯一认可的 slug 落点。
export const DEFAULT_SMALL_FAST_MODEL = 'deepseek-v4-flash'
