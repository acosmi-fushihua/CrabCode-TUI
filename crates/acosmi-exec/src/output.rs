//! stdout/stderr 输出捕获与管理
//!
//! 提供 `OutputCollector`，支持流式读取并自动截断超长输出。
//! 截断时确保 UTF-8 安全，不会在多字节字符中间切断。

use tokio::io::AsyncReadExt;
use tracing::debug;

/// 输出收集器
///
/// 从异步读取器流式读取数据，限制最大字节数。
/// 超过限制时自动截断并追加截断提示消息。
///
/// # 示例
/// ```no_run
/// use acosmi_exec::output::OutputCollector;
///
/// # async fn example() {
/// let reader: &[u8] = b"hello world";
/// let mut collector = OutputCollector::new(1024);
/// let output = collector.collect_from(reader).await;
/// assert_eq!(output, "hello world");
/// # }
/// ```
#[derive(Debug)]
pub struct OutputCollector {
    /// 最大允许字节数
    max_bytes: usize,
    /// 内部缓冲区
    buffer: Vec<u8>,
    /// 是否已被截断
    truncated: bool,
}

impl OutputCollector {
    /// 创建新的输出收集器
    ///
    /// # 参数
    /// - `max_bytes`: 最大允许的字节数，超过此限制的输出将被截断
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            buffer: Vec::with_capacity(max_bytes.min(64 * 1024)), // 预分配最多 64KB
            truncated: false,
        }
    }

    /// 从异步读取器流式收集输出
    ///
    /// 持续读取直到 EOF 或达到字节限制。
    /// 返回 UTF-8 安全的字符串。
    pub async fn collect_from<R: tokio::io::AsyncRead + Unpin>(&mut self, mut reader: R) -> String {
        let mut temp_buf = [0u8; 8192];

        loop {
            let remaining = self.max_bytes.saturating_sub(self.buffer.len());
            if remaining == 0 {
                // 已达到限制，尝试读一次看是否还有更多数据
                match reader.read(&mut temp_buf[..1]).await {
                    Ok(0) | Err(_) => {
                        // EOF 或读取错误，不标记为截断
                    }
                    Ok(_) => {
                        // 还有更多数据，标记为截断
                        self.truncated = true;
                        debug!(max_bytes = self.max_bytes, "输出超过字节限制，已截断");
                    }
                }
                break;
            }

            // 读取数据，不超过剩余可接受量
            let to_read = remaining.min(temp_buf.len());
            match reader.read(&mut temp_buf[..to_read]).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    self.buffer.extend_from_slice(&temp_buf[..n]);
                }
                Err(e) => {
                    debug!(error = %e, "读取输出时发生错误");
                    break;
                }
            }
        }

        self.to_string_internal()
    }

    /// 将缓冲区转换为 UTF-8 安全的字符串
    ///
    /// 如果被截断，在末尾追加截断提示。
    /// 确保不在多字节 UTF-8 字符中间切断。
    #[must_use]
    pub fn into_string(self) -> String {
        self.to_string_internal()
    }

    /// 是否已被截断
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// 当前缓冲区的字节数
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// 缓冲区是否为空
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// 内部实现：转换为字符串
    fn to_string_internal(&self) -> String {
        let bytes = &self.buffer;

        // UTF-8 安全截断：找到最后一个有效的 UTF-8 边界
        let safe_str = utf8_safe_truncate(bytes);

        if self.truncated {
            format!(
                "{safe_str}\n... [输出被截断，超过 {} 字节限制]",
                self.max_bytes
            )
        } else {
            safe_str.to_string()
        }
    }
}

