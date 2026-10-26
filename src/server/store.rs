use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::common::json_file::{append_jsonl, load_json, save_json_atomic};
use crate::common::AppResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientStatus {
    Pending,
    Approved,
    Rejected,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRecord {
    pub id: String,
    pub fingerprint: String,
    pub hostname: String,
    pub ip: String,
    pub os: String,
    pub user: String,
    pub status: ClientStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default)]
    pub whitelist: Vec<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

/// 展示用：隐藏 token
#[derive(Debug, Clone, Serialize)]
pub struct ClientSummary {
    pub id: String,
    pub fingerprint: String,
    pub hostname: String,
    pub ip: String,
    pub os: String,
    pub user: String,
    pub status: ClientStatus,
    pub has_token: bool,
    pub whitelist: Vec<String>,
    pub created_at: String,
    pub approved_at: Option<String>,
    pub last_seen: Option<String>,
}

impl From<&ClientRecord> for ClientSummary {
    fn from(r: &ClientRecord) -> Self {
        ClientSummary {
            id: r.id.clone(),
            fingerprint: r.fingerprint.clone(),
            hostname: r.hostname.clone(),
            ip: r.ip.clone(),
            os: r.os.clone(),
            user: r.user.clone(),
            status: r.status,
            has_token: r.token.is_some(),
            whitelist: r.whitelist.clone(),
            created_at: r.created_at.clone(),
            approved_at: r.approved_at.clone(),
            last_seen: r.last_seen.clone(),
        }
    }
}

pub struct Store {
    dir: PathBuf,
    clients: Mutex<Vec<ClientRecord>>,
}

impl Store {
    pub fn load(dir: &Path) -> AppResult<Self> {
        let clients_file = dir.join("clients.json");
        let clients: Vec<ClientRecord> = load_json(&clients_file)?.unwrap_or_default();
        Ok(Store {
            dir: dir.to_path_buf(),
            clients: Mutex::new(clients),
        })
    }

    fn clients_file(&self) -> PathBuf {
        self.dir.join("clients.json")
    }

    pub fn save(&self) -> AppResult<()> {
        // 先克隆释放锁，再做同步文件 IO（避免持锁期间阻塞其他 store 操作与 tokio worker）
        let snapshot = self.clients.lock().unwrap().clone();
        save_json_atomic(&self.clients_file(), &snapshot)
    }

    /// 按指纹查找注册记录（同一指纹仅一条）
    pub fn find_by_fingerprint(&self, fp: &str) -> Option<ClientRecord> {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.fingerprint == fp)
            .cloned()
    }

    pub fn find_by_id(&self, id: &str) -> Option<ClientRecord> {
        self.clients.lock().unwrap().iter().find(|c| c.id == id).cloned()
    }

    /// 按凭证 token 查找已批准的客户端
    pub fn find_by_token(&self, token: &str) -> Option<ClientRecord> {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.status == ClientStatus::Approved && c.token.as_deref() == Some(token))
            .cloned()
    }

    pub fn insert(&self, record: ClientRecord) -> AppResult<()> {
        self.clients.lock().unwrap().push(record);
        self.save()
    }

    pub fn update<F: FnOnce(&mut ClientRecord)>(&self, id: &str, f: F) -> AppResult<Option<ClientRecord>> {
        let mut clients = self.clients.lock().unwrap();
        let Some(record) = clients.iter_mut().find(|c| c.id == id) else {
            return Ok(None);
        };
        f(record);
        let snapshot = record.clone();
        drop(clients);
        self.save()?;
        Ok(Some(snapshot))
    }

    pub fn remove(&self, id: &str) -> AppResult<bool> {
        let mut clients = self.clients.lock().unwrap();
        let before = clients.len();
        clients.retain(|c| c.id != id);
        let removed = clients.len() != before;
        drop(clients);
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn list(&self) -> Vec<ClientRecord> {
        self.clients.lock().unwrap().clone()
    }
}

// ---- 审计 ----

#[derive(Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts: String,
    pub event: String,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExecHistory {
    pub ts: String,
    pub client_id: String,
    pub hostname: String,
    pub program: String,
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timeout: bool,
    pub truncated: bool,
}

pub struct Audit {
    dir: PathBuf,
}

impl Audit {
    pub fn new(dir: &Path) -> Self {
        Audit { dir: dir.to_path_buf() }
    }

    pub fn log(&self, event: &str, actor: &str, detail: Option<serde_json::Value>) {
        let ev = AuditEvent {
            ts: Utc::now().to_rfc3339(),
            event: event.into(),
            actor: actor.into(),
            detail,
        };
        if let Err(err) = append_jsonl(&self.dir.join("audit.jsonl"), &ev) {
            macro_log::w!("审计写入失败: {err}");
        }
    }

    pub fn query(&self, limit: usize) -> Vec<AuditEvent> {
        query_jsonl_tail(&self.dir.join("audit.jsonl"), limit)
    }

    pub fn log_exec(&self, h: &ExecHistory) {
        if let Err(err) = append_jsonl(&self.dir.join("history.jsonl"), h) {
            macro_log::w!("执行历史写入失败: {err}");
        }
    }

    pub fn query_execs(&self, limit: usize) -> Vec<ExecHistory> {
        query_jsonl_tail(&self.dir.join("history.jsonl"), limit)
    }
}

fn query_jsonl_tail<T: serde::de::DeserializeOwned>(path: &Path, limit: usize) -> Vec<T> {
    // 只读文件尾部（每行按 ~2KB 估算，上限 2MB），避免日志增长后全量加载
    let limit = limit.min(1000);
    let Ok(meta) = std::fs::metadata(path) else {
        return vec![];
    };
    let read_len = meta.len().min((limit as u64 * 2048).min(2 * 1024 * 1024)) as usize;
    let Ok(file) = std::fs::File::open(path) else {
        return vec![];
    };
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::io::BufReader::new(file);
    if file.seek(SeekFrom::End(-(read_len as i64))).is_err() {
        return vec![];
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return vec![];
    }
    // 尾部读取可能从某行中间开始：丢弃第一个不完整行
    let lines: Vec<&str> = buf.lines().skip(1).collect();
    lines
        .into_iter()
        .rev()
        .filter_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| serde_json::from_str(line).ok()).flatten()
        })
        .take(limit)
        .collect()
}
