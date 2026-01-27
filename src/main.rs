mod api;
mod audio;
mod srt;
mod task;
mod web;
mod log_capture;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashSet;
use std::path::PathBuf;
use task::{TaskExecutor, TaskManager};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "ttsvii")]
#[command(about = "语音克隆批量处理工具", long_about = None)]
struct Args {
    /// 启动 Web UI 模式
    #[arg(long)]
    webui: bool,

    /// API 基础 URL
    #[arg(long, default_value = "http://183.147.142.111:63364")]
    api_url: String,

    /// 音频文件路径
    #[arg(short, long)]
    audio: Option<PathBuf>,

    /// SRT 字幕文件路径
    #[arg(short, long)]
    srt: Option<PathBuf>,

    /// 输出目录
    #[arg(short, long, default_value = "./output")]
    output: PathBuf,

    /// 最大并发数（API 请求）
    #[arg(long, default_value_t = 10)]
    max_concurrent: usize,

    /// 音频切分并发数（建议不超过 CPU 核心数）
    #[arg(long, default_value_t = 4)]
    split_concurrent: usize,

    /// 批次大小（每处理多少轮并发后休息）
    #[arg(long, default_value_t = 5)]
    batch_size: usize,

    /// 批次间休息时间（秒）
    #[arg(long, default_value_t = 1)]
    rest_duration: u64,

    /// 失败重试次数
    #[arg(long, default_value_t = 3)]
    retry_count: u32,

    /// 恢复任务 UUID
    #[arg(long)]
    resume: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 如果指定了 resume，则执行恢复任务
    if let Some(uuid) = args.resume {
        // 恢复任务模式下，SRT 文件是必须的
        let srt = args
            .srt
            .clone()
            .context("恢复任务必须提供 SRT 文件路径 (-s/--srt)")?;
        // 原始音频文件是可选的（用于变速）
        let audio = args.audio.clone();

        return resume_cli_task(
            uuid,
            args.api_url,
            args.output,
            args.max_concurrent,
            args.batch_size,
            args.rest_duration,
            args.retry_count,
            audio,
            srt,
        )
        .await;
    }

    // 如果没有提供 audio 和 srt 参数，或者明确指定了 --webui，则启动 Web UI
    let should_start_webui = args.webui || (args.audio.is_none() && args.srt.is_none());

    if should_start_webui {
        return start_webui().await;
    }

    // 命令行模式
    let audio = args.audio.context("必须提供音频文件路径 (-a/--audio)")?;
    let srt = args.srt.context("必须提供 SRT 文件路径 (-s/--srt)")?;

    run_cli_mode(
        args.api_url,
        args.output,
        args.max_concurrent,
        args.split_concurrent,
        args.batch_size,
        args.rest_duration,
        args.retry_count,
        audio,
        srt,
    )
    .await
}

async fn start_webui() -> Result<()> {
    use axum::serve;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio::time::{sleep, Duration};

    let state = web::AppState::new();
    let app = web::create_router(state).await;
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    let url = format!("http://{}", addr);

    println!("正在启动 Web UI 服务器...");
    println!("Web UI 地址: {}", url);

    let listener = TcpListener::bind(&addr).await?;

    // 在后台启动服务器
    let server_handle = tokio::spawn(async move {
        if let Err(e) = serve(listener, app.into_make_service()).await {
            eprintln!("Web 服务器错误: {}", e);
        }
    });

    // 等待服务器启动
    sleep(Duration::from_millis(500)).await;

    // 自动打开浏览器
    println!("正在打开浏览器...");
    if let Err(e) = open::that(&url) {
        eprintln!("无法自动打开浏览器: {}. 请手动访问: {}", e, url);
    }

    println!("按 Ctrl+C 停止服务器");

    // 等待服务器运行
    server_handle.await?;

    Ok(())
}

