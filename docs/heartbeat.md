# Heartbeat 系统（主动任务）

Heartbeat 允许 AdaClaw 定期**主动执行**预设任务，无需用户主动触发。

对标 nanobot / picoclaw / zeroclaw 的同类功能，适合：
- 📰 每日早报（天气、新闻摘要）
- 📧 定期扫描未读邮件
- 📊 周期性数据监控和汇报
- 🔔 定时提醒

## 快速开始

### 1. 在配置文件中启用

```toml
# config.toml

[heartbeat]
enabled          = true
interval_minutes = 30        # 执行间隔（最小 5 分钟）
target_channel   = "telegram" # 结果发送到哪个渠道（可选）
```

### 2. 创建 HEARTBEAT.md

在工作区根目录（`workspace/`）创建 `HEARTBEAT.md`：

```markdown
# Heartbeat Tasks

- [ ] 检查今日天气并总结给我
- [ ] 扫描最近 24 小时内收到的重要邮件摘要
- [ ] 监控 GitHub 仓库的新 PR 和 Issue
```

### 3. 启动 daemon

```bash
adaclaw run
```

启动后会看到：
```
INFO Heartbeat scheduler started  interval_mins=30
```

## HEARTBEAT.md 格式

文件使用标准 Markdown 任务列表格式：

```markdown
# Heartbeat Tasks

## 日常任务
- [ ] 未完成的任务（会被执行）
- [x] 已完成的任务（会被跳过）
- [X] 大写 X 也跳过

## 监控
* [ ] 星号格式也支持
- [ ] 检查服务器状态
```

**规则：**
- `- [ ]` 或 `* [ ]` → 待执行任务
- `- [x]` 或 `- [X]` → 已完成，跳过
- 每次 Heartbeat 执行**所有未完成任务**
- 任务描述直接注入 Agent 上下文作为用户消息

## 配置说明

```toml
[heartbeat]
# 是否启用（默认 false）
enabled = true

# 执行间隔，单位分钟（最小 5 分钟，默认 30）
interval_minutes = 30

# 结果发送渠道名（可选）
# 空 = 不主动发送（任务仍会执行，结果记录在日志）
# "telegram" = 发送到 telegram 渠道的 session
target_channel = "telegram"

# 自定义 HEARTBEAT.md 路径（可选）
# 默认: workspace/HEARTBEAT.md
heartbeat_file = "/path/to/HEARTBEAT.md"
```

## 工作原理

```
每隔 interval_minutes 分钟
    ↓
读取 HEARTBEAT.md
    ↓
提取所有 "- [ ] 任务描述" 行
    ↓
对每个任务：
    创建 InboundMessage（sender_id = "heartbeat"）
        ↓
    注入 MessageBus（绕过白名单）
        ↓
    Agent 执行任务
        ↓
    结果发送到 target_channel
```

## 长任务异步执行

若 Heartbeat 任务需要较长时间（如爬取网页、分析大量数据），结合 DelegateTool：

```markdown
# Heartbeat Tasks

- [ ] 深入分析本周所有项目的代码提交，生成技术周报（可能耗时较长）
```

Agent 会自动判断是否需要 spawn 子 Agent 来处理。

## 安全说明

- HEARTBEAT.md 的符号链接会被拒绝（防止路径穿越）
- 任务内容作为普通用户消息处理，受 Estop / RateLimit 保护
- `heartbeat` sender 可配置在 Agent 白名单中

## 示例：天气早报

```markdown
# 我的日常任务

- [ ] 获取北京今日天气预报，用一句话总结并附表情符号
```

配合 Telegram 渠道，每天早上 8 点收到天气播报：

```toml
[heartbeat]
enabled          = true
interval_minutes = 480   # 8小时间隔（早上定时可配合 cron_expression，待扩展）
target_channel   = "telegram"
```

## 注意事项

1. **最小间隔 5 分钟**：为防止 API 费用失控，间隔不得小于 5 分钟
2. **所有任务串行执行**：任务按顺序依次提交，不并行
3. **daemon 重启后重新从头执行**：不记录上次执行位置
4. **任务内容不应含敏感信息**：HEARTBEAT.md 为明文文件
