mod api;
mod audio;
mod srt;
mod task;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use task::{TaskExecutor, TaskManager};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "ttsvii")]
#[command(about = "语音克隆批量处理工具", long_about = None)]
struct Args {
    /// API 基础 URL
    #[arg(long, default_value = "http://183.147.142.111:63364")]
    api_url: String,

    /// 音频文件路径
    #[arg(short, long)]
    audio: PathBuf,

    /// SRT 字幕文件路径
    #[arg(short, long)]
    srt: PathBuf,

    /// 输出目录
    #[arg(short, long, default_value = "./output")]
    output: PathBuf,

    /// 最大并发数（API 请求）
    #[arg(long, default_value_t = 10)]
    max_concurrent: usize,

    /// 音频切分并发数（建议不超过 CPU 核心数）
    #[arg(long, default_value_t = 4)]
    split_concurrent: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 检查 ffmpeg
    audio::AudioSplitter::check_ffmpeg()
        .await
        .context("请确保已安装 ffmpeg 并在 PATH 中")?;

    // 生成本次运行的 UUID
    let task_uuid = Uuid::new_v4();
    let uuid_str = task_uuid.to_string();
    println!("任务 ID: {}", uuid_str);

    // 创建输出目录
    tokio::fs::create_dir_all(&args.output).await?;

    // 创建临时目录（在当前工作目录的 tmp/{uuid} 目录）
    let tmp_dir = std::env::current_dir()?.join("tmp").join(&uuid_str);
    tokio::fs::create_dir_all(&tmp_dir).await?;

    // 创建输出目录（output/{uuid} 目录）
    let output_task_dir = args.output.join(&uuid_str);
    tokio::fs::create_dir_all(&output_task_dir).await?;

    // 使用 defer 模式确保临时目录被清理
    let result = async {
        println!("正在解析 SRT 文件...");
        let srt_entries = srt::SrtParser::parse_file(&args.srt).context("解析 SRT 文件失败")?;
        println!("找到 {} 个字幕条目", srt_entries.len());

        // 创建任务管理器
        let task_manager = TaskManager::new(srt_entries, &args.audio, &tmp_dir, &output_task_dir)?;

        println!("正在切分音频文件（并发数: {}）...", args.split_concurrent);
        task_manager
            .prepare_audio_segments(&args.audio, args.split_concurrent)
            .await?;

        // 创建 API 客户端
        let api_client = std::sync::Arc::new(api::ApiClient::new(args.api_url.clone()));

        // 创建任务执行器
        let executor = TaskExecutor::new(api_client, args.max_concurrent);
        executor.set_total(task_manager.len() as u64);

        println!(
            "开始处理 {} 个任务（并发数: {}）...",
            task_manager.len(),
            args.max_concurrent
        );

        // 执行所有任务
        let tasks = task_manager.get_tasks();
        let futures: Vec<_> = tasks
            .iter()
            .map(|task| executor.execute_task(task.clone()))
            .collect();

        let results = futures::future::join_all(futures).await;

        // 检查结果
        let mut errors = Vec::new();
        let mut skipped = Vec::new();
        for (i, result) in results.into_iter().enumerate() {
            if let Err(e) = result {
                let err_msg = e.to_string();
                // 区分可跳过的错误（空文本、空音频）和真正的 API 错误
                if err_msg.contains("文本内容为空")
                    || err_msg.contains("音频文件为空")
                    || err_msg.contains("音频文件不存在")
                {
                    skipped.push((i, err_msg));
                } else {
                    errors.push((i, e));
                }
            }
        }

        executor.finish();

        if !skipped.is_empty() {
            println!("跳过 {} 个无效任务（空文本或空音频）", skipped.len());
        }

        if !errors.is_empty() {
            eprintln!("有 {} 个任务失败:", errors.len());
            // 只显示前 20 个错误，避免输出过多
            let display_count = errors.len().min(20);
            for (i, err) in errors.iter().take(display_count) {
                eprintln!("  任务 {}: {}", i, err);
            }
            if errors.len() > 20 {
                eprintln!("  ... 还有 {} 个错误未显示", errors.len() - 20);
            }

            // 显示统计信息
            let success_count = tasks.len() - errors.len() - skipped.len();
            println!(
                "\n统计: 成功: {} 个, 失败: {} 个, 跳过: {} 个",
                success_count,
                errors.len(),
                skipped.len()
            );

            anyhow::bail!("部分任务执行失败");
        }

        println!("所有任务执行完成，正在合并音频...");

        // 收集所有合成的音频文件（只包含实际存在的文件）
        let mut audio_files = Vec::new();
        for task in tasks.iter() {
            if task.output_path.exists() {
                let metadata = tokio::fs::metadata(&task.output_path).await?;
                if metadata.len() > 0 {
                    audio_files.push(task.output_path.clone());
                }
            }
        }

        if audio_files.is_empty() {
            anyhow::bail!("没有有效的合成音频文件可以合并");
        }

        println!("找到 {} 个有效的音频文件用于合并", audio_files.len());

        // 按索引排序
        audio_files.sort();

        // 合并音频（保存到 output/{uuid}.wav）
        let final_output = args.output.join(format!("{}.wav", uuid_str));
        audio::AudioSplitter::merge_audio(&audio_files, &final_output).await?;

        println!("完成！最终音频已保存到: {}", final_output.display());

        Ok::<(), anyhow::Error>(())
    }
    .await;

    // 无论成功还是失败，都清理临时目录
    println!("正在清理临时文件...");
    if let Err(e) = tokio::fs::remove_dir_all(&tmp_dir).await {
        eprintln!("警告: 清理临时目录失败: {}", e);
    } else {
        println!("临时文件已清理");
    }

    result
}
