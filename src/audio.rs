use crate::srt::SrtEntry;
use anyhow::{Context, Result};
use chrono::Duration;
use indicatif::{ProgressBar, ProgressStyle};
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
        // chrono::Duration::num_milliseconds() 返回总毫秒数，直接除以 1000 转换为秒
        let start_secs = start_time.num_milliseconds() as f64 / 1000.0;
        let duration = (end_time - start_time).num_milliseconds() as f64 / 1000.0;

        // 验证时间范围
        if start_secs < 0.0 {
            anyhow::bail!("起始时间不能为负数: {:.3} 秒", start_secs);
        }
        if duration <= 0.0 {
            anyhow::bail!("音频时长无效: {:.3} 秒", duration);
        }
        if duration < 0.01 {
            anyhow::bail!("音频时长太短（小于 0.01 秒），可能无效: {:.3} 秒", duration);
        }

        let output_path = output_dir.join(format!("segment_{:04}.wav", index));

        // 先获取输入音频文件的实际时长
        let probe_output = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(audio_path)
            .output()
            .await;

        let actual_duration = if let Ok(probe_output) = probe_output {
            if probe_output.status.success() {
                let duration_str = String::from_utf8_lossy(&probe_output.stdout);
                duration_str.trim().parse::<f64>().ok()
            } else {
                None
            }
        } else {
            None
        };

        // 如果切分时间超出实际时长，调整结束时间
        let adjusted_duration = if let Some(actual) = actual_duration {
            if start_secs >= actual {
                anyhow::bail!("起始时间 {} 超出音频时长 {}", start_secs, actual);
            }
            if start_secs + duration > actual {
                actual - start_secs
            } else {
                duration
            }
        } else {
            duration
        };

        // 使用 ffmpeg 切分音频
        // 对于从视频提取的 PCM WAV，使用重新编码而不是 copy
        // 使用 -threads 1 限制每个进程的线程数，减少内存占用
        // 使用 -loglevel error 减少日志输出
        // 添加 -avoid_negative_ts make_zero 避免时间戳问题
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
            .arg(adjusted_duration.to_string())
            .arg("-acodec")
            .arg("pcm_s16le") // 使用 PCM 编码，兼容从视频提取的音频
            .arg("-ar")
            .arg("44100") // 采样率
            .arg("-ac")
            .arg("2") // 立体声
            .arg("-avoid_negative_ts")
            .arg("make_zero")
            .arg("-y")
            .arg(&output_path)
            .output()
            .await
            .context("执行 ffmpeg 失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ffmpeg 切分失败: {}", stderr);
        }

        // 验证切分后的音频文件是否有效
        // WAV 文件头部至少 44 字节，如果文件太小说明没有实际音频数据
        let metadata = tokio::fs::metadata(&output_path).await?;
        if metadata.len() <= 44 {
            anyhow::bail!(
                "切分的音频文件无效（文件大小: {} 字节，可能为空或只有文件头）",
                metadata.len()
            );
        }

        // 使用 ffprobe 检查音频时长，确保有实际音频内容
        let probe_output = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(&output_path)
            .output()
            .await;

        if let Ok(probe_output) = probe_output {
            if probe_output.status.success() {
                let duration_str = String::from_utf8_lossy(&probe_output.stdout);
                if let Ok(duration) = duration_str.trim().parse::<f64>() {
                    // 如果音频时长小于 0.01 秒，认为是无效的
                    if duration < 0.01 {
                        anyhow::bail!(
                            "切分的音频文件无效（音频时长: {:.3} 秒，可能为空）",
                            duration
                        );
                    }
                }
            }
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
            // 使用绝对路径，确保 ffmpeg 能找到文件
            // 如果文件不存在或无法规范化，使用原始路径
            let abs_path = if file.exists() {
                std::fs::canonicalize(file).unwrap_or_else(|_| file.clone())
            } else {
                // 如果文件不存在，尝试使用绝对路径
                file.canonicalize().unwrap_or_else(|_| {
                    // 如果还是失败，使用原始路径（可能是相对路径）
                    file.clone()
                })
            };
            // 转义单引号，防止路径中包含单引号导致问题
            let escaped_path = abs_path.display().to_string().replace('\'', "'\"'\"'");
            file_list_content.push_str(&format!("file '{}'\n", escaped_path));
        }

        tokio::fs::write(&file_list_path, file_list_content).await?;

        // 使用 ffmpeg concat 合并
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-loglevel")
            .arg("error") // 只显示错误，减少输出
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

    /// 合并音频并根据 SRT 时间戳调整速度
    /// audio_files: 音频文件路径列表（已按索引排序）
    /// srt_entries: SRT 条目列表（已按索引排序）
    /// output_path: 输出文件路径
    /// progress_callback: 可选的进度回调函数 (当前进度, 总数, 消息)
    pub async fn merge_audio_with_timing<F>(
        audio_files: &[PathBuf],
        srt_entries: &[SrtEntry],
        output_path: &Path,
        mut progress_callback: Option<F>,
    ) -> Result<()>
    where
        F: FnMut(usize, usize, String),
    {
        if audio_files.len() != srt_entries.len() {
            anyhow::bail!(
                "音频文件数量 ({}) 与 SRT 条目数量 ({}) 不匹配",
                audio_files.len(),
                srt_entries.len()
            );
        }

        let temp_dir = TempDir::new()?;
        let mut processed_files = Vec::new();

        // 创建进度条
        let progress = ProgressBar::new(audio_files.len() as u64);
        progress.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} 调整速度 ({eta})")
                .unwrap()
                .progress_chars("#>-"),
        );

        // 处理每个音频文件，根据 SRT 时间戳调整速度
        for (i, (audio_file, entry)) in audio_files.iter().zip(srt_entries.iter()).enumerate() {
            let msg = format!("处理文件 {}/{}", i + 1, audio_files.len());
            progress.set_message(msg.clone());
            if let Some(ref mut cb) = progress_callback {
                cb(i + 1, audio_files.len(), msg.clone());
            }
            // 计算目标时长（从 SRT 时间戳）
            let target_duration_ms = (entry.end_time - entry.start_time).num_milliseconds();
            let target_duration_sec = target_duration_ms as f64 / 1000.0;

            // 获取音频文件的实际时长
            let probe_output = tokio::process::Command::new("ffprobe")
                .arg("-v")
                .arg("error")
                .arg("-show_entries")
                .arg("format=duration")
                .arg("-of")
                .arg("default=noprint_wrappers=1:nokey=1")
                .arg(audio_file)
                .output()
                .await?;

            let actual_duration_sec = if probe_output.status.success() {
                let duration_str = String::from_utf8_lossy(&probe_output.stdout);
                duration_str.trim().parse::<f64>().unwrap_or(0.0)
            } else {
                anyhow::bail!("无法获取音频文件时长: {:?}", audio_file);
            };

            if actual_duration_sec <= 0.0 {
                anyhow::bail!("音频文件时长为 0: {:?}", audio_file);
            }

            // 计算速度调整比例
            let speed_ratio = actual_duration_sec / target_duration_sec;

            // 处理后的音频文件路径
            let processed_file = temp_dir.path().join(format!("processed_{:04}.wav", i + 1));

            // 如果速度比例接近 1.0（差异小于 1%），直接复制，否则调整速度
            if (speed_ratio - 1.0).abs() < 0.01 {
                // 速度几乎不需要调整，直接复制
                tokio::fs::copy(audio_file, &processed_file).await?;
            } else {
                // 使用 atempo 滤镜调整速度
                // atempo 的范围是 0.5 到 2.0，如果超出范围需要链式使用多个 atempo
                let mut ffmpeg_args = vec![
                    "-loglevel".to_string(),
                    "error".to_string(),
                    "-i".to_string(),
                    audio_file.to_string_lossy().to_string(),
                ];

                // 构建 atempo 滤镜链
                let mut tempo_chain = Vec::new();
                let mut remaining_ratio = speed_ratio;

                // atempo 范围是 0.5-2.0，如果超出需要链式使用
                while remaining_ratio > 2.0 {
                    tempo_chain.push(2.0);
                    remaining_ratio /= 2.0;
                }
                while remaining_ratio < 0.5 {
                    tempo_chain.push(0.5);
                    remaining_ratio /= 0.5;
                }
                tempo_chain.push(remaining_ratio);

                // 构建滤镜字符串
                let filter_complex =
                    if tempo_chain.len() == 1 && (tempo_chain[0] - 1.0).abs() < 0.01 {
                        // 不需要调整
                        "anull".to_string()
                    } else {
                        // 链式 atempo 滤镜
                        tempo_chain
                            .iter()
                            .map(|&t| format!("atempo={:.3}", t))
                            .collect::<Vec<_>>()
                            .join(",")
                    };

                ffmpeg_args.extend(vec![
                    "-af".to_string(),
                    filter_complex,
                    "-acodec".to_string(),
                    "pcm_s16le".to_string(),
                    "-ar".to_string(),
                    "44100".to_string(),
                    "-ac".to_string(),
                    "2".to_string(),
                    "-y".to_string(),
                    processed_file.to_string_lossy().to_string(),
                ]);

                let output = tokio::process::Command::new("ffmpeg")
                    .args(&ffmpeg_args)
                    .output()
                    .await
                    .context("执行 ffmpeg 速度调整失败")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!(
                        "ffmpeg 速度调整失败 (文件: {:?}, 速度比例: {:.3}): {}",
                        audio_file,
                        speed_ratio,
                        stderr
                    );
                }
            }

            processed_files.push(processed_file);
            progress.inc(1);
        }

        let finish_msg = "速度调整完成";
        if let Some(ref mut cb) = progress_callback {
            cb(audio_files.len(), audio_files.len(), finish_msg.to_string());
        }
        progress.finish_with_message(finish_msg);

        // 合并所有处理后的音频文件
        let file_list_path = temp_dir.path().join("file_list.txt");
        let mut file_list_content = String::new();
        for file in &processed_files {
            let abs_path = if file.exists() {
                std::fs::canonicalize(file).unwrap_or_else(|_| file.clone())
            } else {
                file.clone()
            };
            let escaped_path = abs_path.display().to_string().replace('\'', "'\"'\"'");
            file_list_content.push_str(&format!("file '{}'\n", escaped_path));
        }

        tokio::fs::write(&file_list_path, file_list_content).await?;

        // 使用 ffmpeg concat 合并
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-loglevel")
            .arg("error")
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
