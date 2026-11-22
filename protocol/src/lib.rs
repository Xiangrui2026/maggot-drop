use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const BUFFER_BYTES: usize = 1024 * 1024;
pub const FILES_PATH: &str = "/api/files";
pub const EVENTS_PATH: &str = "/api/events";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadedFile {
    pub id: Uuid,
    pub sequence: u64,
    pub name: String,
    pub size: u64,
    pub uploaded_at: String,
    pub download_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready { after: u64 },
    FileUploaded { file: UploadedFile },
}

pub fn download_path(id: Uuid) -> String {
    format!("{FILES_PATH}/{id}")
}
