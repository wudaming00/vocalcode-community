# VocalCode

Free, local-first dictation and meeting notes, open source under
AGPL-3.0-only. There is nothing to buy: no purchase, account or activation
code. Recognition runs on your CPU after the selected model downloads.

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

- Windows x64: `VocalCodeSetup.exe` (publisher: Daming Wu).
- Apple-silicon macOS: `VocalCode-<version>.dmg` (signed and notarized).
- Exact Corresponding Source: `VocalCode-source-<version>.tar.gz`.
- `SHA256SUMS`: hashes of release assets; `latest.json`: the app's update feed.

## Installing over an earlier VocalCode

VocalCode is now a single free app. It installs where the paid VocalCode
releases (1.2.1 and earlier) were installed and uses the same data folder, so
it replaces such an installation in place: settings, dictionary, snippets,
meetings and downloaded models stay where they are, and nothing needs
importing. A paid release whose licence or trial is still active offers this
version as an in-app update; otherwise run the installer over it (on a Mac,
replace VocalCode.app in Applications). The paid licence and trial files are
left where they are, and VocalCode never reads them. Uninstalling VocalCode
later keeps your data folder.

A paid VocalCode installed with Scoop cannot update itself this way: run
`scoop uninstall vocalcode`, then install from this page.

The early free builds, VocalCode Community 1.3.1 and 1.4.0, were a separate
app. On Windows, the installer uninstalls VocalCode Community 1.3.1 or 1.4.0
and keeps its data folder; **Settings → System → Previous VocalCode** copies
what you choose from it. On a Mac, remove VocalCode Community 1.3.1 or 1.4.0
yourself. Those builds cannot update to this version in the app: download it
from this page once. Don't run two copies at once: each would type every
dictation.

## Checks

Before publication, every release is tested for installation, upgrade,
signatures, notarization, native linkage and data preservation, and on
Windows for an in-place update of a real paid VocalCode 1.2.1 through its own
updater. Actual microphone, system-audio, meeting and accessibility behavior
still depends on the device and operating-system permissions; these checks do
not promise perfect speech recognition or replace broad native-device testing.

VocalCode updates from this project's GitHub Releases and verifies the exact
size, SHA-256, publisher signature and product identity before installing.

## 中文

1.4.0 新增：「写作」页（可选的语音换行、删掉上一句、口述列表、代码词、结尾说「回车」发送，
正式／随意／很随意三档风格可按程序设置，并带「试一试」）；双击说话键锁定免提；听写时静音
其他声音（Windows，可选）；首页统计（连续天数、每分钟字数、12 周热力图，只记数量）；
改写草稿支持翻译和自定义指令；桌面控制胶囊（Windows，可选）。多行文字和片段改为粘贴，
不再把换行当回车发出半句话；Qwen3-ASR 输出里带换行或漏出提示头时，不再丢掉整句或打出乱码。
发布前用 1072 段合成语音在四条识别路线上做了完整回放测试。

VocalCode 现在是一个免费应用，按 AGPL-3.0 开源，无需购买、账户或激活。下载：Windows 用
`VocalCodeSetup.exe`，Apple 芯片 Mac 用 `VocalCode-<版本>.dmg`，源码为
`VocalCode-source-<版本>.tar.gz`。两个平台的安装包均经过签名，Mac 包完成 Apple 公证。

VocalCode 安装在付费版（1.2.1 及更早）原来的位置，使用同一个数据文件夹，所以会原地替换付费版：
设置、词典、片段、会议记录和已下载的模型原样保留，无需导入；付费版的授权与试用文件保持原样，
VocalCode 从不读取。授权或试用仍有效的付费版会在应用内提示这次更新，其他情况请直接覆盖安装
（Mac 上替换“应用程序”里的 VocalCode.app）。之后卸载 VocalCode 也会保留数据文件夹。
通过 Scoop 安装的付费版无法这样更新：请先运行 `scoop uninstall vocalcode`，再从本页安装。

早期免费版 VocalCode Community 1.3.1 和 1.4.0 是单独的应用。在 Windows 上，安装程序会卸载
VocalCode Community 1.3.1 或 1.4.0 并保留它的数据文件夹，可在 **设置 → 系统 → 旧版 VocalCode**
里选择要复制的内容；在 Mac 上请自行删除 VocalCode Community 1.3.1 或 1.4.0。这两个早期版本无法在
应用内更新到本版本，请从本页下载一次。不要同时运行两个副本，否则每次听写都会被输入两遍。
首次下载模型和检查更新需要联网；录音与识别在本地处理。

Maintainer: [Daming Wu](https://github.com/wudaming00).
Issues: https://github.com/wudaming00/vocalcode-community/issues