pub async fn run_cli_mode(
    api_url: String,
    output: PathBuf,
    max_concurrent: usize,
    split_concurrent: usize,
    batch_size: usize,
    rest_duration: u64,
    retry_count: u32,
    audio: PathBuf,
    srt: PathBuf,
) -> Result<()> {
    // 检查 ffmpeg
    audio::AudioSplitter::check_ffmpeg()
        .await
        .context("请确保已安装 ffmpeg 并在 PATH 中")?;

    // 生成本次运行的 UUID
    let task_uuid = Uuid::new_v4();
    let uuid_str = task_uuid.to_string();
    println!("任务 ID: {}", uuid_str);

    // 创建输出目录
    tokio::fs::create_dir_all(&output).await?;

    // 创建临时目录（在当前工作目录的 tmp/{uuid} 目录）
    let tmp_dir = std::env::current_dir()?.join("tmp").join(&uuid_str);
    tokio::fs::create_dir_all(&tmp_dir).await?;

    // 创建输出目录（output/{uuid} 目录）
    let output_task_dir = output.join(&uuid_str);
    tokio::fs::create_dir_all(&output_task_dir).await?;

    // 使用 defer 模式确保临时目录被清理
    let result = async {
        println!("正在解析 SRT 文件...");
        let srt_entries = srt::SrtParser::parse_file(&srt).context("解析 SRT 文件失败")?;
        println!("找到 {} 个字幕条目", srt_entries.len());

        if !srt_entries.is_empty() {
            println!("\n前 3 个字幕条目的时间信息:");
            for entry in srt_entries.iter().take(3) {
                let start_ms = entry.start_time.num_milliseconds();
                let end_ms = entry.end_time.num_milliseconds();
                println!(
                    "  条目 {}: 开始={}ms ({:.3}s), 结束={}ms ({:.3}s), 时长={}ms ({:.3}s), 文本=\"{}\"",
                    entry.index,
                    start_ms,
                    start_ms as f64 / 1000.0,
                    end_ms,
                    end_ms as f64 / 1000.0,
                    end_ms - start_ms,
                    (end_ms - start_ms) as f64 / 1000.0,
                    entry.text.trim().chars().take(30).collect::<String>()
                );
            }
        }

        // 创建任务管理器
        let task_manager = TaskManager::new(srt_entries, &audio, &tmp_dir, &output_task_dir)?;

        println!("正在切分音频文件（并发数: {}）...", split_concurrent);
        task_manager
            .prepare_audio_segments(&audio, split_concurrent)
            .await?;

        // 创建 API 客户端
        let api_client = std::sync::Arc::new(api::ApiClient::new(api_url.clone()));

        // 创建任务执行器
        let executor = std::sync::Arc::new(TaskExecutor::new(api_client, max_concurrent, retry_count));
        executor.set_total(task_manager.len() as u64);
        if retry_count > 0 {
            println!("失败重试次数: {}", retry_count);
        }

        println!(
            "开始处理 {} 个任务（并发数: {}, 批次大小: {}, 休息时间: {}s）...",
            task_manager.len(),
            max_concurrent,
            batch_size,
            rest_duration
        );

        // 批次处理任务（带失败重试机制）
        let tasks = task_manager.get_tasks();
        // 动态批次大小，根据实时时延微调
        let mut dynamic_batch_size = batch_size.max(1);
        let mut batch_task_count = dynamic_batch_size * max_concurrent;
        // 记录需要重试的任务（失败的任务索引）
        let mut failed_task_indices: HashSet<usize> = HashSet::new();
        let mut retry_round = 0;
        const MAX_RETRY_ROUNDS: u32 = 3;

        loop {
            retry_round += 1;

            // 检查是否超过最大重试轮数
            if retry_round > MAX_RETRY_ROUNDS && !failed_task_indices.is_empty() {
                println!("\n⚠️  重试已达到最大轮数 ({}), 仍有 {} 个任务失败", MAX_RETRY_ROUNDS, failed_task_indices.len());
                println!("正在自动切换到恢复模式以继续处理...");
                println!("\n任务 ID: {} (用于后续手动恢复)", uuid_str);
                break;
            }
            if retry_round > 1 {
                println!("\n=== 第 {} 轮重试失败任务 ===", retry_round - 1);
                println!("等待 60 秒后开始重试...");
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }

            // 确定本次要处理的任务
            let tasks_to_process: Vec<(usize, &task::Task)> = if retry_round == 1 {
                // 第一轮：处理所有任务
                tasks.iter().enumerate().collect()
            } else {
                // 后续轮：只处理失败的任务
                failed_task_indices
                    .iter()
                    .filter_map(|&idx| tasks.get(idx).map(|task| (idx, task)))
                    .collect()
            };

            if tasks_to_process.is_empty() {
                println!("\n所有任务已完成！");
                break;
            }

            // 仅在第一轮时初始化进度条，后续轮数只更新进度数
            if retry_round == 1 {
                executor.set_total(tasks_to_process.len() as u64);
                println!(
                    "\n开始处理 {} 个任务（本轮: 第 {} 轮）...",
                    tasks_to_process.len(),
                    retry_round
                );
            } else {
                executor.set_total(tasks_to_process.len() as u64);
                println!(
                    "本轮处理 {} 个失败任务...",
                    tasks_to_process.len()
                );
            }

            // 按批次处理
            let mut first_batch = true;
            for batch_start in (0..tasks_to_process.len()).step_by(batch_task_count) {
                let batch_num = (batch_start / batch_task_count) + 1;
                let batch_end = (batch_start + batch_task_count).min(tasks_to_process.len());
                let batch_tasks = &tasks_to_process[batch_start..batch_end];
                let total_batches = (tasks_to_process.len() + batch_task_count - 1) / batch_task_count;

                // 执行当前批次
                let futures: Vec<_> = batch_tasks
                    .iter()
                    .map(|(_, task)| {
                        let task = (*task).clone();
                        let executor = executor.clone();
                        async move { executor.execute_task(task).await }
                    })
                    .collect();

                let _ = futures::future::join_all(futures).await;

                // 批次完成后统一更新一次统计信息
                let (p50, p95, _, _) = executor.metrics_snapshot();
                let (success, failure) = executor.get_stats();
                let completed = success + failure;
                
                // 生成进度条
                let bar_width: usize = 40;
                let filled = ((completed as f64 / tasks_to_process.len() as f64) * bar_width as f64) as usize;
                let bar = format!(
                    "[{}{}] {}/{}",
                    "=".repeat(filled),
                    " ".repeat(bar_width.saturating_sub(filled)),
                    completed,
                    tasks_to_process.len()
                );

                if first_batch {
                    // 第一次初始化3行
                    println!("批次 {}/{}: 处理任务 {}-{} | p50={:.0}ms p95={:.0}ms", 
                        batch_num, total_batches, batch_start + 1, batch_end, p50, p95);
                    println!("{}", bar);
                    println!("进度 {}/{} | 成功:{} 失败:{}", completed, tasks_to_process.len(), success, failure);
                    first_batch = false;
                } else {
                    // 后续批次，向上3行覆盖
                    print!("\x1b[3A\x1b[J"); // 向上3行，清除从这里到屏幕末尾
                    println!("批次 {}/{}: 处理任务 {}-{} | p50={:.0}ms p95={:.0}ms", 
                        batch_num, total_batches, batch_start + 1, batch_end, p50, p95);
                    println!("{}", bar);
                    println!("进度 {}/{} | 成功:{} 失败:{}", completed, tasks_to_process.len(), success, failure);
                }

                // p95 较高则缩小批次，p95 较低则放大到上限（2 倍初始）
                if p95 > 4000.0 && dynamic_batch_size > 1 {
                    dynamic_batch_size = (dynamic_batch_size / 2).max(1);
                } else if p95 < 1500.0 {
                    dynamic_batch_size = (dynamic_batch_size + 1).min(batch_size * 2).max(1);
                }
                batch_task_count = dynamic_batch_size * max_concurrent;

                // 如果不是最后一批，休息一下
                if batch_end < tasks_to_process.len() {
                    tokio::time::sleep(std::time::Duration::from_secs(rest_duration)).await;
                }
            }

            // 检查本轮结果，更新失败任务列表
            let mut new_failed_indices = HashSet::new();
            let mut skipped: Vec<(usize, String)> = Vec::new();
            for (original_idx, _) in tasks_to_process.iter() {
                let task = &tasks[*original_idx];
                // 检查任务是否成功（文件是否存在且有效）
                if task.output_path.exists() {
                } else {
                    // 文件不存在，标记为失败
                    new_failed_indices.insert(*original_idx);
                }
            }

            // 更新失败任务集合（只保留仍然失败的任务）
            failed_task_indices = new_failed_indices;

            if failed_task_indices.is_empty() {
                println!("\n本轮所有任务都已完成！");
                break;
            } else {
                println!(
                    "\n本轮完成，仍有 {} 个任务失败，将在下一轮重试",
                    failed_task_indices.len()
                );
            }
        }

        executor.finish();

        // 检查是否由于重试超限进入恢复模式
        let should_enter_resume_mode = retry_round > MAX_RETRY_ROUNDS && !failed_task_indices.is_empty();
        if should_enter_resume_mode {
            println!("\n[恢复模式] 进入恢复模式处理失败任务...");
            // 直接调用恢复模式处理
            return resume_and_merge(
                uuid_str,
                &tmp_dir,
                &output_task_dir,
                api_url,
                output,
                max_concurrent,
                batch_size,
                rest_duration,
                retry_count,
                audio,
                srt,
            )
            .await;
        }

        println!("\n所有任务执行完成，开始音频可用性校验...");

        // 音频可用性校验
        let mut invalid_audio_indices: HashSet<usize> = HashSet::new();
        let mut validation_round = 0;

        loop {
            validation_round += 1;
            if validation_round > 1 {
                println!("第 {} 轮重试...", validation_round - 1);
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }

            // 确定本次要校验的任务
            let tasks_to_validate: Vec<(usize, &task::Task)> = if validation_round == 1 {
                // 第一轮：校验所有任务
                tasks.iter().enumerate().collect()
            } else {
                // 后续轮：只校验失败的任务
                invalid_audio_indices
                    .iter()
                    .filter_map(|&idx| tasks.get(idx).map(|task| (idx, task)))
                    .collect()
            };

            if tasks_to_validate.is_empty() {
                println!("所有音频校验通过！");
                break;
            }

            println!(
                "校验 {} 个音频文件（第 {} 轮）...",
                tasks_to_validate.len(),
                validation_round
            );
            // 校验音频文件
            let mut new_invalid_indices = std::collections::HashSet::new();
            for (original_idx, task) in tasks_to_validate.iter() {
                match audio::AudioSplitter::validate_audio_file(&task.output_path).await {
                    Ok(_) => {
                        // 校验通过
                    }
                    Err(e) => {
                        eprintln!(
                            "✗ 任务 {} 校验失败: {}",
                            task.entry.index, e
                        );
                        new_invalid_indices.insert(*original_idx);
                        // 删除无效文件，准备重新生成
                        let _ = tokio::fs::remove_file(&task.output_path).await;
                    }
                }
            }

            // 如果有无效的音频，需要重新执行这些任务
            if !new_invalid_indices.is_empty() {
                println!(
                    "发现 {} 个无效文件，重新生成...",
                    new_invalid_indices.len()
                );
                // 将这些任务加入失败任务列表，重新执行
                failed_task_indices.extend(new_invalid_indices.iter().cloned());
                // 重新执行失败的任务
                let tasks_to_retry: Vec<(usize, &task::Task)> = new_invalid_indices
                    .iter()
                    .filter_map(|&idx| tasks.get(idx).map(|task| (idx, task)))
                    .collect();

                executor.set_total(tasks_to_retry.len() as u64);

                // 按批次重新执行
                for batch_start in (0..tasks_to_retry.len()).step_by(batch_task_count) {
                    let batch_end = (batch_start + batch_task_count).min(tasks_to_retry.len());
                    let batch_tasks = &tasks_to_retry[batch_start..batch_end];

                    let futures: Vec<_> = batch_tasks
                        .iter()
                        .map(|(_, task)| {
                            let task = (*task).clone();
                            let executor = executor.clone();
                            async move {
                                executor.execute_task(task).await
                            }
                        })
                    .collect();

                    let _ = futures::future::join_all(futures).await;
                    
                    let (success, failure) = executor.get_stats();
                    println!(
                        "✓ 重生成进度 {}/{} | 成功: {} | 失败: {}",
                        success + failure,
                        tasks_to_retry.len(),
                        success,
                        failure
                    );

                    // 检查结果
                    for (original_idx, task) in batch_tasks.iter() {
                        if !task.output_path.exists() {
                            // 仍然失败，保留在失败列表中
                        } else {
                            // 成功，从失败列表中移除
                            failed_task_indices.remove(original_idx);
                        }
                    }

                    if batch_end < tasks_to_retry.len() {
                        tokio::time::sleep(std::time::Duration::from_secs(rest_duration)).await;
                    }
                }

                // 更新无效音频索引（只保留仍然无效的）
                invalid_audio_indices = new_invalid_indices
                    .into_iter()
                    .filter(|&idx| {
                        let task = &tasks[idx];
                        !task.output_path.exists()
                    })
                    .collect();
            } else {
                // 所有音频都有效
                invalid_audio_indices.clear();
            }

            if invalid_audio_indices.is_empty() && failed_task_indices.is_empty() {
                println!("所有音频校验通过！");
                break;
            }
        }

        println!("收集音频文件用于合并...");

        // 收集所有合成的音频文件及其对应的 SRT 条目（只包含实际存在的文件）
        let mut audio_files = Vec::new();
        let mut srt_entries_for_merge = Vec::new();
        // 按索引排序 tasks
        let mut sorted_tasks: Vec<_> = tasks.iter().collect();
        sorted_tasks.sort_by_key(|task| task.entry.index);
        for task in sorted_tasks {
            if task.output_path.exists() {
                let metadata = tokio::fs::metadata(&task.output_path).await?;
                if metadata.len() > 0 {
                    audio_files.push(task.output_path.clone());
                    srt_entries_for_merge.push(task.entry.clone());
                }
            }
        }

        if audio_files.is_empty() {
            anyhow::bail!("没有有效的合成音频文件可以合并");
        }

        println!("找到 {} 个有效的音频文件，开始合并...", audio_files.len());

        // 合并音频（保存到 output/{uuid}.wav）
        let final_output = output.join(format!("{}.wav", uuid_str));
        // 给合并操作添加超时保护（最多 30 分钟）
        tokio::time::timeout(
            std::time::Duration::from_secs(1800), // 30 分钟
            audio::AudioSplitter::merge_audio(&audio_files, &final_output),
        )
        .await
        .context("合并音频超时（超过 30 分钟）")??;

        println!("✓ 原始合并完成: {}", final_output.display());

        // 生成根据 SRT 时间戳变速后的合并音频
        let timed_output = output.join(format!("{}_timed.wav", uuid_str));
        print!("变速处理: ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        tokio::time::timeout(
            std::time::Duration::from_secs(1800), // 30 分钟
            audio::AudioSplitter::merge_audio_with_timing(
                &audio_files,
                &srt_entries_for_merge,
                &timed_output,
                &audio, // 传入原始音频/视频路径
                Some(|current: usize, total: usize, msg: String| {
                    print!("\r变速处理: [{}{}] {}/{} - {}", 
                        "=".repeat((current * 40 / total).max(1)),
                        " ".repeat(40 - (current * 40 / total).max(1)),
                        current, total, msg);
                    std::io::Write::flush(&mut std::io::stdout()).ok();
                }),
            ),
        )
        .await
        .context("合并变速音频超时（超过 30 分钟）")??;

        println!("\r✓ 变速合并完成: {}", timed_output.display());

        Ok::<(), anyhow::Error>(())
    }
    .await;
    result
}

