use axum::{
    extract::{ws::{WebSocket, WebSocketUpgrade, Message}, DefaultBodyLimit, Multipart, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, Mutex};
use uuid::Uuid;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct AppState {
    pub tasks: Arc<RwLock<HashMap<String, TaskRuntime>>>,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[derive(Clone)]
pub struct TaskRuntime {
    pub id: String,
    pub tx: broadcast::Sender<WsMessage>,
    pub files: Arc<Mutex<(Option<PathBuf>, Option<PathBuf>)>>, // (srt_path, audio_path)
    pub name: String,
    pub status: Arc<RwLock<String>>, // running/completed/error
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsMessage {
    pub task_id: String,
    pub msg_type: String, // "log", "status", "error"
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct TaskConfig {
    pub task_name: String,
}

#[derive(Debug, Deserialize)]
pub struct OpenFolderRequest {
    pub path: String,
}

pub async fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .route("/api/tasks", get(list_tasks_handler))
        .route("/api/task/start", post(start_task_handler))
        .route("/api/task/:task_id/stop", post(stop_task_handler))
        .route("/api/task/:task_id/resume", post(resume_task_handler))
        .route("/api/task/:task_id/delete", post(delete_task_handler))
        .route("/api/open-folder", post(open_folder_handler))
    // 允许较大的上传（1GB 上限，根据需要调整）
    .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(state)
}

async fn index_handler() -> impl IntoResponse {
    let html = include_str!("../webui/index.html");
    (StatusCode::OK, [("Content-Type", "text/html; charset=utf-8")], html)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (tx, mut rx) = socket.split();
    let tx = Arc::new(Mutex::new(tx));
    
    // 接收客户端订阅请求
    while let Some(msg) = rx.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // 客户端发送的应该是: {"action": "subscribe", "task_id": "xxx"}
                if let Ok(req) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some("subscribe") = req.get("action").and_then(|v| v.as_str()) {
                        if let Some(task_id) = req.get("task_id").and_then(|v| v.as_str()) {
                            // 为这个客户端订阅该任务的消息
                            if let Some(task) = state.tasks.read().await.get(task_id).cloned() {
                                let mut broadcast_rx = task.tx.subscribe();
                                let tx_clone = Arc::clone(&tx);

                                // 立即推送当前状态，便于刷新后恢复
                                let current_status = task.status.read().await.clone();
                                let init_msg = WsMessage {
                                    task_id: task_id.to_string(),
                                    msg_type: "status".to_string(),
                                    content: current_status,
                                };
                                if let Ok(json) = serde_json::to_string(&init_msg) {
                                    let _ = tx_clone.lock().await.send(Message::Text(json)).await;
                                }

                                // 在后台任务中转发消息
                                tokio::spawn(async move {
                                    while let Ok(msg) = broadcast_rx.recv().await {
                                        if let Ok(json) = serde_json::to_string(&msg) {
                                            let _ = tx_clone.lock().await.send(Message::Text(json)).await;
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Err(_) => break,
            _ => {}
        }
    }
}

async fn start_task_handler(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    println!("收到任务启动请求");
    let task_id = Uuid::new_v4().to_string();
    let (tx, _) = broadcast::channel(100);
    
    let mut srt_path: Option<PathBuf> = None;
    let mut audio_path: Option<PathBuf> = None;
    let mut task_name = "未命名任务".to_string();
    
    // 处理上传的文件
    while let Ok(Some(field)) = multipart.next_field().await {
        if let Some(name) = field.name() {
            let name = name.to_string();
            println!("接收到字段: {}", name);
            
            if name == "task_name" {
                if let Ok(bytes) = field.bytes().await {
                    if let Ok(s) = String::from_utf8(bytes.to_vec()) {
                        if !s.trim().is_empty() {
                            task_name = s.trim().to_string();
                        }
                    }
                }
                continue;
            }

            // 跳过 config，但消费字节避免阻塞
            if name == "config" {
                let _ = field.bytes().await;
                continue;
            }

            match field.bytes().await {
                Ok(bytes) => {
                    println!("字段 {} 大小: {} bytes", name, bytes.len());

                    if bytes.len() == 0 {
                        println!("警告: 字段 {} 为空", name);
                        continue;
                    }

                    let tmp_dir = format!("tmp/{}", task_id);
                    let _ = tokio::fs::create_dir_all(&tmp_dir).await;

                    if name == "srt" {
                        srt_path = Some(PathBuf::from(format!("{}/input.srt", tmp_dir)));
                        match tokio::fs::write(&srt_path.as_ref().unwrap(), bytes).await {
                            Ok(_) => println!("SRT 文件已保存"),
                            Err(e) => println!("SRT 文件保存失败: {}", e),
                        }
                    } else if name == "audio" {
                        audio_path = Some(PathBuf::from(format!("{}/input.audio", tmp_dir)));
                        match tokio::fs::write(&audio_path.as_ref().unwrap(), bytes).await {
                            Ok(_) => println!("音频文件已保存"),
                            Err(e) => println!("音频文件保存失败: {}", e),
                        }
                    }
                }
                Err(e) => {
                    println!("读取字段 {} 失败: {:?}", name, e);
                }
            }
        }
    }
    
    println!("文件接收完成 - SRT: {:?}, Audio: {:?}", srt_path, audio_path);
    
    let task = TaskRuntime {
        id: task_id.clone(),
        tx: tx.clone(),
        files: Arc::new(Mutex::new((srt_path.clone(), audio_path.clone()))),
        name: task_name.clone(),
        status: Arc::new(RwLock::new("running".to_string())),
        created_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64,
    };
    
    state.tasks.write().await.insert(task_id.clone(), task);
    
    // 如果有文件，在后台执行任务
    if let (Some(srt), Some(audio)) = (srt_path, audio_path) {
        println!("启动任务执行");
        let task_id_clone = task_id.clone();
        let state_clone = state.clone();
        
        tokio::spawn(async move {
            execute_task(&task_id_clone, srt, audio, &state_clone).await;
        });
    } else {
        println!("警告: 文件不完整");
    }
    
    Json(serde_json::json!({"task_id": task_id}))
}

async fn execute_task(task_id: &str, srt: PathBuf, audio: PathBuf, state: &AppState) {
    if let Some(task) = state.tasks.read().await.get(task_id) {
        let tx = task.tx.clone();
        let status = task.status.clone();
        
        // 发送开始消息
        let _ = tx.send(WsMessage {
            task_id: task_id.to_string(),
            msg_type: "log".to_string(),
            content: format!("任务开始处理 - SRT: {:?}, 音频: {:?}", srt, audio),
        });
        {
            let mut guard = status.write().await;
            *guard = "running".to_string();
        }
        
        // 创建日志通道捕获 CLI 日志
        let (log_tx, mut log_rx) = tokio::sync::mpsc::channel(100);
        let tx_clone = tx.clone();
        let task_id_clone = task_id.to_string();
        
        // 启动日志转发协程
        tokio::spawn(async move {
            while let Some(msg) = log_rx.recv().await {
                let _ = tx_clone.send(WsMessage {
                    task_id: task_id_clone.clone(),
                    msg_type: "log".to_string(),
                    content: msg,
                });
            }
        });

        // 调用实际的 CLI 逻辑
        // 使用默认配置参数
        let result = crate::run_cli_mode_with_logger(
            "https://models.inference.ai.azure.com".to_string(), // 默认 API URL
            PathBuf::from("./output"),
            5,   // max_concurrent
            3,   // split_concurrent
            20,  // batch_size
            1000, // rest_duration
            3,   // retry_count
            audio,
            srt,
            log_tx,
            Some(task_id.to_string()),
        ).await;
        
        match result {
            Ok(_) => {
                let _ = tx.send(WsMessage {
                    task_id: task_id.to_string(),
                    msg_type: "log".to_string(),
                    content: "✓ 任务完成！".to_string(),
                });
                
                // 发送状态更新
                let _ = tx.send(WsMessage {
                    task_id: task_id.to_string(),
                    msg_type: "status".to_string(),
                    content: "completed".to_string(),
                });

                // 获取任务 ID (UUID) 来确定输出目录
                // run_cli_mode 内部会生成一个 UUID，但 start_task_handler 使用的 task_id 也是 UUID
                // 我们调用 run_cli_mode 时没有传 UUID，它会自己生成一个。
                // 修正：run_cli_mode 应该允许外部传入 UUID 或者返回生成的 UUID。
                // 目前简单起见，假设输出目录就是 ./output/{uuid}
                // 但 run_cli_mode 生成的 UUID 我们拿不到。
                
                // 暂时发送一个通用的状态
                let _ = tx.send(WsMessage {
                    task_id: task_id.to_string(),
                    msg_type: "completed".to_string(),
                    content: format!("./output"), 
                });

                let mut guard = status.write().await;
                *guard = "completed".to_string();
            }
            Err(e) => {
                let _ = tx.send(WsMessage {
                    task_id: task_id.to_string(),
                    msg_type: "log".to_string(),
                    content: format!("✗ 任务失败: {}", e),
                });
                let _ = tx.send(WsMessage {
                    task_id: task_id.to_string(),
                    msg_type: "status".to_string(),
                    content: "error".to_string(),
                });
                let mut guard = status.write().await;
                *guard = "error".to_string();
            }
        }
    }
}

async fn stop_task_handler(
    State(_state): State<AppState>,
    Path(_task_id): Path<String>,
) -> impl IntoResponse {
    // TODO: 实现任务停止逻辑
    Json(serde_json::json!({"status": "stopped"}))
}

async fn resume_task_handler(
    State(_state): State<AppState>,
    Path(_task_id): Path<String>,
) -> impl IntoResponse {
    // TODO: 实现任务恢复逻辑
    Json(serde_json::json!({"status": "resumed"}))
}

async fn delete_task_handler(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> impl IntoResponse {
    println!("删除任务: {}", task_id);
    
    // 从任务列表中移除
    state.tasks.write().await.remove(&task_id);
    
    // 尝试删除临时文件夹
    let tmp_dir = format!("tmp/{}", task_id);
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    
    println!("任务 {} 已删除", task_id);
    
    Json(serde_json::json!({
        "status": "deleted",
        "task_id": task_id
    }))
}

async fn open_folder_handler(
    Query(req): Query<OpenFolderRequest>,
) -> impl IntoResponse {
    println!("打开文件夹: {}", req.path);
    
    // 获取绝对路径
    let path = std::path::Path::new(&req.path);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    
    println!("绝对路径: {:?}", abs_path);
    
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg(&abs_path)
            .spawn();
    }
    
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(&abs_path)
            .spawn();
    }
    
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(&abs_path)
            .spawn();
    }
    
    (StatusCode::OK, Json(serde_json::json!({"success": true})))
}

#[derive(Serialize, Deserialize)]
struct TaskInfo {
    id: String,
    name: String,
    status: String,
    created_at: i64,
}

async fn list_tasks_handler(State(state): State<AppState>) -> impl IntoResponse {
    let task_runtimes: Vec<TaskRuntime> = state.tasks.read().await.values().cloned().collect();

    let mut tasks = Vec::new();
    for task in task_runtimes {
        let status = task.status.read().await.clone();
        tasks.push(TaskInfo {
            id: task.id.clone(),
            name: task.name.clone(),
            status,
            created_at: task.created_at,
        });
    }

    Json(serde_json::json!({ "tasks": tasks }))
}

