use std::{
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use maggotdrop_protocol::{BUFFER_BYTES, EVENTS_PATH, Event, UploadedFile, download_path};
use reqwest::{Client, Url, header};
use serde::Deserialize;
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};

#[derive(Deserialize)]
struct ConfigFile {
    server_url: String,
    key: String,
    download_dir: Option<PathBuf>,
}

pub struct ClientConfig {
    server: Url,
    key: header::HeaderValue,
    pub download_dir: PathBuf,
    _lock: File,
}

pub struct Downloaded {
    pub path: PathBuf,
    pub name: String,
}

impl ClientConfig {
    pub fn load() -> Result<Self> {
        let config_path = if let Some(arg) = env::args_os().nth(1) {
            std::path::absolute(arg)?
        } else {
            env::current_exe()?.with_file_name("client.json")
        };
        let contents = fs::read_to_string(&config_path)
            .with_context(|| format!("无法读取配置 {}", config_path.display()))?;
        let config: ConfigFile = serde_json::from_str(contents.trim_start_matches('\u{feff}'))
            .context("client.json 格式无效")?;
        let server = Url::parse(&config.server_url).context("server_url 无效")?;
        ensure!(
            matches!(server.scheme(), "http" | "https")
                && server.host().is_some()
                && server.username().is_empty()
                && server.password().is_none()
                && server.path() == "/"
                && server.query().is_none()
                && server.fragment().is_none(),
            "server_url 应为 HTTP 或 HTTPS 服务的根地址"
        );
        ensure!(
            !config.key.is_empty() && config.key.bytes().all(|byte| byte.is_ascii_graphic()),
            "KEY 应为无空格的可见 ASCII 字符"
        );
        let mut key = header::HeaderValue::from_str(&format!("Bearer {}", config.key))?;
        key.set_sensitive(true);

        let download_dir = match config.download_dir {
            Some(dir) => config_path.parent().context("无配置目录")?.join(dir),
            None => env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .context("无法定位 LOCALAPPDATA，需设置 download_dir")?
                .join("MaggotDrop")
                .join("Downloads"),
        };
        let download_dir = std::path::absolute(download_dir)?;
        fs::create_dir_all(&download_dir).context("无法创建下载目录")?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(download_dir.join(".lock"))
            .context("无法打开下载目录锁")?;
        lock.try_lock()
            .context("此下载目录已被另一个 MaggotDrop 客户端使用")?;
        Ok(Self {
            server,
            key,
            download_dir,
            _lock: lock,
        })
    }
}

pub async fn run(
    config: ClientConfig,
    notifications: mpsc::UnboundedSender<Downloaded>,
) -> Result<()> {
    let http = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()?;
    let (jobs, queue) = mpsc::channel(32);
    let mut websocket = config.server.join(EVENTS_PATH)?;
    let scheme = if websocket.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    websocket
        .set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("无法创建 WebSocket 地址"))?;
    tokio::select! {
        () = listen(&config, websocket, jobs) => {}
        () = download_queue(&config, http, queue, notifications) => {}
    };
    Ok(())
}