async fn resume_cli_task(
    uuid: String,
    api_url: String,
    output: PathBuf,
    max_concurrent: usize,
    batch_size: usize,
    rest_duration: u64,
    retry_count: u32,
    audio: Option<PathBuf>,
    srt: PathBuf,
) -> Result<()> {
    println!("正在尝试恢复任务: {}", uuid);
    let uuid_str = uuid.clone();

    // 1. 定位目录
    let tmp_dir = std::env::current_dir()?.join("tmp").join(&uuid);
    let output_task_dir = output.join(&uuid);

    if !tmp_dir.exists() {
        anyhow::bail!("临时目录不存在: {:?}", tmp_dir);
    }

    if !output_task_dir.exists() {
        println!("输出目录不存在，将重新创建: {:?}", output_task_dir);
        tokio::fs::create_dir_all(&output_task_dir).await?;
    }

    // 2. 解析 SRT 文件重建任务列表
    println!("正在解析 SRT 文件...");
    let srt_entries = srt::SrtParser::parse_file(&srt).context("解析 SRT 文件失败")?;

    // 按索引排序
    // srt_entries 应该是已经按解析顺序排好的，但为了保险起见
    // 注意：SrtParser 内部已经做了排序和规范化

    println!("成功解析 SRT 文件: {} 个条目", srt_entries.len());

    // 3. 构建 TaskManager
    // TaskManager 需要 audio_path 来构建 Task，但实际上 resume 模式下主要用 tmp_dir 和输出路径
    // 我们可以传入一个空的 PathBuf 或者实际的 audio path（如果存在）
    let dummy_audio_path = audio.clone().unwrap_or_else(|| PathBuf::from(""));
    let task_manager = TaskManager::new(
        srt_entries.clone(),
        &dummy_audio_path,
        &tmp_dir,
        &output_task_dir,
    )?;

    // 4. 执行剩余任务
    let api_client = std::sync::Arc::new(api::ApiClient::new(api_url.clone()));
    let executor = std::sync::Arc::new(TaskExecutor::new(api_client, max_concurrent, retry_count));

    let tasks = task_manager.get_tasks();

    // 找出未完成的任务
    let mut initial_pending_indices = Vec::new();
    for (idx, task) in tasks.iter().enumerate() {
        // 检查 tmp 音频是否存在
        if !task.tmp_audio.exists() {
            // 如果 tmp 音频不存在，说明切分未完成或文件丢失，无法进行推理
            // 在 resume 模式下，我们跳过这些无法处理的任务，或者报错？
            // 根据用户需求，只需要处理 tmp 中有的
            // eprintln!("警告: 任务 {} 缺少临时切分音频，跳过", task.entry.index);
            continue;
        }

        // 检查 output 音频是否存在且有效
        let completed = match tokio::fs::metadata(&task.output_path).await {
            Ok(m) => m.len() > 0,
            Err(_) => false,
        };

        if !completed {
            initial_pending_indices.push(idx);
        }
    }

    println!(
        "总任务数: {} | 已完成: {} | 待处理: {}",
        tasks.len(),
        tasks.len() - initial_pending_indices.len(),
        initial_pending_indices.len()
    );

    if initial_pending_indices.is_empty() {
        println!("所有任务均已完成，直接进行合并步骤。");
    } else {
        executor.set_total(initial_pending_indices.len() as u64);

        let batch_task_count = batch_size * max_concurrent;
        let mut failed_task_indices: HashSet<usize> = HashSet::new();
        let mut retry_round = 0;

        loop {
            retry_round += 1;

            if retry_round > 1 {
                if failed_task_indices.is_empty() {
                    break;
                }
                println!("\n=== 第 {} 轮重试失败任务 ===", retry_round - 1);
                println!("等待 60 秒后开始重试...");
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }

            let tasks_to_process: Vec<(usize, &task::Task)> = if retry_round == 1 {
                initial_pending_indices
                    .iter()
                    .map(|&idx| (idx, &tasks[idx]))
                    .collect()
            } else {
                failed_task_indices
                    .iter()
                    .filter_map(|&idx| tasks.get(idx).map(|task| (idx, task)))
                    .collect()
            };

            if tasks_to_process.is_empty() && retry_round > 1 {
                break;
            }

            println!(
                "\n开始处理 {} 个任务（本轮: 第 {} 轮）...",
                tasks_to_process.len(),
                retry_round
            );

            executor.set_total(tasks_to_process.len() as u64);

            let mut batch_num = 0;
            let mut first_batch = true;
            for batch_start in (0..tasks_to_process.len()).step_by(batch_task_count) {
                batch_num += 1;
                let batch_end = (batch_start + batch_task_count).min(tasks_to_process.len());
                let batch_tasks = &tasks_to_process[batch_start..batch_end];
                let total_batches =
                    (tasks_to_process.len() + batch_task_count - 1) / batch_task_count;

                let futures: Vec<_> = batch_tasks
                    .iter()
                    .map(|(_, task)| {
                        let task = (*task).clone();
                        let executor = executor.clone();
                        async move { executor.execute_task(task).await }
                    })
                    .collect();

                let _ = futures::future::join_all(futures).await;

                let (success, failure) = executor.get_stats();

                // 生成进度条
                let bar_width: usize = 40;
                let filled = (((success + failure) as f64 / tasks_to_process.len() as f64)
                    * bar_width as f64) as usize;
                let bar = format!(
                    "[{}{}] {}/{}",
                    "=".repeat(filled),
                    " ".repeat(bar_width.saturating_sub(filled)),
                    success + failure,
                    tasks_to_process.len()
                );

                if first_batch {
                    // 第一次初始化3行
                    println!(
                        "批次 {}/{}: 处理任务 {}-{}",
                        batch_num,
                        total_batches,
                        batch_start + 1,
                        batch_end
                    );
                    println!("{}", bar);
                    println!(
                        "进度 {}/{} | 成功:{} 失败:{}",
                        success + failure,
                        tasks_to_process.len(),
                        success,
                        failure
                    );
                    first_batch = false;
                } else {
                    // 后续批次，向上3行覆盖
                    print!("\x1b[3A\x1b[J"); // 向上3行，清除从这里到屏幕末尾
                    println!(
                        "批次 {}/{}: 处理任务 {}-{}",
                        batch_num,
                        total_batches,
                        batch_start + 1,
                        batch_end
                    );
                    println!("{}", bar);
                    println!(
                        "进度 {}/{} | 成功:{} 失败:{}",
                        success + failure,
                        tasks_to_process.len(),
                        success,
                        failure
                    );
                }

                if batch_end < tasks_to_process.len() {
                    tokio::time::sleep(std::time::Duration::from_secs(rest_duration)).await;
                }
            }

            let mut new_failed_indices = HashSet::new();
            for (original_idx, _) in tasks_to_process.iter() {
                if !tasks[*original_idx].output_path.exists() {
                    new_failed_indices.insert(*original_idx);
                }
            }
            failed_task_indices = new_failed_indices;

            if failed_task_indices.is_empty() && retry_round == 1 {
                println!("\n本轮所有任务都已完成！");
                break;
            }
            if failed_task_indices.is_empty() {
                break;
            }
        }

        executor.finish();
    }

    println!("\n所有任务执行完成，开始音频可用性校验...");
    // 校验，主要为了确保合并时文件有效
    let mut audio_files = Vec::new();
    let mut srt_entries_for_merge = Vec::new();
    let mut sorted_tasks: Vec<_> = tasks.iter().collect();
    sorted_tasks.sort_by_key(|task| task.entry.index);

    for task in sorted_tasks {
        if task.output_path.exists() {
            // 简单检查文件大小
            let metadata = tokio::fs::metadata(&task.output_path).await?;
            if metadata.len() > 44 {
                audio_files.push(task.output_path.clone());
                srt_entries_for_merge.push(task.entry.clone());
            } else {
                eprintln!(
                    "警告: 任务 {} 输出文件无效 (过小)，跳过合并",
                    task.entry.index
                );
            }
        } else {
            eprintln!("警告: 任务 {} 输出文件缺失，跳过合并", task.entry.index);
        }
    }

    if audio_files.is_empty() {
        anyhow::bail!("没有有效的合成音频文件可以合并");
    }

    println!("找到 {} 个有效的音频文件用于合并", audio_files.len());

    // 合并音频
    let final_output = output.join(format!("{}.wav", uuid_str));
    println!(
        "开始合并 {} 个音频文件到: {}",
        audio_files.len(),
        final_output.display()
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(1800),
        audio::AudioSplitter::merge_audio(&audio_files, &final_output),
    )
    .await
    .context("合并音频超时")??;

    println!("完成！原始合并音频已保存到: {}", final_output.display());

    // 变速合并
    if let Some(audio_path) = audio {
        if audio_path.exists() {
            let timed_output = output.join(format!("{}_timed.wav", uuid_str));
            print!("变速处理: ");
            std::io::Write::flush(&mut std::io::stdout()).ok();
            tokio::time::timeout(
                std::time::Duration::from_secs(1800),
                audio::AudioSplitter::merge_audio_with_timing(
                    &audio_files,
                    &srt_entries_for_merge,
                    &timed_output,
                    &audio_path,
                    Some(|current: usize, total: usize, msg: String| {
                        print!(
                            "\r变速处理: [{}{}] {}/{} - {}",
                            "=".repeat((current * 40 / total).max(1)),
                            " ".repeat(40 - (current * 40 / total).max(1)),
                            current,
                            total,
                            msg
                        );
                        std::io::Write::flush(&mut std::io::stdout()).ok();
                    }),
                ),
            )
            .await
            .context("合并变速音频超时")??;
            println!("\r✓ 变速合并完成: {}", timed_output.display());
        } else {
            println!("✗ 原始音频文件不存在: {:?}，跳过变速合并。", audio_path);
        }
    } else {
        println!("✓ 未提供原始音频，跳过变速合并。");
    }

    Ok(())
}

