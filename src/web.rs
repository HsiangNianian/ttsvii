use crate::task::{TaskExecutor, TaskManager};
use crate::{api, audio, srt};
use anyhow::{Context, Result};
use axum::{
    extract::{ws::WebSocketUpgrade, Multipart, State},
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

fn default_retry_count() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub api_url: String,
    #[serde(default)]
    pub audio: String,
    #[serde(default)]
    pub srt: String,
    #[serde(default)]
    pub audio_filename: String,
    #[serde(default)]
    pub audio_content: String,
    #[serde(default)]
    pub srt_filename: String,
    #[serde(default)]
    pub srt_content: String,
    pub output: String,
    pub max_concurrent: usize,
    pub split_concurrent: usize,
    pub batch_size: usize,
    pub rest_duration: u64,
    #[serde(default = "default_retry_count")]
    pub retry_count: u32,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatus {
    pub state: String, // "idle", "running", "paused", "completed", "error"
    pub progress: u64,
    pub total: u64,
    pub success: u64,
    pub failure: u64,
    pub skipped: u64,
    pub current_batch: usize,
    pub total_batches: usize,
    pub message: String,
    pub task_id: Option<String>,
    pub output_path: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub status: Arc<RwLock<TaskStatus>>,
    pub should_stop: Arc<AtomicBool>,
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<Result<()>>>>>,
    pub tx: broadcast::Sender<TaskStatus>,
}

impl AppState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<TaskStatus>(100);
        Self {
            status: Arc::new(RwLock::new(TaskStatus {
                state: "idle".to_string(),
                progress: 0,
                total: 0,
                success: 0,
                failure: 0,
                skipped: 0,
                current_batch: 0,
                total_batches: 0,
                message: "就绪".to_string(),
                task_id: None,
                output_path: None,
            })),
            should_stop: Arc::new(AtomicBool::new(false)),
            task_handle: Arc::new(RwLock::new(None)),
            tx,
        }
    }

    pub async fn update_status(&self, status: TaskStatus) {
        *self.status.write().await = status.clone();
        let _ = self.tx.send(status);
    }

    pub async fn start_task(&self, config: TaskConfig) -> Result<()> {
        // 检查是否已有任务在运行
        if let Some(handle) = self.task_handle.read().await.as_ref() {
            if !handle.is_finished() {
                anyhow::bail!("已有任务正在运行");
            }
        }

        self.should_stop.store(false, Ordering::Relaxed);
        let state = self.clone();

        let handle = tokio::spawn(async move {
            state
                .update_status(TaskStatus {
                    state: "running".to_string(),
                    progress: 0,
                    total: 0,
                    success: 0,
                    failure: 0,
                    skipped: 0,
                    current_batch: 0,
                    total_batches: 0,
                    message: "正在初始化...".to_string(),
                    task_id: None,
                    output_path: None,
                })
                .await;

            let result = async {
                // 检查 ffmpeg
                audio::AudioSplitter::check_ffmpeg()
                    .await
                    .context("请确保已安装 ffmpeg 并在 PATH 中")?;

                // 生成本次运行的 UUID
                let task_uuid = Uuid::new_v4();
                let uuid_str = task_uuid.to_string();

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: 0,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: format!("任务 ID: {}", uuid_str),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 创建输出目录
                let output_path = PathBuf::from(&config.output);
                tokio::fs::create_dir_all(&output_path).await?;

                // 创建临时目录
                let tmp_dir = std::env::current_dir()?.join("tmp").join(&uuid_str);
                tokio::fs::create_dir_all(&tmp_dir).await?;

                // 创建输出目录
                let output_task_dir = output_path.join(&uuid_str);
                tokio::fs::create_dir_all(&output_task_dir).await?;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: 0,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: "正在解析 SRT 文件...".to_string(),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                let srt_entries = srt::SrtParser::parse_file(&PathBuf::from(&config.srt))
                    .context("解析 SRT 文件失败")?;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: srt_entries.len() as u64,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: format!("找到 {} 个字幕条目", srt_entries.len()),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 创建任务管理器
                let task_manager = TaskManager::new(
                    srt_entries,
                    &PathBuf::from(&config.audio),
                    &tmp_dir,
                    &output_task_dir,
                )?;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: task_manager.len() as u64,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: format!(
                            "正在切分音频文件（并发数: {}）...",
                            config.split_concurrent
                        ),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                task_manager
                    .prepare_audio_segments(&PathBuf::from(&config.audio), config.split_concurrent)
                    .await?;

                // 创建 API 客户端
                let api_client = Arc::new(api::ApiClient::new(config.api_url.clone()));

                // 创建任务执行器
                let executor = Arc::new(TaskExecutor::new(
                    api_client,
                    config.max_concurrent,
                    config.retry_count,
                ));
                executor.set_total(task_manager.len() as u64);

                let tasks = task_manager.get_tasks();
                let tasks_len = tasks.len();
                let batch_task_count = config.batch_size * config.max_concurrent;
                let total_batches = (tasks_len + batch_task_count - 1) / batch_task_count;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: tasks_len as u64,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches,
                        message: format!(
                            "开始处理 {} 个任务（并发数: {}, 批次大小: {}, 休息时间: {}s）...",
                            tasks_len,
                            config.max_concurrent,
                            config.batch_size,
                            config.rest_duration
                        ),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                let mut all_results = Vec::new();
                let mut batch_num = 0;

                for batch_start in (0..tasks_len).step_by(batch_task_count) {
                    // 检查是否需要停止
                    if state.should_stop.load(Ordering::Relaxed) {
                        state
                            .update_status(TaskStatus {
                                state: "paused".to_string(),
                                progress: all_results.len() as u64,
                                total: tasks_len as u64,
                                success: 0,
                                failure: 0,
                                skipped: 0,
                                current_batch: batch_num,
                                total_batches,
                                message: "任务已暂停".to_string(),
                                task_id: Some(uuid_str.clone()),
                                output_path: None,
                            })
                            .await;
                        anyhow::bail!("任务已停止");
                    }

                    batch_num += 1;
                    let batch_end = (batch_start + batch_task_count).min(tasks_len);
                    let batch_tasks = &tasks[batch_start..batch_end];

                    state
                        .update_status(TaskStatus {
                            state: "running".to_string(),
                            progress: all_results.len() as u64,
                            total: tasks_len as u64,
                            success: 0,
                            failure: 0,
                            skipped: 0,
                            current_batch: batch_num,
                            total_batches,
                            message: format!(
                                "批次 {}/{}: 处理任务 {}-{}",
                                batch_num,
                                total_batches,
                                batch_start + 1,
                                batch_end
                            ),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;

                    // 执行当前批次
                    let futures: Vec<_> = batch_tasks
                        .iter()
                        .map(|task| {
                            let task = task.clone();
                            let executor = executor.clone();
                            async move { executor.execute_task(task).await }
                        })
                        .collect();

                    let batch_results = futures::future::join_all(futures).await;
                    all_results.extend(batch_results);

                    // 更新统计
                    let (success, failure) = executor.get_stats();
                    let completed = success + failure;

                    state
                        .update_status(TaskStatus {
                            state: "running".to_string(),
                            progress: completed,
                            total: tasks_len as u64,
                            success,
                            failure,
                            skipped: 0,
                            current_batch: batch_num,
                            total_batches,
                            message: format!(
                                "实时统计: 进度 {}/{} | 成功: {} | 失败: {}",
                                completed, tasks_len, success, failure
                            ),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;

                    // 如果不是最后一批，休息一下
                    if batch_end < tasks_len {
                        state
                            .update_status(TaskStatus {
                                state: "running".to_string(),
                                progress: completed,
                                total: tasks_len as u64,
                                success,
                                failure,
                                skipped: 0,
                                current_batch: batch_num,
                                total_batches,
                                message: format!("休息 {} 秒...", config.rest_duration),
                                task_id: Some(uuid_str.clone()),
                                output_path: None,
                            })
                            .await;
                        tokio::time::sleep(std::time::Duration::from_secs(config.rest_duration))
                            .await;
                    }
                }

                let results = all_results;

                // 检查结果
                let mut errors = Vec::new();
                let mut skipped = Vec::new();
                for (i, result) in results.into_iter().enumerate() {
                    if let Err(e) = result {
                        let err_msg = e.to_string();
                        if err_msg.contains("文本内容为空")
                            || err_msg.contains("音频文件为空")
                            || err_msg.contains("音频文件不存在")
                            || err_msg.contains("音频文件无效")
                            || err_msg.contains("切分的音频文件无效")
                        {
                            skipped.push((i, err_msg));
                        } else {
                            errors.push((i, e));
                        }
                    }
                }

                executor.finish();

                let success_count = tasks_len - errors.len() - skipped.len();

                if !skipped.is_empty() {
                    state
                        .update_status(TaskStatus {
                            state: "running".to_string(),
                            progress: tasks_len as u64,
                            total: tasks_len as u64,
                            success: success_count as u64,
                            failure: errors.len() as u64,
                            skipped: skipped.len() as u64,
                            current_batch: total_batches,
                            total_batches,
                            message: format!("跳过 {} 个无效任务", skipped.len()),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;
                }

                if !errors.is_empty() {
                    state
                        .update_status(TaskStatus {
                            state: "error".to_string(),
                            progress: tasks_len as u64,
                            total: tasks_len as u64,
                            success: success_count as u64,
                            failure: errors.len() as u64,
                            skipped: skipped.len() as u64,
                            current_batch: total_batches,
                            total_batches,
                            message: format!("有 {} 个任务失败", errors.len()),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;
                    anyhow::bail!("部分任务执行失败");
                }

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: tasks_len as u64,
                        total: tasks_len as u64,
                        success: success_count as u64,
                        failure: 0,
                        skipped: skipped.len() as u64,
                        current_batch: total_batches,
                        total_batches,
                        message: "所有任务执行完成，正在合并音频...".to_string(),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 收集所有合成的音频文件
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

                audio_files.sort();

                // 收集对应的 SRT 条目（按索引排序）
                let mut srt_entries_for_merge = Vec::new();
                let mut sorted_tasks: Vec<_> = tasks.iter().collect();
                sorted_tasks.sort_by_key(|task| task.entry.index);
                for task in sorted_tasks {
                    if task.output_path.exists() {
                        let metadata = tokio::fs::metadata(&task.output_path).await?;
                        if metadata.len() > 0 {
                            srt_entries_for_merge.push(task.entry.clone());
                        }
                    }
                }

                // 合并音频
                let final_output = output_path.join(format!("{}.wav", uuid_str));
                audio::AudioSplitter::merge_audio(&audio_files, &final_output).await?;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: tasks_len as u64,
                        total: tasks_len as u64,
                        success: success_count as u64,
                        failure: 0,
                        skipped: skipped.len() as u64,
                        current_batch: total_batches,
                        total_batches,
                        message: format!("原始音频合并完成: {}", final_output.display()),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 生成根据 SRT 时间戳变速后的合并音频
                let timed_output = output_path.join(format!("{}_timed.wav", uuid_str));
                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: tasks_len as u64,
                        total: tasks_len as u64,
                        success: success_count as u64,
                        failure: 0,
                        skipped: skipped.len() as u64,
                        current_batch: total_batches,
                        total_batches,
                        message: format!(
                            "开始生成根据 SRT 时间戳变速后的合并音频: {}",
                            timed_output.display()
                        ),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 克隆需要的变量用于闭包
                let state_for_callback = state.clone();
                let uuid_str_for_callback = uuid_str.clone();
                let tasks_len_for_callback = tasks_len;
                let success_count_for_callback = success_count;
                let skipped_len_for_callback = skipped.len();
                let total_batches_for_callback = total_batches;
                let audio_files_len = audio_files.len();

                // 总进度 = 原始任务数 + 变速处理的文件数
                let total_progress = tasks_len_for_callback + audio_files_len;

                // 获取原始音频路径（用于计算总时长和空白时间段）
                let original_audio_path = PathBuf::from(&config.audio);

                audio::AudioSplitter::merge_audio_with_timing(
                    &audio_files,
                    &srt_entries_for_merge,
                    &timed_output,
                    &original_audio_path,
                    Some(move |current, total, msg: String| {
                        // 在异步上下文中更新状态
                        let state_clone = state_for_callback.clone();
                        let uuid_str_clone = uuid_str_for_callback.clone();
                        let tasks_len_clone = tasks_len_for_callback;
                        let success_count_clone = success_count_for_callback;
                        let skipped_len_clone = skipped_len_for_callback;
                        let total_batches_clone = total_batches_for_callback;
                        let total_progress_clone = total_progress;
                        // 当前进度 = 已完成的任务数 + 当前变速处理的进度
                        let current_progress = tasks_len_clone + current;
                        tokio::spawn(async move {
                            state_clone
                                .update_status(TaskStatus {
                                    state: "running".to_string(),
                                    progress: current_progress as u64,
                                    total: total_progress_clone as u64,
                                    success: success_count_clone as u64,
                                    failure: 0,
                                    skipped: skipped_len_clone as u64,
                                    current_batch: total_batches_clone,
                                    total_batches: total_batches_clone,
                                    message: format!("[变速处理] {}/{} - {}", current, total, msg),
                                    task_id: Some(uuid_str_clone),
                                    output_path: None,
                                })
                                .await;
                        });
                    }),
                )
                .await?;

                // 更新最终完成状态，进度应该是 100%
                let total_progress = tasks_len + audio_files.len();
                state
                    .update_status(TaskStatus {
                        state: "completed".to_string(),
                        progress: total_progress as u64,
                        total: total_progress as u64,
                        success: success_count as u64,
                        failure: 0,
                        skipped: skipped.len() as u64,
                        current_batch: total_batches,
                        total_batches,
                        message: format!(
                            "完成！最终音频已保存到: {} (变速版本: {})",
                            final_output.display(),
                            timed_output.display()
                        ),
                        task_id: Some(uuid_str.clone()),
                        output_path: Some(final_output.display().to_string()),
                    })
                    .await;

                Ok::<(), anyhow::Error>(())
            }
            .await;

            if let Err(ref e) = result {
                let current_status = state.status.read().await.clone();
                let error_msg = e.to_string();
                state
                    .update_status(TaskStatus {
                        state: "error".to_string(),
                        progress: current_status.progress,
                        total: current_status.total,
                        success: current_status.success,
                        failure: current_status.failure,
                        skipped: current_status.skipped,
                        current_batch: current_status.current_batch,
                        total_batches: current_status.total_batches,
                        message: format!("错误: {}", error_msg),
                        task_id: current_status.task_id,
                        output_path: current_status.output_path,
                    })
                    .await;
            }

            result
        });

        *self.task_handle.write().await = Some(handle);
        Ok(())
    }

    pub async fn stop_task(&self) {
        self.should_stop.store(true, Ordering::Relaxed);
    }
}

pub async fn create_router() -> Router {
    use axum::body::Body;
    use axum::http::{header, StatusCode};
    use axum::response::Response;
    use std::path::Path;
    use tower::ServiceBuilder;
    use tower_http::limit::RequestBodyLimitLayer;

    let state = AppState::new();

    // 静态文件服务处理器（用于提供音频文件）
    async fn serve_audio_file(
        path: axum::extract::Path<String>,
    ) -> Result<Response<Body>, StatusCode> {
        // 移除开头的 ./ 或 /
        let file_path = path.trim_start_matches("./").trim_start_matches("/");
        let path_buf = Path::new(file_path);

        // 安全检查：只允许访问 output 目录下的文件
        if !path_buf.starts_with("output/") {
            return Err(StatusCode::FORBIDDEN);
        }

        // 检查文件是否存在
        if !path_buf.exists() {
            return Err(StatusCode::NOT_FOUND);
        }

        // 读取文件
        match tokio::fs::read(&path_buf).await {
            Ok(data) => {
                // 根据文件扩展名设置 Content-Type
                let content_type = if path_buf.extension().and_then(|s| s.to_str()) == Some("wav") {
                    "audio/wav"
                } else {
                    "application/octet-stream"
                };

                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CACHE_CONTROL, "public, max-age=3600")
                    .body(Body::from(data))
                    .unwrap())
            }
            Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(get_status))
        .route("/api/start", post(start_task))
        .route("/api/stop", post(stop_task))
        .route("/api/upload/chunk", post(upload_chunk))
        .route("/api/upload/check", get(check_upload))
        .route("/api/upload/merge", post(merge_upload))
        .route("/api/audio/*path", get(serve_audio_file))
        .route("/ws", get(ws_handler))
        .layer(
            ServiceBuilder::new()
                .layer(RequestBodyLimitLayer::new(3 * 1024 * 1024)) // 3MB 限制（每个分块 1MB，加上元数据足够）
                .layer(CorsLayer::permissive()),
        )
        .with_state(state)
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../webui/index.html"))
}

async fn get_status(State(state): State<AppState>) -> Json<TaskStatus> {
    Json(state.status.read().await.clone())
}

async fn start_task(
    State(state): State<AppState>,
    Json(config): Json<TaskConfig>,
) -> Json<serde_json::Value> {
    // 验证文件路径
    let audio_path = std::path::Path::new(&config.audio);
    if !audio_path.exists() {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("音频/视频文件不存在: {}", config.audio)
        }));
    }

    // 如果是视频文件，先提取音频
    let final_audio_path = if let Some(ext) = audio_path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        if matches!(
            ext_lower.as_str(),
            "mp4" | "avi" | "mov" | "mkv" | "webm" | "flv" | "wmv"
        ) {
            // 视频文件，需要提取音频
            let temp_audio = std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join("tmp")
                .join(format!("extracted_audio_{}.wav", uuid::Uuid::new_v4()));

            if let Err(e) = tokio::fs::create_dir_all(temp_audio.parent().unwrap()).await {
                return Json(serde_json::json!({
                    "success": false,
                    "error": format!("创建临时目录失败: {}", e)
                }));
            }

            // 使用 ffmpeg 提取音频
            let output = tokio::process::Command::new("ffmpeg")
                .arg("-i")
                .arg(&config.audio)
                .arg("-vn") // 不包含视频
                .arg("-acodec")
                .arg("pcm_s16le") // PCM 16-bit little-endian
                .arg("-ar")
                .arg("44100") // 采样率
                .arg("-ac")
                .arg("2") // 立体声
                .arg("-y")
                .arg(&temp_audio)
                .output()
                .await;

            match output {
                Ok(output) if output.status.success() => temp_audio,
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Json(serde_json::json!({
                        "success": false,
                        "error": format!("提取视频音频失败: {}", stderr)
                    }));
                }
                Err(e) => {
                    return Json(serde_json::json!({
                        "success": false,
                        "error": format!("执行 ffmpeg 失败: {}", e)
                    }));
                }
            }
        } else {
            // 音频文件，直接使用
            audio_path.to_path_buf()
        }
    } else {
        // 无扩展名，假设是音频文件
        audio_path.to_path_buf()
    };

    // 创建新的配置，使用提取后的音频路径
    let mut final_config = config.clone();
    final_config.audio = final_audio_path.to_string_lossy().to_string();

    let srt_path = std::path::Path::new(&config.srt);
    if !srt_path.exists() {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("SRT 字幕文件不存在: {}", config.srt)
        }));
    }

    match state.start_task(config).await {
        Ok(_) => Json(serde_json::json!({ "success": true })),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

// 上传分块
async fn upload_chunk(mut multipart: Multipart) -> Json<serde_json::Value> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    // 存储上传信息（实际应用中应该使用数据库或文件系统）
    static UPLOAD_INFO: OnceLock<Mutex<HashMap<String, Vec<usize>>>> = OnceLock::new();
    let upload_info = UPLOAD_INFO.get_or_init(|| Mutex::new(HashMap::new()));

    let temp_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("tmp")
        .join("uploads");

    if let Err(e) = tokio::fs::create_dir_all(&temp_dir).await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("创建临时目录失败: {}", e)
        }));
    }

    let mut upload_id = String::new();
    let mut file_type = String::new();
    let mut chunk_index = 0usize;
    let mut chunk_data = Vec::new();

    // 解析 multipart 数据
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or("").to_string();

                match name.as_str() {
                    "chunk" => match field.bytes().await {
                        Ok(data) => chunk_data = data.to_vec(),
                        Err(e) => {
                            eprintln!("读取分块数据失败: {}", e);
                        }
                    },
                    "upload_id" => match field.text().await {
                        Ok(text) => upload_id = text,
                        Err(e) => {
                            eprintln!("读取 upload_id 失败: {}", e);
                        }
                    },
                    "file_type" => match field.text().await {
                        Ok(text) => file_type = text,
                        Err(e) => {
                            eprintln!("读取 file_type 失败: {}", e);
                        }
                    },
                    "chunk_index" => match field.text().await {
                        Ok(text) => {
                            chunk_index = text.parse().unwrap_or(0);
                        }
                        Err(e) => {
                            eprintln!("读取 chunk_index 失败: {}", e);
                        }
                    },
                    _ => {
                        // 忽略其他字段（如 filename, file_size, total_chunks）
                        // 但需要读取数据以推进解析器，否则会阻塞
                        let _ = field.bytes().await;
                    }
                }
            }
            Ok(None) => {
                // 所有字段都已处理完
                break;
            }
            Err(e) => {
                eprintln!("解析 multipart 字段失败: {}", e);
                eprintln!("错误详情: {:?}", e);
                return Json(serde_json::json!({
                    "success": false,
                    "error": format!("解析 multipart 数据失败: {}. 请检查文件大小是否超过限制（5MB）。", e)
                }));
            }
        }
    }

    if upload_id.is_empty() || file_type.is_empty() || chunk_data.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "缺少必要参数"
        }));
    }

    // 保存分块
    let chunk_dir = temp_dir.join(&upload_id);
    if let Err(e) = tokio::fs::create_dir_all(&chunk_dir).await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("创建分块目录失败: {}", e)
        }));
    }

    let chunk_path = chunk_dir.join(format!("chunk_{}", chunk_index));
    if let Err(e) = tokio::fs::write(&chunk_path, &chunk_data).await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("保存分块失败: {}", e)
        }));
    }

    // 记录已上传的分块
    let key = format!("{}_{}", upload_id, file_type);
    let mut info = upload_info.lock().unwrap();
    let chunks = info.entry(key.clone()).or_insert_with(Vec::new);
    if !chunks.contains(&chunk_index) {
        chunks.push(chunk_index);
    }

    Json(serde_json::json!({
        "success": true,
        "chunk_index": chunk_index
    }))
}

