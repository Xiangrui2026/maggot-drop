"use strict";

const id = (name) => document.getElementById(name);
const webRequest = { "X-MaggotDrop-Request": "1" };
const loginPanel = id("login-panel");
const workspace = id("workspace");
const loginForm = id("login-form");
const uploadForm = id("upload-form");
const logout = id("logout");
const refresh = id("refresh");
const more = id("more");
const list = id("files");
const progress = id("progress");
const dates = new Intl.DateTimeFormat("zh-CN", { dateStyle: "medium", timeStyle: "short" });
let before = null;
let uploading = false;
let loadingFiles = false;

function showLogin(message = "") {
  workspace.hidden = true;
  logout.hidden = true;
  loginPanel.hidden = false;
  id("initial-status").hidden = true;
  id("login-status").textContent = message;
  id("upload-status").textContent = "";
  id("key").focus();
  list.replaceChildren();
}

function showWorkspace() {
  workspace.hidden = false;
  logout.hidden = false;
  loginPanel.hidden = true;
  id("initial-status").hidden = true;
}

loginForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const field = id("key");
  const submit = loginForm.querySelector("button");
  submit.disabled = true;
  id("login-status").textContent = "";
  try {
    const response = await fetch("/api/session", {
      method: "POST",
      headers: { Authorization: "Bearer " + field.value }
    });
    if (response.status === 401) {
      id("login-status").textContent = "KEY 无效。";
      return;
    }
    if (!response.ok) throw new Error();
    field.value = "";
    await loadFiles();
  } catch {
    id("login-status").textContent = "登录失败，请检查连接后重试。";
  } finally {
    submit.disabled = false;
  }
});

logout.addEventListener("click", async () => {
  try {
    const response = await fetch("/api/session", { method: "DELETE", headers: webRequest });
    if (!response.ok) throw new Error();
    showLogin();
  } catch {
    id("list-status").textContent = "退出失败，请重试。";
  }
});

function fileSize(bytes) {
  if (bytes < 1024) return bytes + " B";
  let size = bytes;
  for (const unit of ["KiB", "MiB", "GiB", "TiB", "PiB"]) {
    size /= 1024;
    if (size < 1024 || unit === "PiB") return size.toFixed(1) + " " + unit;
  }
}

function fileRow(file) {
  const row = document.createElement("li");
  const name = document.createElement("span");
  const details = document.createElement("small");
  const link = document.createElement("a");
  name.textContent = file.name;
  details.textContent = fileSize(file.size) + " · " + dates.format(new Date(file.uploaded_at));
  link.href = file.download_url;
  link.download = file.name;
  link.textContent = "下载";
  link.setAttribute("aria-label", "下载 " + file.name);
  row.append(name, details, link);
  return row;
}

async function loadFiles(append = false) {
  if (loadingFiles) return;
  loadingFiles = true;
  refresh.disabled = true;
  more.disabled = true;
  logout.disabled = true;
  try {
    const path = append ? "/api/files?before=" + encodeURIComponent(before) : "/api/files";
    const response = await fetch(path);
    if (response.status === 401) {
      showLogin(workspace.hidden ? "" : "会话已结束，请重新输入 KEY。");
      return;
    }
    if (!response.ok) throw new Error();
    const page = await response.json();
    showWorkspace();
    if (!append) list.replaceChildren();
    list.append(...page.files.map(fileRow));
    before = page.next_before;
    more.hidden = before === null;
    id("list-status").textContent = list.childElementCount ? "" : "还没有上传文件。";
  } catch {
    if (workspace.hidden) showLogin("无法读取文件列表，请重试。");
    else id("list-status").textContent = "读取文件列表失败，请重试。";
  } finally {
    loadingFiles = false;
    refresh.disabled = false;
    more.disabled = false;
    logout.disabled = uploading;
  }
}

refresh.addEventListener("click", () => loadFiles());
more.addEventListener("click", () => loadFiles(true));
uploadForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const data = new FormData(uploadForm);
  const submit = uploadForm.querySelector("button");
  const fileInput = id("file");
  const status = id("upload-status");
  uploading = true;
  submit.disabled = true;
  fileInput.disabled = true;
  logout.disabled = true;
  progress.hidden = false;
  progress.removeAttribute("value");
  status.textContent = "正在上传…";

  const request = new XMLHttpRequest();
  request.open("POST", "/api/files");
  request.setRequestHeader("X-MaggotDrop-Request", "1");
  request.upload.addEventListener("progress", (event) => {
    if (event.lengthComputable) progress.value = event.loaded / event.total * 100;
  });
  request.addEventListener("loadend", () => {
    uploading = false;
    submit.disabled = false;
    fileInput.disabled = false;
    logout.disabled = loadingFiles;
    progress.hidden = true;
    if (request.status === 201) {
      uploadForm.reset();
      status.textContent = "已上传。";
      loadFiles();
    } else if (request.status === 401) {
      showLogin("会话已结束，请重新输入 KEY。");
    } else if (request.status === 413) {
      status.textContent = "文件超过服务器的上传大小限制。";
    } else {
      status.textContent = "上传失败，请重试。";
    }
  });
  request.send(data);
});

loadFiles();
