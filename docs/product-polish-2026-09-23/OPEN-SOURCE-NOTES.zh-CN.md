# 开源同类：哪些设计值得借鉴

核查日期：2026-09-23。本文补充 [Wispr Flow / Typeless 功能对照](COMPETITOR-REVIEW.zh-CN.md)。只审阅公开仓库中的相关文件，不是完整安全审计，也没有运行其应用或测试其性能。没有将第三方实现、图片或提示词复制到 VocalCode。

## 固定版本及范围

- [Handy](https://github.com/cjpais/Handy/tree/8f9cf53cd1410cda26beea39ff802ac306e39585)，提交 `8f9cf53cd1410cda26beea39ff802ac306e39585`。仓库 LICENSE 为 MIT。重点查看 `overlay.rs` 和 `transcription_coordinator.rs`。
- [VoiceInk](https://github.com/Beingpax/VoiceInk/tree/d7b528aaf184db2ee946dffe920e557a1a34617d)，提交 `d7b528aaf184db2ee946dffe920e557a1a34617d`。仓库 LICENSE 为 GPL-3.0 文本。重点查看 macOS 小窗、流式录音和 Local CLI 的实现。许可证记录不是复用代码的批准；本轮没有引入第三方代码。

## 可直接用于设计决策的发现

| 观察到的实现 | 对 VocalCode 的启发 | 本轮状态 / 边界 |
| --- | --- | --- |
| Handy 以串行 coordinator 处理录音、处理中的快捷键与取消；其纯状态判断独立于 GUI | 控件不能直接启动第二套录音，必须走既有引擎队列 | 已采用带版本号与有效期的单槽命令桥；取消优先于等待中的 Start；补回归测试 |
| Handy 对延迟隐藏记录 show generation，避免旧会话的 hide 影响新会话 | 所有异步确认、隐藏、错误与候选稿都需要防止过期结果作用到新会话 | 控制条和改写请求已带状态 / 请求版本；控制条没有延迟隐藏回调；补过期回包测试 |
| Handy Windows 小窗处理目标显示器 DPI 与文字缩放；README 也明确记录浮窗抢焦点风险 | 漂亮小窗必须同时验证坐标、辅助功能字号与焦点，不只是 CSS 截图 | Windows 隐藏原生探针测了本机 127% 文字缩放；混合 DPI 与真实鼠标 / 触屏仍待人工验收 |
| Handy 不向关闭的 overlay 发送音量，且限制事件更新频率 | 隐藏浏览器组件不应持续耗资源 | VocalCode 本轮新增隐藏状态抑制和相同音量去重；只在可见录音状态推送 |
| VoiceInk `MiniRecorderPanel` 使用 nonactivating panel、跨 Space、无背景阴影的原生窗口 | 控制条与可编辑卡片要分层，不能假设 WebView 点击不会激活窗口 | 本轮实现 Windows 控制条非激活路径；macOS 原生控制条尚未实现，不用 Windows 结果替代 macOS 验收 |
| VoiceInk 流式服务有连接 / 流式 / 提交 / 失败 / 取消状态，记录音频收发与丢弃量，并在连接完成后再检查取消 | “首字快”和“最终完整”是不同指标，取消及队列溢出必须有明确处理 | VocalCode 保留追加式稳定片段和完整输出校验；测试记录首输出、松键等待、错误率；不把中间预览宣称成已安全写入 |
| VoiceInk CLI 提供 Claude、Codex 等模板，经 shell 执行，并通过环境 / 参数或 stdin 传入文字 | 用户已有 CLI 可以是可选后端，但“本机命令”不能等同于“本地推理”，也不能照搬通用 shell 模板 | VocalCode 使用受限直接进程调用、stdin、超时和输出协议检查；Claude 逐次同意；Codex 仅检测，未启用生成 |

相关实现的固定链接：

- [Handy overlay](https://github.com/cjpais/Handy/blob/8f9cf53cd1410cda26beea39ff802ac306e39585/src-tauri/src/overlay.rs)
- [Handy transcription coordinator](https://github.com/cjpais/Handy/blob/8f9cf53cd1410cda26beea39ff802ac306e39585/src-tauri/src/transcription_coordinator.rs)
- [VoiceInk MiniRecorderPanel](https://github.com/Beingpax/VoiceInk/blob/d7b528aaf184db2ee946dffe920e557a1a34617d/VoiceInk/Features/Recording/Presentation/MiniRecorderPanel.swift)
- [VoiceInk StreamingTranscriptionService](https://github.com/Beingpax/VoiceInk/blob/d7b528aaf184db2ee946dffe920e557a1a34617d/VoiceInk/Features/Recording/Streaming/StreamingTranscriptionService.swift)
- [VoiceInk LocalCLIService](https://github.com/Beingpax/VoiceInk/blob/d7b528aaf184db2ee946dffe920e557a1a34617d/VoiceInk/Infrastructure/Providers/Enhancement/LocalCLI/LocalCLIService.swift)

## 不应该直接追平的东西

1. 不为追求更低的首字延迟，把不稳定候选稿直接写进用户文档。撤回已输入文字容易覆盖用户自己打的字。
2. 不把“只读沙箱”写成“没有工具权限”。仍可读取文件或启动子进程的 agent，不能自动视为纯文本润色器。
3. 不因为其他产品有整屏上下文、云历史或任意 shell 命令，就默认给 VocalCode 加同样权限。显式选择一小段文本，先预览、再接受，是本轮边界。
4. 不把别的仓库当前参数或模型名称当作本机可用配置。版本、服务账户、模型许可、CPU 性能均需独立核验。

## 下一阶段优先级

- **P0：真实输入端的可靠性。** 本轮内部 Collector 校验不能证明所有 IDE / 浏览器原生文本框都能成功输入；需用户空白测试窗口下的焦点切换、快速开始停止、异常取消验收。
- **P0：自然人音频。** 当前新样本是合成音频，不能代表口音、轻声、杂音和自我改口。后续收集经同意且有人工校对的参考文本。
- **P1：英文 CPU 模型对照。** 先验证当前语言提示和线程数，不贸然下载数 GB 模型；再在同机同音频上比较更合适的英文模型。
- **P1：macOS 独立控制条与辅助功能。** 保留快捷键和原有指示器；新 Windows 控制条维持默认关闭 Beta。
- **P2：更丰富的应用风格与选择文本改写。** 只有在预览、数字 / 否定词保护、撤销及焦点验收完成后，再考虑可选直接替换。