async fn listen(config: &ClientConfig, base_url: Url, jobs: mpsc::Sender<UploadedFile>) {
    let mut after = None;
    let mut delay = Duration::from_secs(1);
    loop {
        match jobs.reserve().await {
            Ok(permit) => drop(permit),
            Err(_) => return,
        }
        let mut url = base_url.clone();
        if let Some(cursor) = after {
            url.query_pairs_mut()
                .append_pair("after", &u64::to_string(&cursor));
        }
        match receive(config, url, &mut after, &jobs, &mut delay).await {
            Ok(()) => continue, // The queue filled. Resume when the downloader frees a slot.
            Err(error) => log(&config.download_dir, &format!("WebSocket: {error:#}")),
        }
        sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn receive(
    config: &ClientConfig,
    url: Url,
    after: &mut Option<u64>,
    jobs: &mpsc::Sender<UploadedFile>,
    delay: &mut Duration,
) -> Result<()> {
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, config.key.clone());
    let options = WebSocketConfig::default()
        .max_message_size(Some(16 * 1024))
        .max_frame_size(Some(16 * 1024));
    let (mut socket, _) = timeout(
        Duration::from_secs(15),
        connect_async_with_config(request, Some(options), false),
    )
    .await??;
    let mut ready = false;
    while let Some(message) = timeout(Duration::from_secs(55), socket.next()).await? {
        match message? {
            Message::Text(text) => match serde_json::from_str::<Event>(&text)? {
                Event::Ready { after: cursor } if !ready => {
                    *after = Some(cursor);
                    *delay = Duration::from_secs(1);
                    ready = true;
                }
                Event::FileUploaded { file }
                    if ready && after.is_some_and(|cursor| file.sequence > cursor) =>
                {
                    ensure!(
                        file.download_url == download_path(file.id),
                        "服务器发来了无效下载链接"
                    );
                    let cursor = file.sequence;
                    if jobs.try_send(file).is_err() {
                        return Ok(());
                    }
                    *after = Some(cursor);
                }
                _ => bail!("服务器发来了无效事件顺序"),
            },
            Message::Ping(_) => timeout(Duration::from_secs(10), socket.flush()).await??,
            Message::Close(_) => break,
            _ => {}
        }
    }
    bail!("连接已关闭")
}

async fn download_queue(
    config: &ClientConfig,
    http: Client,
    mut queue: mpsc::Receiver<UploadedFile>,
    notifications: mpsc::UnboundedSender<Downloaded>,
) {
    while let Some(file) = queue.recv().await {
        for attempt in 0..4 {
            match download(config, &http, &file).await {
                Ok(path) => {
                    if notifications
                        .send(Downloaded {
                            path,
                            name: file.name,
                        })
                        .is_err()
                    {
                        return;
                    }
                    break;
                }
                Err(error) => {
                    log(
                        &config.download_dir,
                        &format!("下载 {} 失败 ({}/4): {error:#}", file.id, attempt + 1),
                    );
                    if attempt < 3 {
                        sleep(Duration::from_secs(1 << attempt)).await;
                    }
                }
            }
        }
    }
}

async fn download(config: &ClientConfig, http: &Client, file: &UploadedFile) -> Result<PathBuf> {
    let directory = config.download_dir.join(file.id.to_string());
    let destination = directory.join(windows_filename(&file.name));
    tokio::fs::create_dir_all(&directory).await?;
    match tokio::fs::metadata(&destination).await {
        Ok(meta) => {
            ensure!(
                meta.is_file() && meta.len() == file.size,
                "目标路径已有不同文件"
            );
            return Ok(destination);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let response = http
        .get(config.server.join(&file.download_url)?)
        .header(header::AUTHORIZATION, config.key.clone())
        .send()
        .await?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "HTTP {}",
        response.status()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length == file.size),
        "文件大小与通知不一致"
    );
    let stage = tempfile::Builder::new()
        .suffix(".part")
        .tempfile_in(directory)?;
    let output = tokio::fs::File::from_std(stage.reopen()?);
    let mut output = BufWriter::with_capacity(BUFFER_BYTES, output);
    let mut size = 0u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        size = size.saturating_add(chunk.len() as u64);
        ensure!(size <= file.size, "下载的数据超过通知中的大小");
        output.write_all(&chunk).await?;
    }
    ensure!(size == file.size, "下载的数据不完整");
    output.flush().await?;
    output.into_inner().sync_all().await?;
    drop(stage.persist_noclobber(&destination)?);
    Ok(destination)
}

fn windows_filename(original: &str) -> String {
    let filtered: String = original
        .chars()
        .map(|ch| {
            if ch.is_control() || "<>:\"/\\|?*".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let name = filtered.trim_end_matches([' ', '.']);
    let mut name = if name.is_empty() {
        "file".to_owned()
    } else {
        name.to_owned()
    };
    let device = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let numbered = ["COM", "LPT"].iter().any(|prefix| {
        device.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if numbered
        || matches!(
            device.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
    {
        name.insert(0, '_');
    }
    if name.encode_utf16().count() > 120 {
        let (stem, extension) = name.rsplit_once('.').unwrap_or((&name, ""));
        if !stem.is_empty() && !extension.is_empty() && extension.encode_utf16().count() <= 24 {
            name = format!(
                "{}.{}",
                utf16_prefix(stem, 119 - extension.encode_utf16().count()),
                extension
            );
        } else {
            name = utf16_prefix(&name, 120)
                .trim_end_matches([' ', '.'])
                .to_owned();
        }
    }
    if name.is_empty() { "file".into() } else { name }
}

fn utf16_prefix(value: &str, mut remaining: usize) -> &str {
    for (index, ch) in value.char_indices() {
        match remaining.checked_sub(ch.len_utf16()) {
            Some(rest) => remaining = rest,
            None => return &value[..index],
        }
    }
    value
}

pub fn log(directory: &Path, message: &str) {
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("client.log"))
    {
        let _ = writeln!(log, "{:?} {message}", std::time::SystemTime::now());
    }
}
