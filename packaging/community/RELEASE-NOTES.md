# VocalCode Community

Free, local-first dictation and meeting notes under AGPL-3.0-only. No purchase,
account or activation code is required. Recognition runs on your CPU after
the selected model downloads.

## New in 1.4.0

- **Writing page.** Opt-in voice commands for complete dictations: "new
  line" / "new paragraph" (换行 / 另起一段), "scratch that" (删掉上一句),
  spoken lists ("first… second…", 第一，第二…), coding words ("camel case user
  id" → `userId`, "open paren"), and ending with "press enter" (回车) to send.
  Formal / Casual / Very casual style, per program, with a chat-app preset and
  a Try-it box that runs the same rules as dictation. Everything is off until
  you turn it on; History keeps the original recognition.
- **Double-tap to lock.** Hold to talk as before, or double-tap the talk key to
  keep listening hands-free; press once more to finish.
- **Mute other audio while dictating** (Windows, opt-in), restoring exactly what
  it muted; skipped during meeting capture.
- **Home insights:** day streak, words per minute, words this week and a
  12-week heatmap, from counts only.
- **Rewrite scratchpad:** translate to English or Chinese, or give your own
  instruction (you can dictate it). You still review every candidate.
- **Desktop control capsule** (Windows, opt-in) for starting, stopping and
  cancelling dictation with the mouse.
- **Fixes:** multi-line text and snippets are pasted rather than typed, so a
  line break no longer presses Enter and sends half a chat message; a
  Qwen3-ASR result containing a line break or a leaked
  "language English<asr_text>" header no longer drops or garbles the
  dictation; Spanish, French and German cover every string.
- **Tested with speech:** a new voice-corpus release test replays 1,072
  synthetic clips (generic neural voices, accents, noise, speed and low
  volume) through the production pipeline on four speech-model routes before
  release. Spoken coding words ("snake case", "open paren") are recognized
  best by the Qwen3-ASR and Parakeet English models; the default SenseVoice
  model often mishears them.

## Downloads

- Windows x64: `VocalCodeCommunitySetup.exe` (publisher: Daming Wu).
- Apple-silicon macOS: `VocalCodeCommunity-<version>.dmg` (signed and notarized).
- Exact Corresponding Source: the attached versioned source archive.
- `SHA256SUMS`: hashes of release assets; `latest.json`: the app's update feed.

Community installs separately from the previous paid edition, with its own
data folder, startup entry and update channel. Existing data is neither moved
nor deleted. Close the other edition before dictating to avoid two global
hotkey listeners. Dictionary/snippets can be exported and imported explicitly.

The first community installer has automated installation, upgrade, signature,
notarization, native-linkage and data-preservation checks. Actual microphone,
system-audio, meeting and accessibility behavior still depends on the device
and operating-system permissions; these checks do not promise perfect speech
recognition or replace broad native-device testing.

Community updates use GitHub Releases and still verify exact sizes/hashes and
the expected platform publisher and product identity before installation.
The previous paid updater does not silently migrate users to this edition.

## 中文

1.4.0 新增：「写作」页（可选的语音换行、删掉上一句、口述列表、代码词、结尾说「回车」发送，
正式／随意／很随意三档风格可按程序设置，并带「试一试」）；双击说话键锁定免提；听写时静音
其他声音（Windows，可选）；首页统计（连续天数、每分钟字数、12 周热力图，只记数量）；
改写草稿支持翻译和自定义指令；桌面控制胶囊（Windows，可选）。多行文字和片段改为粘贴，
不再把换行当回车发出半句话；Qwen3-ASR 输出里带换行或漏出提示头时，不再丢掉整句或打出乱码。
发布前用 1072 段合成语音在四条识别路线上做了完整回放测试。

社区版免费使用全部本地功能，无需激活。Windows 与 Apple 芯片 Mac 安装包
均经过签名；Mac 包完成 Apple 公证。社区版独立安装，不覆盖旧版或迁移／删除旧数据。
开始听写前请退出另一个版本，避免快捷键监听冲突。词典与片段可由用户主动导入。
首次下载模型和检查更新需要联网；录音与识别在本地处理。

Maintainer: [Daming Wu](https://github.com/wudaming00).
Issues: https://github.com/wudaming00/vocalcode-community/issues
