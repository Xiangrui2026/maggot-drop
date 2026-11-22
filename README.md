# MaggotDrop

MaggotDrop 用一个共享 KEY 认证文件上传、下载和 WebSocket 订阅。Rust 服务端内置中文管理页面。Windows 客户端在系统托盘运行，下载完成后会在桌面右下角弹出可复制文件的窗口。

## 构建和启动

使用 Rust 1.89 或更新版本。在 Windows 上还需要 Visual Studio C++ Build Tools 和 Windows SDK。

```powershell
cargo build --release --locked
$env:MAGGOTDROP_KEY = [guid]::NewGuid().ToString('N')
$env:MAGGOTDROP_KEY
.\target\release\maggotdrop-server.exe
```

打开 [管理页面](http://127.0.0.1:3000/)，输入与 `MAGGOTDROP_KEY` 相同的值。页面支持上传、下载和分页查看有效文件。服务端从上传完成起保留文件 1 小时，到期后自动删除。可在启动服务端前设置以下环境变量。

| 环境变量 | 默认值 | 用途 |
| --- | --- | --- |
| `MAGGOTDROP_BIND` | `127.0.0.1:3000` | 监听地址。需要局域网直连时可设置为 `0.0.0.0:3000`。 |
| `MAGGOTDROP_DATA_DIR` | `./data` | 保存文件和元数据的目录。重启后仍按原上传时间计算有效期。 |
| `MAGGOTDROP_MAX_UPLOAD_BYTES` | `17179869184` | 单文件最大字节数，默认 16 GiB。 |
| `MAGGOTDROP_SECURE_COOKIE` | 未设置 | 通过 HTTPS 反向代理部署时设为 `1`。 |

上传和下载均流式传输，文件读写使用 1 MiB 缓冲。上传成功后才可下载。跨不可信网络时，使用支持 WebSocket 升级和大文件上传的 HTTPS 反向代理，客户端的 `server_url` 填写对应的 HTTPS 地址。

## Docker 部署

在安装了 Docker Engine 和 Compose 插件的 Linux 主机上运行以下命令。

```sh
umask 077
cp -n .env.example .env
# 首次部署时，在 .env 中设置独立的随机 MAGGOTDROP_KEY
docker compose up -d --build
docker compose ps
```

容器以普通用户运行，上传文件保存在 Compose 的 `data` 卷中。重新构建或重启容器时会保留有效期内的文件。Compose 默认只映射服务器的 `127.0.0.1:3000`，可用 `MAGGOTDROP_HOST_PORT` 修改主机端口。

使用 HTTPS 反向代理时，让代理转发到主机映射端口，保留 WebSocket 升级，并允许流式传输所需大小的请求。将 `.env` 中的 `MAGGOTDROP_SECURE_COOKIE` 改为 `1`，再用 `docker compose up -d` 应用配置。

生产地址为 [maggot.shitpool.click](https://maggot.shitpool.click)。现有 Caddy 通过专用 Docker 网络转发到 MaggotDrop。生产主机上的 `maggotdrop-ingress.service` 会在 Caddy 容器重新创建时重新接入网络和注册域名。

## CI/CD

GitHub Actions 会检查发往 `main` 的拉取请求及推送。Linux 作业检查格式、lint 和测试，Windows 作业构建客户端并上传 `maggotdrop-windows` 构建产物，Docker 作业验证容器启动、KEY 认证、上传和下载。

`main` 的提交通过检查后，部署作业会使用 GitHub 的 `production` 环境连接生产服务器。该环境使用 `DEPLOY_HOST`、`DEPLOY_KNOWN_HOSTS` 和 `DEPLOY_SSH_KEY`，SSH 公钥在服务器上只能触发 [部署脚本](deploy/deploy-on-host.sh)。脚本构建已检查的提交，再切换容器并等待健康检查。切换失败时会恢复上一版本的镜像和代码，上传文件保存在原数据卷中。也可从 GitHub Actions 页面手动运行 `main` 的流水线。

## Windows 客户端

将 [client.example.json](client/client.example.json) 复制到客户端 EXE 所在目录并命名为 `client.json`，填写服务端地址和相同的 KEY。

```json
{
  "server_url": "http://127.0.0.1:3000",
  "key": "填入实际的 MAGGOTDROP_KEY"
}
```

双击 `target\release\maggotdrop-client.exe`。默认下载位置为 `%LOCALAPPDATA%\MaggotDrop\Downloads`，可用配置中的 `download_dir` 修改。相对路径从配置文件所在目录解析。也可以将配置文件路径作为程序的第一个参数。

客户端只从启动连接后的新上传开始接收。运行期间断线后会自动重连并补收事件。下载完成后，点击右下角弹窗的「复制文件」，再在资源管理器中按 Ctrl+V 粘贴。多个文件会依次显示，当前弹窗会保留到复制成功或手动关闭。也可右键托盘图标，选择「复制最近接收的文件」。下载和连接错误记录在下载目录的 `client.log`。

## API

API 和 WebSocket 使用 `Authorization: Bearer <KEY>`。KEY 使用无空格的可见 ASCII 字符。

| 方法 | 路径 | 行为 |
| --- | --- | --- |
| POST | `/api/files` | 接收 `multipart/form-data`，只允许一个名为 `file` 的文件字段。返回 `201` 和文件元数据。 |
| GET | `/api/files` | 由新到旧列出文件，每页最多 100 条。将返回的 `next_before` 作为下一页的 `?before=`。 |
| GET | `/api/files/{id}` | 下载指定文件。 |
| WebSocket | `/api/events` | 首条消息 `{"type":"ready","after":N}` 给出起始序号。此后收到 `{"type":"file_uploaded","file":{...}}`。 |

```powershell
curl.exe -H "Authorization: Bearer $env:MAGGOTDROP_KEY" -F "file=@example.zip" http://127.0.0.1:3000/api/files
```

文件元数据含 `id`、`sequence`、`name`、`size`、`uploaded_at` 和 `download_url`。下载链接是相对于同一服务根地址的路径。WebSocket 重连时可附加 `?after=<已处理的 sequence>`，服务端从该序号之后按顺序补发。
