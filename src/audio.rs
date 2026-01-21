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
    /// original_audio_path: 原始音频/视频文件路径（用于获取总时长和计算空白时间段）
    /// progress_callback: 可选的进度回调函数 (当前进度, 总数, 消息)
    pub async fn merge_audio_with_timing<F>(
        audio_files: &[PathBuf],
        srt_entries: &[SrtEntry],
        output_path: &Path,
        original_audio_path: &Path,
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

        // 获取原始音频/视频的总时长
        let probe_output = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(original_audio_path)
            .output()
            .await?;

        let original_duration_sec = if probe_output.status.success() {
            let duration_str = String::from_utf8_lossy(&probe_output.stdout);
            duration_str.trim().parse::<f64>().unwrap_or(0.0)
        } else {
            anyhow::bail!("无法获取原始音频/视频时长: {:?}", original_audio_path);
        };

        if original_duration_sec <= 0.0 {
            anyhow::bail!("原始音频/视频时长为 0: {:?}", original_audio_path);
        }

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
            // 只在每 10 个文件或最后一个文件时更新消息和回调，减少刷屏
            let should_update = (i + 1) % 10 == 0 || (i + 1) == audio_files.len();
            if should_update {
                let msg = format!("处理文件 {}/{}", i + 1, audio_files.len());
                progress.set_message(msg.clone());
                if let Some(ref mut cb) = progress_callback {
                    cb(i + 1, audio_files.len(), msg.clone());
                }
            }
            // 进度条会自动更新进度（通过 progress.inc(1)），不需要每次都更新消息
            // 计算目标时长（从 SRT 时间戳）
            let target_duration_ms = (entry.end_time - entry.start_time).num_milliseconds();
            let target_duration_sec = target_duration_ms as f64 / 1000.0;

            println!(
                "[变速] 任务 {}: SRT目标时长 = {:.3}s ({:.0}ms)",
                i + 1,
                target_duration_sec,
                target_duration_ms
            );

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

            println!(
                "[变速] 任务 {}: 实际音频时长 = {:.3}s",
                i + 1,
                actual_duration_sec
            );

            // 计算速度调整比例
            let speed_ratio = actual_duration_sec / target_duration_sec;
            println!(
                "[变速] 任务 {}: 速度比例 = {:.3}x (需要{}倍速)",
                i + 1,
                speed_ratio,
                if speed_ratio > 1.0 {
                    "加速"
                } else {
                    "减速"
                }
            );

            // 处理后的音频文件路径
            let processed_file = temp_dir.path().join(format!("processed_{:04}.wav", i + 1));

            // 如果速度比例接近 1.0（差异小于 1%），直接复制，否则调整速度
            if (speed_ratio - 1.0).abs() < 0.01 {
                // 速度几乎不需要调整，直接复制
                println!(
                    "[变速] 任务 {}: 速度比例接近1.0，直接复制（无需变速）",
                    i + 1
                );
                tokio::fs::copy(audio_file, &processed_file).await?;
                processed_files.push(processed_file);
            } else if speed_ratio < 1.0 {
                // 生成的音频比原来短，不变速，而是追加静音
                let silence_duration_sec = target_duration_sec - actual_duration_sec;
                println!(
                    "[变速] 任务 {}: 音频较短（{:.3}s < {:.3}s），追加 {:.3}s 静音而非变速",
                    i + 1,
                    actual_duration_sec,
                    target_duration_sec,
                    silence_duration_sec
                );

                // 使用 ffmpeg 追加静音
                let silence_filter = format!("apad=pad_dur={:.3}", silence_duration_sec);
                
                let output = tokio::process::Command::new("ffmpeg")
                    .arg("-loglevel")
                    .arg("error")
                    .arg("-i")
                    .arg(audio_file)
                    .arg("-af")
                    .arg(&silence_filter)
                    .arg("-acodec")
                    .arg("pcm_s16le")
                    .arg("-ar")
                    .arg("44100")
                    .arg("-ac")
                    .arg("2")
                    .arg("-y")
                    .arg(&processed_file)
                    .output()
                    .await
                    .context("执行 ffmpeg 追加静音失败")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!(
                        "ffmpeg 追加静音失败 (文件: {:?}): {}",
                        audio_file,
                        stderr
                    );
                }

                println!(
                    "[变速] 任务 {}: ✅ 静音追加完成",
                    i + 1
                );
                processed_files.push(processed_file);
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

                println!(
                    "[变速] 任务 {}: atempo链 = {:?} (共{}个)",
                    i + 1,
                    tempo_chain,
                    tempo_chain.len()
                );

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

                // 验证变速后的实际时长，如果与目标时长不一致，需要进一步调整
                let processed_probe_output = tokio::process::Command::new("ffprobe")
                    .arg("-v")
                    .arg("error")
                    .arg("-show_entries")
                    .arg("format=duration")
                    .arg("-of")
                    .arg("default=noprint_wrappers=1:nokey=1")
                    .arg(&processed_file)
                    .output()
                    .await?;

                if processed_probe_output.status.success() {
                    let processed_duration_str =
                        String::from_utf8_lossy(&processed_probe_output.stdout);
                    if let Ok(processed_duration_sec) = processed_duration_str.trim().parse::<f64>()
                    {
                        let duration_diff = (processed_duration_sec - target_duration_sec).abs();
                        println!("[变速] 任务 {}: 变速后时长 = {:.3}s, 目标 = {:.3}s, 差值 = {:.3}s ({:.0}ms)", 
                            i + 1, processed_duration_sec, target_duration_sec, duration_diff, duration_diff * 1000.0);

                        // 如果差值超过 50 毫秒，需要进一步调整
                        if duration_diff > 0.05 {
                            // 计算需要再次调整的比例
                            let correction_ratio = target_duration_sec / processed_duration_sec;
                            println!(
                                "[变速] 任务 {}: 需要二次修正，修正比例 = {:.3}x",
                                i + 1,
                                correction_ratio
                            );

                            // 如果修正比例在合理范围内（0.5-2.0），进行二次调整
                            if correction_ratio >= 0.5 && correction_ratio <= 2.0 {
                                let corrected_file =
                                    temp_dir.path().join(format!("corrected_{:04}.wav", i + 1));

                                let correction_filter = format!("atempo={:.3}", correction_ratio);
                                let correction_output = tokio::process::Command::new("ffmpeg")
                                    .arg("-loglevel")
                                    .arg("error")
                                    .arg("-i")
                                    .arg(&processed_file)
                                    .arg("-af")
                                    .arg(correction_filter)
                                    .arg("-acodec")
                                    .arg("pcm_s16le")
                                    .arg("-ar")
                                    .arg("44100")
                                    .arg("-ac")
                                    .arg("2")
                                    .arg("-y")
                                    .arg(&corrected_file)
                                    .output()
                                    .await
                                    .context("执行 ffmpeg 时长修正失败")?;

                                if correction_output.status.success() {
                                    // 验证修正后的时长
                                    let corrected_probe = tokio::process::Command::new("ffprobe")
                                        .arg("-v")
                                        .arg("error")
                                        .arg("-show_entries")
                                        .arg("format=duration")
                                        .arg("-of")
                                        .arg("default=noprint_wrappers=1:nokey=1")
                                        .arg(&corrected_file)
                                        .output()
                                        .await?;

                                    if corrected_probe.status.success() {
                                        let corrected_duration_str =
                                            String::from_utf8_lossy(&corrected_probe.stdout);
                                        if let Ok(corrected_duration_sec) =
                                            corrected_duration_str.trim().parse::<f64>()
                                        {
                                            let final_diff = (corrected_duration_sec
                                                - target_duration_sec)
                                                .abs();
                                            println!("[变速] 任务 {}: 二次修正后时长 = {:.3}s, 目标 = {:.3}s, 最终差值 = {:.3}s ({:.0}ms)", 
                                                i + 1, corrected_duration_sec, target_duration_sec, final_diff, final_diff * 1000.0);
                                        }
                                    }

                                    // 使用修正后的文件
                                    processed_files.push(corrected_file);
                                } else {
                                    println!(
                                        "[变速] 任务 {}: 二次修正失败，使用原始变速结果",
                                        i + 1
                                    );
                                    // 修正失败，使用原始变速后的文件
                                    processed_files.push(processed_file);
                                }
                            } else {
                                println!(
                                    "[变速] 任务 {}: 修正比例超出范围 ({:.3}), 使用原始变速结果",
                                    i + 1,
                                    correction_ratio
                                );
                                // 修正比例超出范围，使用原始变速后的文件
                                processed_files.push(processed_file);
                            }
                        } else {
                            println!("[变速] 任务 {}: ✅ 时长准确，无需修正", i + 1);
                            // 时长已经足够准确，直接使用
                            processed_files.push(processed_file);
                        }
                    } else {
                        processed_files.push(processed_file);
                    }
                } else {
                    processed_files.push(processed_file);
                }
            }

            progress.inc(1);
        }

        let finish_msg = "速度调整完成";
        if let Some(ref mut cb) = progress_callback {
            cb(audio_files.len(), audio_files.len(), finish_msg.to_string());
        }
        progress.finish_with_message(finish_msg);

        // 计算空白时间段并生成空白音频片段
        println!("\n[空白计算] 开始计算空白时间段...");
        println!(
            "[空白计算] 原始音频总时长: {:.3}s ({:.0}ms)",
            original_duration_sec,
            original_duration_sec * 1000.0
        );

        let mut silence_segments = Vec::new();
        let mut total_silence_sec = 0.0;

        // 1. 起始空白：0 到第一句话的开始时间
        if let Some(first_entry) = srt_entries.first() {
            let start_silence_ms = first_entry.start_time.num_milliseconds();
            if start_silence_ms > 0 {
                let start_silence_sec = start_silence_ms as f64 / 1000.0;
                silence_segments.push((0, start_silence_sec)); // 0 表示在开头
                total_silence_sec += start_silence_sec;
                println!(
                    "[空白计算] 起始空白: {:.3}s ({:.0}ms) [0 -> 条目1开始]",
                    start_silence_sec, start_silence_ms
                );
            } else {
                println!("[空白计算] 起始空白: 0s (第一句话从0秒开始)");
            }
        }

        // 2. 中间空白：每句话之间的空白
        let mut total_gap_sec = 0.0;
        let mut gap_count = 0;
        for i in 0..(srt_entries.len() - 1) {
            let current_end = srt_entries[i].end_time.num_milliseconds();
            let next_start = srt_entries[i + 1].start_time.num_milliseconds();
            let gap_ms = next_start - current_end;

            if gap_ms > 0 {
                let gap_sec = gap_ms as f64 / 1000.0;
                silence_segments.push((i + 1, gap_sec)); // i+1 表示在第 i+1 个音频之后
                total_gap_sec += gap_sec;
                gap_count += 1;
                println!(
                    "[空白计算] 中间空白 {}: {:.3}s ({:.0}ms) [条目{}结束 -> 条目{}开始]",
                    gap_count,
                    gap_sec,
                    gap_ms,
                    i + 1,
                    i + 2
                );
            }
        }
        total_silence_sec += total_gap_sec;
        println!(
            "[空白计算] 中间空白总计: {:.3}s ({:.0}ms), 共{}段",
            total_gap_sec,
            total_gap_sec * 1000.0,
            gap_count
        );

        // 3. 末尾空白：最后一句话的结束时间到原始音频总时长
        if let Some(last_entry) = srt_entries.last() {
            let last_end_ms = last_entry.end_time.num_milliseconds();
            let original_duration_ms = (original_duration_sec * 1000.0) as i64;
            let end_silence_ms = original_duration_ms - last_end_ms;

            if end_silence_ms > 0 {
                let end_silence_sec = end_silence_ms as f64 / 1000.0;
                silence_segments.push((srt_entries.len(), end_silence_sec)); // 在最后
                total_silence_sec += end_silence_sec;
                println!(
                    "[空白计算] 末尾空白: {:.3}s ({:.0}ms) [最后条目结束 -> 原始音频结束]",
                    end_silence_sec, end_silence_ms
                );
            } else {
                println!("[空白计算] 末尾空白: 0s (最后条目结束时间 = 原始音频结束时间)");
            }
        }

        println!(
            "[空白计算] 空白总时长: {:.3}s ({:.0}ms), 共{}段",
            total_silence_sec,
            total_silence_sec * 1000.0,
            silence_segments.len()
        );

        // 计算所有音频片段的总时长（从SRT）
        let total_audio_duration_ms: i64 = srt_entries
            .iter()
            .map(|e| (e.end_time - e.start_time).num_milliseconds())
            .sum();
        let total_audio_duration_sec = total_audio_duration_ms as f64 / 1000.0;
        println!(
            "[空白计算] SRT音频总时长: {:.3}s ({:.0}ms)",
            total_audio_duration_sec, total_audio_duration_ms
        );

        let calculated_total = total_silence_sec + total_audio_duration_sec;
        println!(
            "[空白计算] 计算总时长 = 空白({:.3}s) + 音频({:.3}s) = {:.3}s",
            total_silence_sec, total_audio_duration_sec, calculated_total
        );
        println!(
            "[空白计算] 原始音频时长: {:.3}s, 差值: {:.3}s ({:.0}ms)",
            original_duration_sec,
            (calculated_total - original_duration_sec).abs(),
            (calculated_total - original_duration_sec).abs() * 1000.0
        );

        // 生成空白音频文件
        println!("\n[空白生成] 开始生成空白音频片段...");
        let mut silence_files = Vec::new();
        for (idx, (position, duration_sec)) in silence_segments.iter().enumerate() {
            let silence_file = temp_dir.path().join(format!("silence_{}.wav", idx));
            let position_desc = if *position == 0 {
                "起始".to_string()
            } else if *position == srt_entries.len() {
                "末尾".to_string()
            } else {
                format!("条目{}之后", position)
            };

            println!(
                "[空白生成] 生成空白片段 {}: {:.3}s ({:.0}ms), 位置: {}",
                idx,
                duration_sec,
                duration_sec * 1000.0,
                position_desc
            );
            Self::generate_silence(&silence_file, *duration_sec).await?;
            silence_files.push((*position, silence_file));
        }
        println!("[空白生成] 完成，共生成{}个空白片段", silence_files.len());

        // 合并所有处理后的音频文件和空白片段
        let file_list_path = temp_dir.path().join("file_list.txt");
        let mut file_list_content = String::new();

        // 构建合并列表：空白片段 + 音频片段交替插入
        let mut all_files: Vec<PathBuf> = Vec::new();

        // 添加起始空白
        if let Some((_, path)) = silence_files.iter().find(|(p, _)| *p == 0) {
            all_files.push(path.clone());
        }

        // 添加音频片段和中间空白
        for (i, processed_file) in processed_files.iter().enumerate() {
            all_files.push(processed_file.clone());

            // 在当前位置之后添加空白
            if let Some((_, path)) = silence_files.iter().find(|(p, _)| *p == i + 1) {
                all_files.push(path.clone());
            }
        }

        // 添加末尾空白
        if let Some((_, path)) = silence_files.iter().find(|(p, _)| *p == srt_entries.len()) {
            all_files.push(path.clone());
        }

        // 写入文件列表
        for file in all_files {
            let abs_path = if file.exists() {
                std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone())
            } else {
                file.clone()
            };
            let escaped_path = abs_path.display().to_string().replace('\'', "'\"'\"'");
            file_list_content.push_str(&format!("file '{}'\n", escaped_path));
        }

        tokio::fs::write(&file_list_path, file_list_content).await?;

        // 使用 ffmpeg concat 合并
        // 注意：不使用 -c copy，而是重新编码以确保时长精确
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-loglevel")
            .arg("error")
            .arg("-f")
            .arg("concat")
            .arg("-safe")
            .arg("0")
            .arg("-i")
            .arg(&file_list_path)
            .arg("-acodec")
            .arg("pcm_s16le")
            .arg("-ar")
            .arg("44100")
            .arg("-ac")
            .arg("2")
            .arg("-y")
            .arg(output_path)
            .output()
            .await
            .context("执行 ffmpeg 合并失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ffmpeg 合并失败: {}", stderr);
        }

        // 验证最终音频时长是否与原始音频一致，如果不一致则修正
        let final_probe_output = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(output_path)
            .output()
            .await?;

        let final_duration_sec = if final_probe_output.status.success() {
            let duration_str = String::from_utf8_lossy(&final_probe_output.stdout);
            duration_str.trim().parse::<f64>().unwrap_or(0.0)
        } else {
            anyhow::bail!("无法获取最终音频时长: {:?}", output_path);
        };

        // 计算时长差值
        let duration_diff = final_duration_sec - original_duration_sec;
        let tolerance_sec = 0.05; // 50毫秒

        if duration_diff.abs() > tolerance_sec {
            eprintln!(
                "检测到时长不一致: 最终音频 ({:.3}s) vs 原始音频 ({:.3}s), 差值: {:.3}s",
                final_duration_sec, original_duration_sec, duration_diff
            );

            // 如果差值较大，尝试通过调整最后一个空白片段来修正
            if duration_diff.abs() > 0.1 {
                eprintln!("尝试修正时长...");

                // 找到最后一个空白片段的位置
                if let Some((_, last_silence_path)) =
                    silence_files.iter().find(|(p, _)| *p == srt_entries.len())
                {
                    // 计算需要调整的时长（负值表示需要缩短，正值表示需要延长）
                    let adjustment_sec = -duration_diff;

                    // 获取当前最后一个空白片段的时长
                    let last_silence_probe = tokio::process::Command::new("ffprobe")
                        .arg("-v")
                        .arg("error")
                        .arg("-show_entries")
                        .arg("format=duration")
                        .arg("-of")
                        .arg("default=noprint_wrappers=1:nokey=1")
                        .arg(last_silence_path)
                        .output()
                        .await?;

                    if last_silence_probe.status.success() {
                        let current_duration_str =
                            String::from_utf8_lossy(&last_silence_probe.stdout);
                        if let Ok(current_duration_sec) = current_duration_str.trim().parse::<f64>()
                        {
                            let new_duration_sec = current_duration_sec + adjustment_sec;

                            // 确保新时长不为负
                            if new_duration_sec > 0.0 {
                                eprintln!(
                                    "调整最后一个空白片段: {:.3}s -> {:.3}s",
                                    current_duration_sec, new_duration_sec
                                );

                                // 重新生成最后一个空白片段
                                let adjusted_silence_file =
                                    temp_dir.path().join("silence_end_adjusted.wav");
                                Self::generate_silence(&adjusted_silence_file, new_duration_sec)
                                    .await?;

                                // 重新构建文件列表，使用调整后的空白片段
                                let mut adjusted_file_list_content = String::new();

                                // 添加起始空白
                                if let Some((_, path)) = silence_files.iter().find(|(p, _)| *p == 0)
                                {
                                    let abs_path = if path.exists() {
                                        std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())
                                    } else {
                                        path.clone()
                                    };
                                    let escaped_path =
                                        abs_path.display().to_string().replace('\'', "'\"'\"'");
                                    adjusted_file_list_content
                                        .push_str(&format!("file '{}'\n", escaped_path));
                                }

                                // 添加音频片段和中间空白
                                for (i, processed_file) in processed_files.iter().enumerate() {
                                    let abs_path = if processed_file.exists() {
                                        std::fs::canonicalize(processed_file)
                                            .unwrap_or_else(|_| processed_file.clone())
                                    } else {
                                        processed_file.clone()
                                    };
                                    let escaped_path =
                                        abs_path.display().to_string().replace('\'', "'\"'\"'");
                                    adjusted_file_list_content
                                        .push_str(&format!("file '{}'\n", escaped_path));

                                    // 在当前位置之后添加空白
                                    if let Some((_, path)) =
                                        silence_files.iter().find(|(p, _)| *p == i + 1)
                                    {
                                        let abs_path = if path.exists() {
                                            std::fs::canonicalize(path)
                                                .unwrap_or_else(|_| path.clone())
                                        } else {
                                            path.clone()
                                        };
                                        let escaped_path =
                                            abs_path.display().to_string().replace('\'', "'\"'\"'");
                                        adjusted_file_list_content
                                            .push_str(&format!("file '{}'\n", escaped_path));
                                    }
                                }

                                // 添加调整后的末尾空白
                                let abs_path = if adjusted_silence_file.exists() {
                                    std::fs::canonicalize(&adjusted_silence_file)
                                        .unwrap_or_else(|_| adjusted_silence_file.clone())
                                } else {
                                    adjusted_silence_file.clone()
                                };
                                let escaped_path =
                                    abs_path.display().to_string().replace('\'', "'\"'\"'");
                                adjusted_file_list_content
                                    .push_str(&format!("file '{}'\n", escaped_path));

                                // 重新合并
                                let adjusted_file_list_path =
                                    temp_dir.path().join("file_list_adjusted.txt");
                                tokio::fs::write(
                                    &adjusted_file_list_path,
                                    adjusted_file_list_content,
                                )
                                .await?;

                                let adjusted_output = tokio::process::Command::new("ffmpeg")
                                    .arg("-loglevel")
                                    .arg("error")
                                    .arg("-f")
                                    .arg("concat")
                                    .arg("-safe")
                                    .arg("0")
                                    .arg("-i")
                                    .arg(&adjusted_file_list_path)
                                    .arg("-acodec")
                                    .arg("pcm_s16le")
                                    .arg("-ar")
                                    .arg("44100")
                                    .arg("-ac")
                                    .arg("2")
                                    .arg("-y")
                                    .arg(output_path)
                                    .output()
                                    .await
                                    .context("执行 ffmpeg 修正合并失败")?;

                                if adjusted_output.status.success() {
                                    // 验证修正后的时长
                                    let corrected_probe_output =
                                        tokio::process::Command::new("ffprobe")
                                            .arg("-v")
                                            .arg("error")
                                            .arg("-show_entries")
                                            .arg("format=duration")
                                            .arg("-of")
                                            .arg("default=noprint_wrappers=1:nokey=1")
                                            .arg(output_path)
                                            .output()
                                            .await?;

                                    if corrected_probe_output.status.success() {
                                        let corrected_duration_str =
                                            String::from_utf8_lossy(&corrected_probe_output.stdout);
                                        if let Ok(corrected_duration_sec) =
                                            corrected_duration_str.trim().parse::<f64>()
                                        {
                                            let final_diff = (corrected_duration_sec
                                                - original_duration_sec)
                                                .abs();
                                            if final_diff <= tolerance_sec {
                                                eprintln!(
                                                    "✅ 时长修正成功: {:.3}s (差值: {:.3}s)",
                                                    corrected_duration_sec, final_diff
                                                );
                                            } else {
                                                eprintln!("⚠️  时长修正后仍有误差: {:.3}s vs {:.3}s (差值: {:.3}s)", corrected_duration_sec, original_duration_sec, final_diff);
                                            }
                                        }
                                    }
                                } else {
                                    let stderr = String::from_utf8_lossy(&adjusted_output.stderr);
                                    eprintln!("警告: 修正合并失败: {}", stderr);
                                }
                            } else {
                                eprintln!("警告: 无法修正（空白片段时长将变为负数）");
                            }
                        }
                    }
                } else {
                    eprintln!("警告: 未找到末尾空白片段，无法修正时长");
                }
            }
        } else {
            eprintln!(
                "✅ 时长验证通过: {:.3}s (差值: {:.3}s)",
                final_duration_sec, duration_diff
            );
        }

        Ok(())
    }

    /// 生成指定时长的空白音频（静音）
    async fn generate_silence(output_path: &Path, duration_sec: f64) -> Result<()> {
        if duration_sec <= 0.0 {
            anyhow::bail!("空白时长必须大于 0");
        }

        // 使用 ffmpeg 生成空白音频
        // -f lavfi: 使用 libavfilter 输入
        // anullsrc: 生成空音频源
        // -t: 指定时长
        let output = tokio::process::Command::new("ffmpeg")
            .arg("-loglevel")
            .arg("error")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("anullsrc=channel_layout=stereo:sample_rate=44100")
            .arg("-t")
            .arg(duration_sec.to_string())
            .arg("-acodec")
            .arg("pcm_s16le")
            .arg("-ar")
            .arg("44100")
            .arg("-ac")
            .arg("2")
            .arg("-y")
            .arg(output_path)
            .output()
            .await
            .context("执行 ffmpeg 生成空白音频失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ffmpeg 生成空白音频失败: {}", stderr);
        }

        Ok(())
    }

    /// 检查音频文件是否有效（存在、有内容、有时长）
    pub async fn validate_audio_file(audio_path: &Path) -> Result<()> {
        // 检查文件是否存在
        if !audio_path.exists() {
            anyhow::bail!("音频文件不存在: {:?}", audio_path);
        }

        // 检查文件大小
        let metadata = tokio::fs::metadata(audio_path).await?;
        if metadata.len() == 0 {
            anyhow::bail!("音频文件为空: {:?}", audio_path);
        }
        if metadata.len() <= 44 {
            anyhow::bail!(
                "音频文件无效（文件大小: {} 字节，可能只有文件头）: {:?}",
                metadata.len(),
                audio_path
            );
        }

        // 使用 ffprobe 检查音频时长
        let probe_output = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(audio_path)
            .output()
            .await
            .context("执行 ffprobe 失败")?;

        if !probe_output.status.success() {
            let stderr = String::from_utf8_lossy(&probe_output.stderr);
            anyhow::bail!("ffprobe 检查音频失败: {}", stderr);
        }

        let duration_str = String::from_utf8_lossy(&probe_output.stdout);
        let duration = duration_str
            .trim()
            .parse::<f64>()
            .context("无法解析音频时长")?;

        if duration <= 0.0 {
            anyhow::bail!("音频时长为 0 或无效: {:?}", audio_path);
        }

        if duration < 0.01 {
            anyhow::bail!("音频时长太短（小于 0.01 秒），可能无效: {:?}", audio_path);
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
