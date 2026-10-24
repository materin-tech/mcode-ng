# MCode

Rust 实现的 AI 编程代理：**mcode**（终端 CLI）+ **MCode Desktop**（桌面端），共享同一内核 [`mcode-core`](crates/mcode-core)。代理式循环执行工具、读写代码、跑命令、管理会话——支持多家模型供应商，原生跑在 macOS / Linux / Windows。

## 功能

### 代理内核
- **工具循环**：bash、文件读写编辑（写前指纹校验 + 检查点快照）、todo 管理、Web 搜索抓取；只读工具并行执行
- **子代理**：`task` 派发后台/前台子任务，递归护栏防套娃；后台任务状态实时轮询，`task_output` / `task_stop` 管理
- **goal 模式**：`/goal <目标>` 持续推进直到独立判官宣判完成（上限 12 轮，判官 fail-open，可随时打断）
- **上下文管理**：自动压缩（80% 上下文门控 + LLM 摘要）、上下文窗口表按模型区分（GLM/DeepSeek 等长窗口模型识别 1M 后缀）
- **权限系统**：manual / auto / plan / passon 四模式（shift+tab 切换），规则引擎支持命令前缀与 glob 匹配，危险操作弹窗确认

### 扩展机制
- **MCP**：stdio 与 streamable-http 双传输，`mcp__server__tool` 挂载，单 server 故障不拖垮整体
- **Hooks**：8 生命周期事件（PreToolUse/PostToolUse/Stop 等），exit 2 阻断、stdout JSON 改写参数/决策
- **Skills / Agents / Plugins**：`SKILL.md` 目录格式技能、frontmatter 子代理定义、插件清单聚合，用户级 → 项目级 → 插件三层覆盖

### 会话与恢复
- JSONL 逐行落盘，崩溃不丢历史；`/resume` 列表恢复，`-c` 继续最近会话
- **检查点回退**：双击 Esc 或 `/rewind` 回退到任意回合之前，非只读操作写前快照自动还原文件
- 会话标题、导出 Markdown、剪贴板复制、自动保留策略（按项目 50 个上限 + 天数淘汰）

### 交互体验
- **@ 文件补全**：层级浏览式补全，目录下钻、文件插入内容
- **斜杠指令**：29 条指令全量补全（用法+作用同源展示），未知 `/xxx` 自动按 skill 调用
- **打回性能**：流式输出、思考过程可视化、运行态提示（耗时/token 增量）、队列续发、可见光标
- **多协议**：Anthropic Messages / OpenAI Responses / OpenAI Chat 三协议后端，兼容层容错（分片缺失回退 start.input）

### 桌面端（MCode Desktop）
GPUI 原生应用：会话视图、设置（供应商/模型/权限/语言）、扩展管理（MCP/Hooks/技能/子代理）、会话列表；复用 CLI 同一内核与指令系统。

## 安装

### 下载预编译包

