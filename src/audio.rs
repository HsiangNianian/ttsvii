use anyhow::{Context, Result};
use chrono::Duration;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct AudioSplitter;

impl AudioSplitter {
    /// 切分音频文件，返回临时目录和切分后的音频文件路径列表
    pub async fn split_audio(
        audio_path: &Path,
        start_time: Duration,
        end_time: Duration,
        output_dir: &Path,
        index: u32,
    ) -> Result<PathBuf> {
        let start_secs =
            start_time.num_seconds() as f64 + start_time.num_milliseconds() as f64 / 1000.0;
        let duration = (end_time - start_time).num_seconds() as f64
            + (end_time - start_time).num_milliseconds() as f64 / 1000.0;

        let output_path = output_dir.join(format!("segment_{:04}.wav", index));

        // 使用 ffmpeg 切分音频
        // 使用 -threads 1 限制每个进程的线程数，减少内存占用
        // 使用 -loglevel error 减少日志输出
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-loglevel")
            .arg("error")
            .arg("-threads")
            .arg("1")
            .arg("-i")
            .arg(audio_path)
            .arg("-ss")
            .arg(start_secs.to_string())
            .arg("-t")
            .arg(duration.to_string())
            .arg("-acodec")
            .arg("copy")
            .arg("-y")
            .arg(&output_path)
            .output()
            .await
            .context("执行 ffmpeg 失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ffmpeg 切分失败: {}", stderr);
        }

        Ok(output_path)
    }

    /// 合并多个音频文件
    pub async fn merge_audio(audio_files: &[PathBuf], output_path: &Path) -> Result<()> {
        // 创建文件列表
        let temp_dir = TempDir::new()?;
        let file_list_path = temp_dir.path().join("file_list.txt");

        let mut file_list_content = String::new();
        for file in audio_files {
            file_list_content.push_str(&format!("file '{}'\n", file.display()));
        }

        tokio::fs::write(&file_list_path, file_list_content).await?;

        // 使用 ffmpeg concat 合并
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-f")
            .arg("concat")
            .arg("-safe")
            .arg("0")
            .arg("-i")
            .arg(&file_list_path)
            .arg("-c")
            .arg("copy")
            .arg("-y")
            .arg(output_path)
            .output()
            .await
            .context("执行 ffmpeg 合并失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ffmpeg 合并失败: {}", stderr);
        }

        Ok(())
    }

    /// 检查 ffmpeg 是否可用
    pub async fn check_ffmpeg() -> Result<()> {
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .await
            .context("ffmpeg 未安装或不在 PATH 中")?;

        if !output.status.success() {
            anyhow::bail!("ffmpeg 不可用");
        }

        Ok(())
    }
}
