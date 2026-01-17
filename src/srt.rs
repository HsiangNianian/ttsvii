use anyhow::{Context, Result};
use chrono::Duration;
use regex::Regex;
use std::fs;
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
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("无法读取 SRT 文件: {:?}", path.as_ref()))?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Vec<SrtEntry>> {
        let mut entries = Vec::new();

        // 匹配 SRT 条目的正则表达式
        let entry_re = Regex::new(
            r"(?m)^(\d+)\s*\n(\d{2}):(\d{2}):(\d{2}),(\d{3})\s*-->\s*(\d{2}):(\d{2}):(\d{2}),(\d{3})\s*\n((?:.*\n?)+?)(?=\n\d+\s*\n|\Z)"
        ).unwrap();

        for cap in entry_re.captures_iter(content) {
            let index: u32 = cap[1].parse().with_context(|| "无法解析序号")?;

            let start_h = cap[2].parse::<i64>()?;
            let start_m = cap[3].parse::<i64>()?;
            let start_s = cap[4].parse::<i64>()?;
            let start_ms = cap[5].parse::<i64>()?;
            let start_time = Duration::hours(start_h)
                + Duration::minutes(start_m)
                + Duration::seconds(start_s)
                + Duration::milliseconds(start_ms);

            let end_h = cap[6].parse::<i64>()?;
            let end_m = cap[7].parse::<i64>()?;
            let end_s = cap[8].parse::<i64>()?;
            let end_ms = cap[9].parse::<i64>()?;
            let end_time = Duration::hours(end_h)
                + Duration::minutes(end_m)
                + Duration::seconds(end_s)
                + Duration::milliseconds(end_ms);

            let text = cap[10].trim().to_string();

            entries.push(SrtEntry {
                index,
                start_time,
                end_time,
                text,
            });
        }

        if entries.is_empty() {
            anyhow::bail!("未找到有效的 SRT 条目");
        }

        Ok(entries)
    }
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
        assert_eq!(entries[0].text, "Hello world");
        assert_eq!(entries[1].text, "This is a test");
    }
}
