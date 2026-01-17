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
    let (hours, minutes, seconds, milliseconds) = ts.get();
    let total_ms = hours as i64 * 3600 * 1000
        + minutes as i64 * 60 * 1000
        + seconds as i64 * 1000
        + milliseconds as i64;

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