/// UTF-8 安全截断：确保不在多字节字符中间切断
///
/// 如果 `bytes` 末尾包含不完整的 UTF-8 序列，向前退到最后一个完整字符的边界。
fn utf8_safe_truncate(bytes: &[u8]) -> &str {
    match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            // `valid_up_to()` 返回最后一个有效 UTF-8 位置
            let valid_len = e.valid_up_to();
            // SAFETY: from_utf8 已验证 bytes[..valid_len] 是有效 UTF-8
            std::str::from_utf8(&bytes[..valid_len]).unwrap_or("")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_output_collector_new() {
        let collector = OutputCollector::new(1024);
        assert_eq!(collector.max_bytes, 1024);
        assert!(collector.buffer.is_empty());
        assert!(!collector.truncated);
        assert!(collector.is_empty());
        assert_eq!(collector.len(), 0);
    }

    #[tokio::test]
    async fn test_collect_small_output() {
        let data: &[u8] = b"hello world";
        let mut collector = OutputCollector::new(1024);
        let output = collector.collect_from(data).await;
        assert_eq!(output, "hello world");
        assert!(!collector.is_truncated());
    }

    #[tokio::test]
    async fn test_collect_truncated_output() {
        let data: &[u8] = b"hello world, this is a long string";
        let mut collector = OutputCollector::new(5);
        let output = collector.collect_from(data).await;
        assert!(collector.is_truncated());
        assert!(output.starts_with("hello"));
        assert!(output.contains("输出被截断"));
        assert!(output.contains("5 字节限制"));
    }

    #[tokio::test]
    async fn test_collect_exact_limit() {
        let data: &[u8] = b"hello";
        let mut collector = OutputCollector::new(5);
        let output = collector.collect_from(data).await;
        // 恰好在限制处结束，EOF 时不算截断
        assert!(!collector.is_truncated());
        assert_eq!(output, "hello");
    }

    #[tokio::test]
    async fn test_collect_empty() {
        let data: &[u8] = b"";
        let mut collector = OutputCollector::new(1024);
        let output = collector.collect_from(data).await;
        assert_eq!(output, "");
        assert!(!collector.is_truncated());
    }

    #[tokio::test]
    async fn test_collect_utf8_safe_truncation() {
        // "你好" 的 UTF-8 编码：\xe4\xbd\xa0\xe5\xa5\xbd（每个汉字 3 字节）
        let data = "你好世界".as_bytes();
        // 限制为 4 字节：第一个汉字占 3 字节，第二个汉字的第一个字节会被读入，
        // 但应该安全截断到 "你" 的边界
        let mut collector = OutputCollector::new(4);
        let output = collector.collect_from(data).await;
        assert!(collector.is_truncated());
        // 应该包含 "你" 并且不包含损坏的字符
        assert!(output.starts_with('你'));
        assert!(output.contains("输出被截断"));
    }

    #[tokio::test]
    async fn test_collect_utf8_full_chinese() {
        let data = "你好".as_bytes(); // 6 字节
        let mut collector = OutputCollector::new(100);
        let output = collector.collect_from(data).await;
        assert_eq!(output, "你好");
        assert!(!collector.is_truncated());
    }

    #[test]
    fn test_utf8_safe_truncate_valid() {
        let s = "hello";
        assert_eq!(utf8_safe_truncate(s.as_bytes()), "hello");
    }

    #[test]
    fn test_utf8_safe_truncate_broken() {
        // "你" = \xe4\xbd\xa0, 只取前 2 个字节是不完整的
        let bytes = &[0xe4, 0xbd];
        assert_eq!(utf8_safe_truncate(bytes), "");
    }

    #[test]
    fn test_utf8_safe_truncate_partial_multi_byte() {
        // "你好" = \xe4\xbd\xa0\xe5\xa5\xbd
        // 取前 4 字节：\xe4\xbd\xa0\xe5 —— 第二个字符不完整
        let full = "你好".as_bytes();
        let partial = &full[..4];
        assert_eq!(utf8_safe_truncate(partial), "你");
    }

    #[test]
    fn test_into_string_not_truncated() {
        let collector = OutputCollector {
            max_bytes: 100,
            buffer: b"hello".to_vec(),
            truncated: false,
        };
        assert_eq!(collector.into_string(), "hello");
    }

    #[test]
    fn test_into_string_truncated() {
        let collector = OutputCollector {
            max_bytes: 5,
            buffer: b"hello".to_vec(),
            truncated: true,
        };
        let s = collector.into_string();
        assert!(s.starts_with("hello"));
        assert!(s.contains("输出被截断"));
    }

    #[test]
    fn test_len_and_is_empty() {
        let mut collector = OutputCollector::new(100);
        assert!(collector.is_empty());
        assert_eq!(collector.len(), 0);

        collector.buffer.extend_from_slice(b"data");
        assert!(!collector.is_empty());
        assert_eq!(collector.len(), 4);
    }
}
