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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub api_url: String,
    pub audio: String,
    pub srt: String,
    pub output: String,
    pub max_concurrent: usize,
    pub split_concurrent: usize,
    pub batch_size: usize,
    pub rest_duration: u64,
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
                let executor = Arc::new(TaskExecutor::new(api_client, config.max_concurrent));
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

                // 合并音频
                let final_output = output_path.join(format!("{}.wav", uuid_str));
                audio::AudioSplitter::merge_audio(&audio_files, &final_output).await?;

                state
                    .update_status(TaskStatus {
                        state: "completed".to_string(),
                        progress: tasks_len as u64,
                        total: tasks_len as u64,
                        success: success_count as u64,
                        failure: 0,
                        skipped: skipped.len() as u64,
                        current_batch: total_batches,
                        total_batches,
                        message: format!("完成！最终音频已保存到: {}", final_output.display()),
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
    let state = AppState::new();

    Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(get_status))
        .route("/api/start", post(start_task))
        .route("/api/stop", post(stop_task))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
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
    mut multipart: Multipart,
) -> Json<serde_json::Value> {
    // 创建临时目录用于存储上传的文件
    let upload_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("uploads");

    if let Err(e) = tokio::fs::create_dir_all(&upload_dir).await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("创建上传目录失败: {}", e)
        }));
    }

    let mut config = TaskConfig {
        api_url: "http://183.147.142.111:63364".to_string(),
        audio: String::new(),
        srt: String::new(),
        output: "./output".to_string(),
        max_concurrent: 10,
        split_concurrent: 4,
        batch_size: 5,
        rest_duration: 1,
    };

    // 解析 multipart 数据
    while let Some(field) = multipart.next_field().await.unwrap_or_else(|e| {
        eprintln!("解析 multipart 字段失败: {}", e);
        None
    }) {
        let name = field.name().unwrap_or("").to_string();

        if name == "audio" || name == "srt" {
            // 处理文件上传
            let original_filename = field
                .file_name()
                .unwrap_or(if name == "audio" { "audio" } else { "subtitle" })
                .to_string();

            // 生成唯一文件名（使用 UUID + 原始文件名）
            let unique_id = Uuid::new_v4().to_string();
            let extension = std::path::Path::new(&original_filename)
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or(if name == "audio" { "mp3" } else { "srt" });

            let filename = format!("{}_{}.{}", name, unique_id, extension);
            let file_path = upload_dir.join(&filename);

            // 读取文件内容
            let data = match field.bytes().await {
                Ok(data) => data,
                Err(e) => {
                    return Json(serde_json::json!({
                        "success": false,
                        "error": format!("读取文件 {} 失败: {}", name, e)
                    }));
                }
            };

            // 保存文件
            if let Err(e) = tokio::fs::write(&file_path, &data).await {
                return Json(serde_json::json!({
                    "success": false,
                    "error": format!("保存文件 {} 失败: {}", name, e)
                }));
            }

            if name == "audio" {
                config.audio = file_path.to_string_lossy().to_string();
            } else if name == "srt" {
                config.srt = file_path.to_string_lossy().to_string();
            }
        } else {
            // 处理文本字段
            let value = match field.text().await {
                Ok(v) => v,
                Err(_) => continue,
            };

            match name.as_str() {
                "api_url" => config.api_url = value,
                "output" => config.output = value,
                "max_concurrent" => {
                    if let Ok(v) = value.parse() {
                        config.max_concurrent = v;
                    }
                }
                "split_concurrent" => {
                    if let Ok(v) = value.parse() {
                        config.split_concurrent = v;
                    }
                }
                "batch_size" => {
                    if let Ok(v) = value.parse() {
                        config.batch_size = v;
                    }
                }
                "rest_duration" => {
                    if let Ok(v) = value.parse() {
                        config.rest_duration = v;
                    }
                }
                _ => {}
            }
        }
    }

    // 验证必需的文件
    if config.audio.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "未提供音频文件"
        }));
    }

    if config.srt.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "未提供 SRT 字幕文件"
        }));
    }

    match state.start_task(config).await {
        Ok(_) => Json(serde_json::json!({ "success": true })),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
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
