use anyhow::Result;
use chrono::Duration;
use srtlib::{Subtitles, Timestamp};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct SrtEntry {
    pub index: u32,
    pub start_time: Duration,
    pub end_time: Duration,
    pub text: String,
}

pub struct SrtParser;

impl SrtParser {
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<SrtEntry>> {
        let subtitles = Subtitles::parse_from_file(path.as_ref(), None)
            .map_err(|e| anyhow::anyhow!("解析 SRT 文件失败: {}", e))?;

        Self::convert_to_entries(subtitles)
    }

    /// 从字符串内容解析 SRT 格式的字幕
    #[allow(dead_code)] // 公开 API，可能被外部使用
    pub fn parse(content: &str) -> Result<Vec<SrtEntry>> {
        let subtitles = Subtitles::parse_from_str(content.to_string())
            .map_err(|e| anyhow::anyhow!("解析 SRT 文件失败: {}", e))?;

        Self::convert_to_entries(subtitles)
    }

    fn convert_to_entries(subtitles: Subtitles) -> Result<Vec<SrtEntry>> {
        let mut entries = Vec::new();

        for subtitle in subtitles.to_vec() {
            let index = subtitle.num as u32;

            // 转换时间戳为 chrono::Duration
            let start_time = timestamp_to_duration(&subtitle.start_time)?;
            let end_time = timestamp_to_duration(&subtitle.end_time)?;

            entries.push(SrtEntry {
                index,
                start_time,
                end_time,
                text: subtitle.text,
            });
        }

        if entries.is_empty() {
            anyhow::bail!("未找到有效的 SRT 条目");
        }

        Ok(entries)
    }
}

/// 将 srtlib 的 Timestamp 转换为 chrono::Duration
fn timestamp_to_duration(ts: &Timestamp) -> Result<Duration> {
    // srtlib 的 get() 方法返回 (hours, minutes, seconds, milliseconds)
    // 根据 SRT 格式：00:00:06,633 中 633 是毫秒（0-999）
    let (hours, minutes, seconds, milliseconds) = ts.get();

    // 检查 milliseconds 是否在合理范围内（0-999）
    // 如果 >= 1000，说明 srtlib 可能返回的是其他单位，需要调整
    let ms = if milliseconds >= 1000 {
        // 如果 milliseconds >= 1000，可能是 srtlib 返回的是微秒或其他单位
        // 或者解析错误，先尝试除以1000
        eprintln!("警告: milliseconds 值异常 ({})，尝试调整", milliseconds);
        milliseconds / 1000
    } else {
        milliseconds
    };

    let total_ms =
        hours as i64 * 3600 * 1000 + minutes as i64 * 60 * 1000 + seconds as i64 * 1000 + ms as i64;

    Ok(Duration::milliseconds(total_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_srt() {
        let content = r"1
00:00:00,000 --> 00:00:02,500
Hello world

2
00:00:02,500 --> 00:00:05,000
This is a test
";
        let entries = SrtParser::parse(content).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text.trim(), "Hello world");
        assert_eq!(entries[1].text.trim(), "This is a test");
    }
}
