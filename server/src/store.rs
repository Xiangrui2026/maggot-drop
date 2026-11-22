use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    sync::RwLock,
};

use anyhow::{Context, Result, ensure};
use maggotdrop_protocol::{UploadedFile, download_path};
use tempfile::{Builder, TempDir};
use tokio::sync::watch;
use uuid::Uuid;

const METADATA: &str = "metadata.json";
pub const PAYLOAD: &str = "payload";

#[derive(Default)]
struct Catalog {
    ordered: BTreeMap<u64, UploadedFile>,
    ids: HashMap<Uuid, u64>,
}

impl Catalog {
    fn latest(&self) -> u64 {
        self.ordered
            .last_key_value()
            .map_or(0, |(&sequence, _)| sequence)
    }

    fn insert(&mut self, file: UploadedFile) {
        self.ids.insert(file.id, file.sequence);
        self.ordered.insert(file.sequence, file);
    }
}

pub struct Store {
    files: PathBuf,
    staging: PathBuf,
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
        fs::create_dir_all(&files)?;
        fs::create_dir_all(&staging)?;
        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry.file_name().to_string_lossy().starts_with("upload-")
            {
                fs::remove_dir_all(entry.path())?;
            }
        }

        let mut catalog = Catalog::default();
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
            catalog.insert(file);
        }
        let (updates, _) = watch::channel(catalog.latest());
        Ok(Self {
            files,
            staging,
            catalog: RwLock::new(catalog),
            updates,
            _lock: lock,
        })
    }

    pub fn stage(&self) -> Result<TempDir> {
        Ok(Builder::new().prefix("upload-").tempdir_in(&self.staging)?)
    }

    pub fn commit(&self, stage: TempDir, name: String, size: u64) -> Result<UploadedFile> {
        let mut catalog = self.catalog.write().unwrap();
        let sequence = catalog
            .latest()
            .checked_add(1)
            .context("upload sequence exhausted")?;
        let id = Uuid::new_v4();
        let file = UploadedFile {
            id,
            sequence,
            name,
            size,
            uploaded_at: chrono::Utc::now().to_rfc3339(),
            download_url: download_path(id),
        };
        let mut metadata = File::create(stage.path().join(METADATA))?;
        let serialized = serde_json::to_vec(&file)?;
        metadata.write_all(&serialized)?;
        metadata.sync_all()?;
        drop(metadata);
        fs::rename(stage.path(), self.files.join(id.to_string()))?;
        catalog.insert(file.clone());
        self.updates.send_replace(sequence);
        Ok(file)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.updates.subscribe()
    }

    pub fn list(&self, before: Option<u64>, count: usize) -> Vec<UploadedFile> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.catalog
            .read()
            .unwrap()
            .ordered
            .range((Unbounded, before.map_or(Unbounded, Excluded)))
            .rev()
            .take(count)
            .map(|(_, file)| file.clone())
            .collect()
    }

    pub fn after(&self, sequence: u64, count: usize) -> Vec<UploadedFile> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.catalog
            .read()
            .unwrap()
            .ordered
            .range((Excluded(sequence), Unbounded))
            .take(count)
            .map(|(_, file)| file.clone())
            .collect()
    }

    pub fn get(&self, id: Uuid) -> Option<(UploadedFile, PathBuf)> {
        let catalog = self.catalog.read().unwrap();
        let file = catalog.ordered.get(catalog.ids.get(&id)?)?.clone();
        Some((file, self.files.join(id.to_string()).join(PAYLOAD)))
    }
}