/// 恢复模式：对未完成的任务进行重试，然后直接进行合并
async fn resume_and_merge(
    uuid_str: String,
    tmp_dir: &PathBuf,
    output_task_dir: &PathBuf,
    api_url: String,
    output: PathBuf,
    max_concurrent: usize,
    batch_size: usize,
    rest_duration: u64,
    retry_count: u32,
    audio: PathBuf,
    srt: PathBuf,
) -> Result<()> {
    println!("\n[恢复] 正在解析 SRT 文件...");
    let srt_entries = srt::SrtParser::parse_file(&srt).context("解析 SRT 文件失败")?;
    println!("[恢复] 成功解析 SRT 文件: {} 个条目", srt_entries.len());

    let dummy_audio_path = PathBuf::from("");
    let task_manager = TaskManager::new(
        srt_entries.clone(),
        &dummy_audio_path,
        tmp_dir,
        output_task_dir,
    )?;

    let api_client = std::sync::Arc::new(api::ApiClient::new(api_url.clone()));
    let executor = std::sync::Arc::new(TaskExecutor::new(api_client, max_concurrent, retry_count));

    let tasks = task_manager.get_tasks();

    // 找出未完成的任务
    let mut pending_indices = Vec::new();
    for (idx, task) in tasks.iter().enumerate() {
        if !task.tmp_audio.exists() {
            continue;
        }

        let completed = match tokio::fs::metadata(&task.output_path).await {
            Ok(m) => m.len() > 0,
            Err(_) => false,
        };

        if !completed {
            pending_indices.push(idx);
        }
    }

    println!(
        "[恢复] 待处理任务: {} / {}",
        pending_indices.len(),
        tasks.len()
    );

    if !pending_indices.is_empty() {
        executor.set_total(pending_indices.len() as u64);

        let batch_task_count = batch_size * max_concurrent;
        let mut failed_indices: HashSet<usize> = HashSet::new();
        let mut retry_round = 0;

        loop {
            retry_round += 1;

            if retry_round > 1 {
                if failed_indices.is_empty() {
                    break;
                }
                println!("\n[恢复] 第 {} 轮重试...", retry_round - 1);
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }

            let tasks_to_process: Vec<(usize, &task::Task)> = if retry_round == 1 {
                pending_indices
                    .iter()
                    .map(|&idx| (idx, &tasks[idx]))
                    .collect()
            } else {
                failed_indices
                    .iter()
                    .filter_map(|&idx| tasks.get(idx).map(|task| (idx, task)))
                    .collect()
            };

            if tasks_to_process.is_empty() && retry_round > 1 {
                break;
            }

            println!(
                "[恢复] 处理 {} 个任务（第 {} 轮）...",
                tasks_to_process.len(),
                retry_round
            );

            // 每轮只设置一次进度
            executor.set_total(tasks_to_process.len() as u64);

            for batch_start in (0..tasks_to_process.len()).step_by(batch_task_count) {
                let batch_end = (batch_start + batch_task_count).min(tasks_to_process.len());
                let batch_tasks = &tasks_to_process[batch_start..batch_end];

                let futures: Vec<_> = batch_tasks
                    .iter()
                    .map(|(_, task)| {
                        let task = (*task).clone();
                        let executor = executor.clone();
                        async move { executor.execute_task(task).await }
                    })
                    .collect();

                let _ = futures::future::join_all(futures).await;

                let (success, failure) = executor.get_stats();
                println!(
                    "[恢复] 进度 {}/{} | 成功: {} | 失败: {}",
                    success + failure,
                    tasks_to_process.len(),
                    success,
                    failure
                );

                if batch_end < tasks_to_process.len() {
                    tokio::time::sleep(std::time::Duration::from_secs(rest_duration)).await;
                }
            }

            let mut new_failed = HashSet::new();
            for (original_idx, _) in tasks_to_process.iter() {
                if !tasks[*original_idx].output_path.exists() {
                    new_failed.insert(*original_idx);
                }
            }
            failed_indices = new_failed;

            if failed_indices.is_empty() {
                break;
            }
        }

        executor.finish();
    }

    println!("\n[恢复] 收集音频文件用于合并...");

    let mut audio_files = Vec::new();
    let mut srt_entries_for_merge = Vec::new();
    let mut sorted_tasks: Vec<_> = tasks.iter().collect();
    sorted_tasks.sort_by_key(|task| task.entry.index);

    for task in sorted_tasks {
        if task.output_path.exists() {
            let metadata = tokio::fs::metadata(&task.output_path).await?;
            if metadata.len() > 44 {
                audio_files.push(task.output_path.clone());
                srt_entries_for_merge.push(task.entry.clone());
            }
        }
    }

    if audio_files.is_empty() {
        anyhow::bail!("没有有效的合成音频文件可以合并");
    }

    println!(
        "[恢复] 找到 {} 个有效的音频文件，开始合并...",
        audio_files.len()
    );

    let final_output = output.join(format!("{}.wav", uuid_str));
    tokio::time::timeout(
        std::time::Duration::from_secs(1800),
        audio::AudioSplitter::merge_audio(&audio_files, &final_output),
    )
    .await
    .context("合并音频超时（超过 30 分钟）")??;

    println!("[恢复] 原始合并完成: {}", final_output.display());

    let timed_output = output.join(format!("{}_timed.wav", uuid_str));
    tokio::time::timeout(
        std::time::Duration::from_secs(1800),
        audio::AudioSplitter::merge_audio_with_timing(
            &audio_files,
            &srt_entries_for_merge,
            &timed_output,
            &audio,
            None::<fn(usize, usize, String)>,
        ),
    )
    .await
    .context("合并变速音频超时（超过 30 分钟）")??;

    println!("[恢复] 变速合并完成: {}", timed_output.display());

    Ok(())
}
