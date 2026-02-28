# 语音转文字（Groq Whisper）

AdaClaw 支持通过 **Groq Whisper API** 自动转录 Telegram 语音/音频消息，极低配置成本，极快转录速度。

## 效果

发送语音消息 → Groq Whisper 自动转录 → 以文本形式交给 Agent 处理，附注 `[语音转文字]` 标记。

## 快速开始

### 1. 获取 Groq API Key

访问 [console.groq.com](https://console.groq.com) 注册并获取 API Key（免费计划即可）。

### 2. 配置

```toml
# config.toml

[providers.groq]
api_key = "gsk_..."   # 或通过 GROQ_API_KEY 环境变量

[channels.telegram]
kind  = "telegram"
token = "..."

[channels.telegram.extra]
# 启用 Groq Whisper 语音转录
groq_transcription = "true"
# 可选：指定语言（zh=中文，en=英文，auto=自动检测）
transcription_language = "zh"
```

或通过环境变量：

```bash
export GROQ_API_KEY=gsk_...
```

### 3. 发送语音消息

在 Telegram 向 Bot 发送语音消息，Bot 会自动转录并处理。

## 支持的音频格式

Groq Whisper 支持所有主流音频格式：
- `ogg`（Telegram 语音消息默认格式）
- `mp3`, `mp4`, `mpeg`, `mpga`
- `m4a`, `wav`, `webm`

## Groq Provider 配置

Groq 同时提供 **OpenAI-compatible LLM** 接口（超快推理速度）：

```toml
[providers.groq]
api_key = "gsk_..."
default_model = "llama-3.1-70b-versatile"

[agents.fast-assistant]
provider = "groq"
model    = "llama-3.3-70b-versatile"
```

### 可用模型

| 模型 | 说明 |
|------|------|
| `llama-3.3-70b-versatile` | 通用场景，高质量 |
| `llama-3.1-8b-instant` | 极速，低延迟 |
| `mixtral-8x7b-32768` | 长上下文 |
| `gemma2-9b-it` | Google Gemma |

## 工作原理

```
用户发送 Telegram 语音消息
    ↓
TelegramChannel 检测到 voice/audio 类型
    ↓
如果配置了 Groq API：
    调用 Telegram getFile API 获取下载 URL
        ↓
    下载音频文件字节
        ↓
    POST https://api.groq.com/openai/v1/audio/transcriptions
    Content-Type: multipart/form-data
    model: whisper-large-v3
        ↓
    返回转录文本
        ↓
消息内容 = "[语音转文字] " + 转录文本
    ↓
Agent 处理（与普通文本消息相同）
```

## 转录速度

Groq 的 Whisper 推理速度**比实时快约 80 倍**，通常在 1-2 秒内完成转录。

## 隐私说明

- 音频文件在转录后**不会被存储**
- 音频内容通过 HTTPS 加密传输到 Groq API
- 请参阅 [Groq 隐私政策](https://groq.com/privacy-policy/) 了解数据处理方式
- 可通过 Groq 控制台管理数据保留设置

## 限制

- 免费计划：约 28,800 秒/天的音频转录
- 单文件最大：25 MB
- 支持语言：约 100+ 种（Whisper large-v3）

## 故障排查

**转录失败时的处理：**
- 如果 Groq API 不可用，消息内容变为 `[voice]`（原始标注），Agent 会提示用户重新发送文本
- 错误会记录在日志中：`WARN Groq transcription failed`