// 检查已上传的分块
async fn check_upload(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static UPLOAD_INFO: OnceLock<Mutex<HashMap<String, Vec<usize>>>> = OnceLock::new();
    let upload_info = UPLOAD_INFO.get_or_init(|| Mutex::new(HashMap::new()));

    let upload_id = params.get("upload_id").cloned().unwrap_or_default();
    let file_type = params.get("file_type").cloned().unwrap_or_default();

    if upload_id.is_empty() || file_type.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "缺少必要参数"
        }));
    }

    let key = format!("{}_{}", upload_id, file_type);
    let info = upload_info.lock().unwrap();
    let uploaded_chunks = info.get(&key).cloned().unwrap_or_default();

    Json(serde_json::json!({
        "success": true,
        "uploaded_chunks": uploaded_chunks
    }))
}

// 合并文件
async fn merge_upload(Json(params): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let upload_id = params
        .get("upload_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let file_type = params
        .get("file_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let filename = params
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let total_chunks = params
        .get("total_chunks")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    if upload_id.is_empty() || file_type.is_empty() || total_chunks == 0 {
        return Json(serde_json::json!({
            "success": false,
            "error": "缺少必要参数"
        }));
    }

    let temp_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("tmp")
        .join("uploads");

    let chunk_dir = temp_dir.join(upload_id);
    let unique_id = Uuid::new_v4().to_string();
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or(if file_type == "audio" { "mp3" } else { "srt" });

    let final_filename = format!("{}_{}.{}", file_type, unique_id, extension);
    let final_path = temp_dir.join(&final_filename);

    // 合并所有分块
    let mut output_file = match tokio::fs::File::create(&final_path).await {
        Ok(file) => file,
        Err(e) => {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("创建文件失败: {}", e)
            }));
        }
    };

    use tokio::io::AsyncWriteExt;
    for chunk_index in 0..total_chunks {
        let chunk_path = chunk_dir.join(format!("chunk_{}", chunk_index));
        if !chunk_path.exists() {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("分块 {} 不存在", chunk_index)
            }));
        }

        let chunk_data = match tokio::fs::read(&chunk_path).await {
            Ok(data) => data,
            Err(e) => {
                return Json(serde_json::json!({
                    "success": false,
                    "error": format!("读取分块 {} 失败: {}", chunk_index, e)
                }));
            }
        };

        if let Err(e) = output_file.write_all(&chunk_data).await {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("写入分块 {} 失败: {}", chunk_index, e)
            }));
        }
    }

    if let Err(e) = output_file.sync_all().await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("同步文件失败: {}", e)
        }));
    }

    // 清理分块目录
    let _ = tokio::fs::remove_dir_all(&chunk_dir).await;

    Json(serde_json::json!({
        "success": true,
        "file_path": final_path.to_string_lossy().to_string()
    }))
}

async fn stop_task(State(state): State<AppState>) -> Json<serde_json::Value> {
    state.stop_task().await;
    Json(serde_json::json!({ "success": true }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> axum::response::Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, state: AppState) {
    use futures_util::{SinkExt, StreamExt};

    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    // 发送当前状态
    let current_status = state.status.read().await.clone();
    let _ = sender
        .send(axum::extract::ws::Message::Text(
            serde_json::to_string(&current_status).unwrap(),
        ))
        .await;

    // 监听状态更新
    let mut send_task = tokio::spawn(async move {
        while let Ok(status) = rx.recv().await {
            let msg = axum::extract::ws::Message::Text(serde_json::to_string(&status).unwrap());
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 监听客户端消息（用于保持连接）
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(_)) = receiver.next().await {
            // 忽略客户端消息，仅用于保持连接
        }
    });

    tokio::select! {
        _ = &mut send_task => {},
        _ = &mut recv_task => {},
    }
}
