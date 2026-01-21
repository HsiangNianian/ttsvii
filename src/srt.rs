use anyhow::Result;
use chrono::Duration;
use serde::{Deserialize, Serialize};
use srtlib::{Subtitles, Timestamp};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SrtEntry {
    pub index: u32,
    #[serde(with = "duration_serde")]
    pub start_time: Duration,
    #[serde(with = "duration_serde")]
    pub end_time: Duration,
    pub text: String,
}

mod duration_serde {
    use chrono::Duration;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(duration.num_milliseconds())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = i64::deserialize(deserializer)?;
        Ok(Duration::milliseconds(millis))
    }
}

pub struct SrtParser;

impl SrtParser {
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Vec<SrtEntry>> {
        let subtitles = Subtitles::parse_from_file(path.as_ref(), None)
            .map_err(|e| anyhow::anyhow!("解析 SRT 文件失败: {}", e))?;

        let entries = Self::convert_to_entries(subtitles)?;
        Self::normalize_entries(entries)
    }

    /// 从字符串内容解析 SRT 格式的字幕
    #[allow(dead_code)] // 公开 API，可能被外部使用
    pub fn parse(content: &str) -> Result<Vec<SrtEntry>> {
        let subtitles = Subtitles::parse_from_str(content.to_string())
            .map_err(|e| anyhow::anyhow!("解析 SRT 文件失败: {}", e))?;

        let entries = Self::convert_to_entries(subtitles)?;
        Self::normalize_entries(entries)
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

    /// 规范化字幕：移除空文本、排序并重排序号，合并时长过短的字幕。
    fn normalize_entries(entries: Vec<SrtEntry>) -> Result<Vec<SrtEntry>> {
        // 去除空文本的条目
        let mut filtered: Vec<SrtEntry> = entries
            .into_iter()
            .filter(|e| !e.text.trim().is_empty())
            .collect();

        if filtered.is_empty() {
            anyhow::bail!("未找到有效的 SRT 条目");
        }

        // 按开始时间排序，若开始时间相同则按结束时间排序，确保序号可重排
        filtered.sort_by(|a, b| match a.start_time.cmp(&b.start_time) {
            std::cmp::Ordering::Equal => a.end_time.cmp(&b.end_time),
            other => other,
        });

        let threshold = Duration::milliseconds(500);
        let mut normalized = Vec::with_capacity(filtered.len());
        let mut i = 0;

        while i < filtered.len() {
            let mut current = filtered[i].clone();
            let mut j = i;

            // 如果时长小于阈值，则向后合并，直到达到阈值或没有后续字幕
            while current.end_time - current.start_time < threshold && j + 1 < filtered.len() {
                let next = filtered[j + 1].clone();

                let merged_text = format!("{}, {}", current.text.trim(), next.text.trim());
                let merged_start = current.start_time;
                let merged_end = std::cmp::max(current.end_time, next.end_time);

                current = SrtEntry {
                    index: current.index,
                    start_time: merged_start,
                    end_time: merged_end,
                    text: merged_text,
                };

                j += 1;
            }

            if current.end_time <= current.start_time {
                anyhow::bail!("字幕时间区间无效 (开始时间应早于结束时间)");
            }

            normalized.push(current);
            i = j + 1;
        }

        // 重排序号，保证连续合法
        for (idx, entry) in normalized.iter_mut().enumerate() {
            entry.index = (idx as u32) + 1;
        }

        Ok(normalized)
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

    #[test]
    fn test_merge_and_reindex_short_segments() {
        let content = r"1
00:00:00,000 --> 00:00:00,400
Hi

2
00:00:00,400 --> 00:00:01,000
there

3
00:00:01,000 --> 00:00:02,000
Keep this line
";

        let entries = SrtParser::parse(content).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].index, 1);
        assert_eq!(entries[1].index, 2);

        assert_eq!(entries[0].start_time, Duration::milliseconds(0));
        assert_eq!(entries[0].end_time, Duration::milliseconds(1000));
        assert_eq!(entries[0].text.trim(), "Hi, there");

        assert_eq!(entries[1].start_time, Duration::milliseconds(1000));
        assert_eq!(entries[1].end_time, Duration::milliseconds(2000));
        assert_eq!(entries[1].text.trim(), "Keep this line");
    }
}
