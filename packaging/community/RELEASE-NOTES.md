# VocalCode Community

Free, local-first dictation and meeting notes under AGPL-3.0-only. No purchase,
account or activation code is required. Recognition runs on your CPU after
the selected model downloads.

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

社区版免费使用全部本地功能，无需激活。Windows 与 Apple 芯片 Mac 安装包
均经过签名；Mac 包完成 Apple 公证。社区版独立安装，不覆盖旧版或迁移／删除旧数据。
开始听写前请退出另一个版本，避免快捷键监听冲突。词典与片段可由用户主动导入。
首次下载模型和检查更新需要联网；录音与识别在本地处理。

Maintainer: [Daming Wu](https://github.com/wudaming00).
Issues: https://github.com/wudaming00/vocalcode-community/issues