从 [Releases](https://github.com/materin-tech/mcode-ng/releases) 下载对应平台包，解压后把二进制放进 PATH 即可：

| 平台 | 包 |
|---|---|
| macOS Apple Silicon | `mcode-v*-*-macos-arm64.tar.gz` |
| Linux x86_64（musl 静态） | `mcode-v*-*-linux-x86_64.tar.gz` |
| Linux arm64（musl 静态） | `mcode-v*-*-linux-arm64.tar.gz` |
| Windows x86_64 | `mcode-v*-*-windows-x86_64.zip` |
| macOS 桌面端 | `MCode_Desktop-v*-*-macos-arm64.zip`（解压得 `MCode_Desktop.app`，拖进 /Applications） |

```bash
# 示例：macOS
tar xzf mcode-v*-macos-arm64.tar.gz
sudo mv mcode /usr/local/bin/
mcode --version
```

### 从源码构建

需要 Rust 1.85+（stable）：

```bash
git clone https://github.com/materin-tech/mcode-ng.git
cd mcode-ng
cargo build --release -p mcode-cli          # CLI → target/release/mcode
cargo build --release -p mcode-ui           # 桌面端 → target/release/MCode_Desktop（仅 macOS/Linux）
```

Linux 桌面端需要 wayland/xkb/fontconfig 开发头文件；Windows 桌面端构建未经验证。

## 配置

首次运行前配置供应商（`~/.mcode/config.json`，与 mcode CLI 互通 schema）：

```json
{
  "activeProvider": "my-provider",
  "activeModel": "glm-5.3",
  "defaultMode": "passon",
  "providers": [
    {
      "name": "my-provider",
      "baseURL": "https://api.example.com",
      "api_key_env": "MY_API_KEY",
      "models": ["glm-5.3", "deepseek-v4.1"],
      "multimodalModels": ["glm-5.3"],
      "protocol": "anthropic"
    }
  ]
}
```

- `protocol`：`anthropic` / `responses` / `chat`（旧配置缺省 → `responses`）
- `api_key_env`：从环境变量读 key（推荐）；值若不是合法变量名则按明文兜底
- MCP servers 与 hooks 配置在 `~/.mcode/desktop.json` 的 `mcpServers` / `hooks` 字段

然后进入项目目录运行：

```bash
cd your-project
mcode                    # 交互式 TUI
mcode -p "修复这个 bug"   # 一次性执行（print 模式）
mcode -c                 # 继续本项目最近会话
mcode -r                 # 恢复历史会话（列表选择）
mcode --model glm-5.3    # 指定模型
mcode --fallback-model m2,m3   # 模型失败回退链
```

## 指令速查

<details>
<summary>对话与会话</summary>

| 指令 | 作用 |
|---|---|
| `/help [command]` | 显示全部指令或单个指令的用法与作用 |
| `/clear` | 清空当前会话历史，开始新对话（会话文件保留） |
| `/compact [instructions]` | 压缩历史为摘要腾出上下文，可选指示摘要侧重点 |
| `/copy [last\|all]` | 复制最后一条回复（默认）或全部对话到剪贴板 |
| `/export [file]` | 导出会话为 Markdown 文件，缺省写到 mcode-export.md |
| `/goal <objective>` | 持续目标模式：循环执行直到判官宣判完成（上限 12 轮） |
| `/rename <title>` | 重命名当前会话 |
| `/resume [id]` | 恢复历史会话；无参数 = 交互式选择最近会话 |
| `/rewind` | 回退会话到上一个检查点（双击 Esc 同效） |
| `/save` | 立即落盘当前会话 |
| `/quit` | 退出 mcode（会话已自动落盘） |
</details>

<details>
<summary>上下文与用量</summary>

| 指令 | 作用 |
|---|---|
| `/context` | 显示上下文窗口用量分解 |
| `/cost` | 当前会话累计成本估算 |
| `/diff` | 显示工作区 git diff --stat 变更概要 |
| `/files` | 列出本会话触碰过的文件 |
| `/token` | 当前会话 token 用量 |
</details>

<details>
<summary>配置、扩展与诊断</summary>

| 指令 | 作用 |
|---|---|
| `/effort [low\|medium\|high]` | 查看或设置推理努力级别 |
| `/memory` | 用 $EDITOR 打开 MCODE.md 记忆文件 |
| `/mcp` | 列出 MCP servers 与连接状态 |
| `/mode [manual\|auto\|plan\|passon]` | 查看或切换权限模式（缺省 = 循环切换） |
| `/model [name]` | 查看或切换模型 |
| `/provider [name]` | 查看或切换 API provider |
| `/agents` | 列出可用 subagents |
| `/hooks` | 列出已配置的 hooks |
| `/init` | 让模型分析代码库并生成 MCODE.md 项目说明 |
| `/skills` | 列出可用 skills（`/<skill名>` 直接调用） |
| `/doctor` | 体检：配置/网络/权限/依赖自检并给出修复建议 |
| `/status` | 显示会话/模型/provider/权限模式等当前状态 |
</details>

**快捷键**：`shift+tab` 切权限模式 · `@` 文件补全 · `/` 指令补全 · `esc` 打断 / 双击回退 · `ctrl+o` 展开详情 · `!cmd` 跑 shell · `↑↓` 历史 · `ctrl+c` 退出

## 目录结构

```
~/.mcode/
├── config.json          # 全局配置（供应商/模型/权限/语言）
├── desktop.json         # 扩展配置（MCP/hooks/thinking/权限规则）
├── assets/              # 内置资产物化（技能/子代理）
├── agents/              # 用户级子代理定义
├── skills/              # 用户级技能
└── projects/<项目>/     # 会话 JSONL + 标题 + 检查点快照

<项目>/
├── MCODE.md             # 项目说明与记忆（/init 生成）
└── .mcode/
    ├── agents/          # 项目级子代理
    └── skills/          # 项目级技能
```

## License

[MIT](LICENSE)
