use crate::task::{TaskExecutor, TaskManager};
use crate::{api, audio, srt};
use anyhow::{Context, Result};
use axum::{
    extract::{ws::WebSocketUpgrade, State},
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

/// 查找文件路径，如果路径不存在，尝试在常见位置查找
fn find_file_path(path_str: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path_str);
    
    // 如果路径存在，直接返回
    if path.exists() {
        return Ok(path);
    }
    
    // 如果只是文件名，尝试在常见位置查找
    if path.parent().is_none() || path.parent() == Some(std::path::Path::new("")) {
        let filename = path.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("无效的文件名: {}", path_str))?;
        
        // 尝试在当前工作目录查找
        let current_dir = std::env::current_dir()?;
        let possible_paths = vec![
            current_dir.join(filename),
            current_dir.join("tests").join(filename),
            current_dir.join("uploads").join(filename),
        ];
        
        for possible_path in possible_paths {
            if possible_path.exists() {
                return Ok(possible_path);
            }
        }
        
        anyhow::bail!("文件不存在且无法找到: {}", path_str);
    }
    
    anyhow::bail!("文件不存在: {}", path_str);
}

async fn start_task(
    State(state): State<AppState>,
    Json(config): Json<TaskConfig>,
) -> Json<serde_json::Value> {
    // 验证必需的文件路径
    if config.audio.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "未提供音频文件路径"
        }));
    }

    if config.srt.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "未提供 SRT 字幕文件路径"
        }));
    }

    // 验证文件是否存在，如果路径不存在，尝试查找文件
    let audio_path = match find_file_path(&config.audio) {
        Ok(path) => path,
        Err(e) => {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("音频文件: {}", e)
            }));
        }
    };
    
    let srt_path = match find_file_path(&config.srt) {
        Ok(path) => path,
        Err(e) => {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("SRT 字幕文件: {}", e)
            }));
        }
    };
    
    // 更新配置中的路径为实际找到的路径
    let mut final_config = config;
    final_config.audio = audio_path.to_string_lossy().to_string();
    final_config.srt = srt_path.to_string_lossy().to_string();

    match state.start_task(final_config).await {
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
