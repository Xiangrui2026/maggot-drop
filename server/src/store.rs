use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::RwLock,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, TimeDelta, Utc};
use maggotdrop_protocol::{UploadedFile, download_path};
use tempfile::{Builder, TempDir};
use tokio::sync::watch;
use uuid::Uuid;

const METADATA: &str = "metadata.json";
const RETENTION: TimeDelta = TimeDelta::hours(1);
pub const PAYLOAD: &str = "payload";

struct Entry {
    file: UploadedFile,
    expires_at: DateTime<Utc>,
}

#[derive(Default)]
struct Catalog {
    ordered: BTreeMap<u64, Entry>,
    ids: HashMap<Uuid, u64>,
    latest: u64,
}

impl Catalog {
    fn insert(&mut self, file: UploadedFile, expires_at: DateTime<Utc>) {
        self.latest = self.latest.max(file.sequence);
        self.ids.insert(file.id, file.sequence);
        self.ordered
            .insert(file.sequence, Entry { file, expires_at });
    }

    fn remove(&mut self, id: Uuid) {
        if let Some(sequence) = self.ids.remove(&id) {
            self.ordered.remove(&sequence);
        }
    }
}

pub struct Store {
    files: PathBuf,
    staging: PathBuf,
    expired: PathBuf,
    sequence_file: PathBuf,
    catalog: RwLock<Catalog>,
    updates: watch::Sender<u64>,
    _lock: File,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".lock"))?;
        lock.try_lock().context("cannot lock the data directory")?;
        let files = root.join("files");
        let staging = root.join("staging");
        let expired = root.join("expired");
        let sequence_file = root.join(".sequence");
        fs::create_dir_all(&files)?;
        fs::create_dir_all(&staging)?;
        fs::create_dir_all(&expired)?;
        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() && name.starts_with("upload-") {
                fs::remove_dir_all(entry.path())?;
            } else if kind.is_file() && name.starts_with("sequence-") {
                fs::remove_file(entry.path())?;
            }
        }

        let recorded = match fs::read_to_string(&sequence_file) {
            Ok(value) => value.trim().parse().context("invalid upload sequence")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        let mut catalog = Catalog {
            latest: recorded,
            ..Default::default()
        };
        for entry in fs::read_dir(&files)? {
            let entry = entry?;
            let Ok(id) = entry.file_name().to_string_lossy().parse::<Uuid>() else {
                continue;
            };
            let path = entry.path();
            let file: UploadedFile =
                serde_json::from_reader(BufReader::new(File::open(path.join(METADATA))?))
                    .with_context(|| format!("cannot read metadata at {}", path.display()))?;
            ensure!(
                file.id == id
                    && file.sequence > 0
                    && file.download_url == download_path(id)
                    && !catalog.ordered.contains_key(&file.sequence)
                    && !catalog.ids.contains_key(&id)
                    && fs::metadata(path.join(PAYLOAD))?.len() == file.size,
                "invalid stored file at {}",
                path.display()
            );
            let expires_at = DateTime::parse_from_rfc3339(&file.uploaded_at)
                .with_context(|| format!("invalid upload timestamp at {}", path.display()))?
                .with_timezone(&Utc)
                .checked_add_signed(RETENTION)
                .context("upload expiry is out of range")?;
            catalog.insert(file, expires_at);
        }
        let latest = catalog.latest;
        let (updates, _) = watch::channel(latest);
        let store = Self {
            files,
            staging,
            expired,
            sequence_file,
            catalog: RwLock::new(catalog),
            updates,
            _lock: lock,
        };
        if latest > recorded {
            store.persist_sequence(latest)?;
        }
        Ok(store)
    }

    pub fn stage(&self) -> Result<TempDir> {
        Ok(Builder::new().prefix("upload-").tempdir_in(&self.staging)?)
    }

    pub fn commit(&self, stage: TempDir, name: String, size: u64) -> Result<UploadedFile> {
        let mut catalog = self.catalog.write().unwrap();
        let sequence = catalog
            .latest
            .checked_add(1)
            .context("upload sequence exhausted")?;
        let id = Uuid::new_v4();
        let uploaded_at = Utc::now();
        let file = UploadedFile {
            id,
            sequence,
            name,
            size,
            uploaded_at: uploaded_at.to_rfc3339(),
            download_url: download_path(id),
        };
        let mut metadata = File::create(stage.path().join(METADATA))?;
        let serialized = serde_json::to_vec(&file)?;
        metadata.write_all(&serialized)?;
        metadata.sync_all()?;
        drop(metadata);
        // Reserve the sequence before publishing. Expiring every file must never rewind the cursor.
        self.persist_sequence(sequence)?;
        catalog.latest = sequence;
        fs::rename(stage.path(), self.files.join(id.to_string()))?;
        catalog.insert(file.clone(), uploaded_at + RETENTION);
        self.updates.send_replace(sequence);
        Ok(file)
    }

    fn persist_sequence(&self, sequence: u64) -> Result<()> {
        let mut temporary = Builder::new()
            .prefix("sequence-")
            .tempfile_in(&self.staging)?;
        writeln!(temporary, "{sequence}")?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.sequence_file)?;
        Ok(())
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.updates.subscribe()
    }

    pub async fn expire_files(&self) {
        let mut uploads = self.subscribe();
        loop {
            drop(uploads.borrow_and_update());
            let next = self.expire_due().await;
            tokio::select! {
                _ = tokio::time::sleep(next) => {}
                _ = uploads.changed() => {}
            }
        }
    }

    async fn expire_due(&self) -> Duration {
        let now = Utc::now();
        let mut due = Vec::new();
        let mut next = now + TimeDelta::minutes(1);
        for entry in self.catalog.read().unwrap().ordered.values() {
            if entry.expires_at <= now {
                due.push(entry.file.id);
            } else {
                next = next.min(entry.expires_at);
            }
        }
        let mut retry = false;
        for id in due {
            let name = id.to_string();
            // Move first so an interrupted recursive delete cannot leave a broken upload on disk.
            match tokio::fs::rename(self.files.join(&name), self.expired.join(&name)).await {
                Ok(()) => self.catalog.write().unwrap().remove(id),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.catalog.write().unwrap().remove(id);
                }
                Err(error) => {
                    eprintln!("Could not expire upload {id}: {error}");
                    retry = true;
                }
            }
        }
        if let Err(error) = self.delete_expired().await {
            eprintln!("Could not clean up expired uploads: {error:#}");
            retry = true;
        }
        let delay = (next - Utc::now()).to_std().unwrap_or_default();
        if retry {
            delay.min(Duration::from_secs(5))
        } else {
            delay
        }
    }

    async fn delete_expired(&self) -> Result<()> {
        let mut entries = tokio::fs::read_dir(&self.expired).await?;
        let mut result = Ok(());
        while let Some(entry) = entries.next_entry().await? {
            if let Err(error) = tokio::fs::remove_dir_all(entry.path()).await {
                result = Err(error).with_context(|| format!("deleting {}", entry.path().display()));
            }
        }
        result
    }

    pub fn list(&self, before: Option<u64>, count: usize) -> Vec<UploadedFile> {
        use std::ops::Bound::{Excluded, Unbounded};
        let now = Utc::now();
        self.catalog
            .read()
            .unwrap()
            .ordered
            .range((Unbounded, before.map_or(Unbounded, Excluded)))
            .rev()
            .filter(|(_, entry)| entry.expires_at > now)
            .take(count)
            .map(|(_, entry)| entry.file.clone())
            .collect()
    }

    pub fn after(&self, sequence: u64, count: usize) -> Vec<UploadedFile> {
        use std::ops::Bound::{Excluded, Unbounded};
        let now = Utc::now();
        self.catalog
            .read()
            .unwrap()
            .ordered
            .range((Excluded(sequence), Unbounded))
            .filter(|(_, entry)| entry.expires_at > now)
            .take(count)
            .map(|(_, entry)| entry.file.clone())
            .collect()
    }

    pub fn get(&self, id: Uuid) -> Option<(UploadedFile, PathBuf)> {
        let catalog = self.catalog.read().unwrap();
        let entry = catalog.ordered.get(catalog.ids.get(&id)?)?;
        (entry.expires_at > Utc::now()).then(|| {
            (
                entry.file.clone(),
                self.files.join(id.to_string()).join(PAYLOAD),
            )
        })
    }
}
