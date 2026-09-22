# Zeron

默认在本地管理编码 agent（Claude Code、Codex、Cursor、Devin、Grok、Hermes、Pi、Mimir 和 Antigravity），也可启用私有多设备同步。

*[English](README.md) | 简体中文*

![Zeron 驱动 Claude Code 会话，侧边栏显示实时分支 diff](apps/landing/public/assets/app-screenshot.jpg)

每台设备都运行一个小型引擎并保留本地缓存。全新安装默认离线、纯本地，不需要账号、托管后端或云厂商凭据。

## 安装

Linux：

```bash
curl -fsSL https://github.com/wasimysaid/Kratos/releases/latest/download/install.sh | sh
zeron status
```

安装器会校验 GitHub Release 清单和 SHA-256，并安装引擎及由应用管理的 Tailcat 适配器；系统支持 systemd 时会启动用户服务。

日常命令：

```bash
zeron status
zeron update
zeron daemon start|stop|restart|status
```

macOS 请从 [GitHub 最新发行版](https://github.com/wasimysaid/Kratos/releases/latest)下载 DMG。Windows 请下载便携 ZIP，把 `zeron-update.json` 和 `zeron.exe` 放在一起；源码构建说明见 [Windows 开发文档](docs/reference/windows-development.md)。

## 可选：多设备同步

Tailcat 负责私有连接，Zeron 的 durable peer 负责认证、同步和持久存储。若设备不会同时在线，请在 VPS 等常开机器上初始化 peer：

```bash
zeron daemon stop
zeron peer init --name home-peer
zeron daemon start
zeron peer invite --output invite.txt
```

请通过私密渠道传递 `invite.txt`。在另一台已停止引擎的设备上运行：

```bash
zeron daemon stop
zeron pair --code-file invite.txt
zeron daemon start
```

邀请短时有效且只能使用一次。配对使用持久设备密钥和持钥证明；可用 `zeron peer devices` 查看设备，用 `zeron peer revoke <device-id>` 撤销设备。请把配对码和 Tailcat 地址当作秘密保存。

发行包已包含适配器。源码构建可运行 `scripts/build-tailcat.sh native`，再用 `ZERON_TAILCAT_ADAPTER` 指向生成的程序。公共 DERP 有速率限制且没有 SLA；需要可靠中继的部署应在 `peer init` 时通过 `--derp-map` 指定自有 HTTPS DERP map。

已配对设备可访问远程允许的工作区能力，包括列出、读取和写入所属设备上的文件。显示 ignored 文件时，`.env` 等 gitignored 文件也可能远程可见；`.git` 始终排除。

同步从全新 profile 开始：创建 peer 会创建新的同步 profile，加入已有 peer 只加载该 peer 的数据。不提供本地会话导入或旧云端账号迁移；现有本地和历史文件保留不动。桌面端可在应用内切换引擎，headless 用户需重启引擎。恢复纯本地模式：

```bash
zeron daemon stop
zeron logout
zeron daemon start
```

运行时和存储模型见 [ARCHITECTURE.md](ARCHITECTURE.md)；新 peer 的备份与恢复见 [docs/peer-backup.md](docs/peer-backup.md)。

采用 [MIT License](LICENSE)。
