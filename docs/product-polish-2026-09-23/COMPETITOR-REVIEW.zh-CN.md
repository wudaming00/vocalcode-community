# VocalCode 产品打磨：Wispr Flow / Typeless 对照

研究日期：2026-09-23。范围以 Windows 桌面为主，Mac / 移动端差异明确标注。下面是本轮能核对的公开工作流，不声称枚举了所有未公开、灰度或账户专属功能。实际改动与测试见 [验证记录](RESULTS.zh-CN.md)。

## 结论先行

值得借鉴的是完整工作流：**发现入口 → 知道是否正在录音 → 安全地结束 → 看到结果 → 能恢复和纠正**，不是把竞品所有开关都堆进设置。

VocalCode 的价值仍是本机 CPU 识别、用户可控的模型和数据、可审查源码。应先提升输入可靠性和可发现性，再评估需要本地语言模型的改写、翻译、会议问答。没有同机、同音频、同输入目标的实测，就不能宣称“比竞品更快、更准”。

## 证据范围

- **官方文档**：表示厂商公开说明，不表示我们实测通过；文档也可能受版本、平台、账户和灰度影响。
- **本机静态证据**：只读检查已安装客户端发行资源，不是获得完整项目源码或服务端实现。看到字符串不等于功能已启用。
- **本项目代码**：只表示实现存在；需要另外区分单元测试、模拟音频测试和实际桌面操作。
- **未证实**：不根据广告、截图或字符串猜测功能存在或不存在。

本机安装版本为 Wispr Flow 1.6.937、Typeless 2.6.0。Typeless 官网已发布 Windows 2.8.0 的 Help me write；本次没有更新其安装，因此不能把新文档算成本机体验。[版本说明][typeless-write]

没有读取竞争产品的私人听写历史、会话凭证或账户数据库，没有触发录音、购买、权限授权或上传。客户端资源仅用于理解可观察机制，不复制其实现或素材。

## 功能对照

“有代码”不等于无缺陷；VocalCode 栏是本次修改前的基线。

| 功能 | Wispr Flow | Typeless | VocalCode 基线与借鉴判断 |
| --- | --- | --- | --- |
| 按住说话 / 免提 | 快捷键、双击锁定、浮条开始与结束。[说明][flow-handsfree] | Dictate 快捷键，可额外绑定键盘。[说明][typeless-settings] | 已有 hold / toggle；缺少鼠标可发现的常驻入口。优先补。 |
| 桌面小浮条 | 可常驻、贴三条边、临时隐藏、透明区穿透。[说明][flow-bar] | 本机浮条资源存在，非聚焦窗；完整交互尚未实测。 | 现有 Classic / Mini / Off 只作录音提示；空闲隐藏。 |
| 状态与取消 | 录音、处理、停止、取消；存在恢复入口。[说明][flow-handsfree] | 声音和麦克风状态设置。[说明][typeless-settings] | 已有状态、提示音、取消路径。需要保证按钮反馈与后台真实状态一致。 |
| 麦克风选择 | 设备选择、音量测试、断连回退；会议麦克风可独立。[说明][flow-mic] | 默认或指定设备、音量条、交互声音、听写时静音其他声音。[说明][typeless-settings] | 已有设备选择与故障恢复；不能为了降低首字延迟而偷偷保持录音。 |
| 长听写 | 最长 20 分钟；结束后插入，不等同于逐词实时写入。[说明][flow-handsfree] | 不能从“快于打字”的营销指标推算松键延迟。 | 新后台预解码缩短松键等待；分段可能降低专有词准确率，必须继续测。 |
| 边说边写 | Mac 特定 CLI 的长文本分块粘贴不是流式识别的充分证据。[说明][flow-ide] | 未证实与 VocalCode 同语义的稳定片段实时写入。 | 已有实验性停顿分段、只追加；不回删用户随后输入的内容。 |
| 标点与口头自我纠正 | Auto-cleanup 有 None / Light / Medium，默认 Light，短或特别长的片段可能跳过。[说明][flow-cleanup] | 官网说明去填充词、重复、自我纠正和列表格式。[功能页][typeless-home] | 已有轻清理和中英独立可选语气词处理；不要把有意义的“嗯 / like / so”一律删除。 |
| 按应用调整风格 | 四类应用风格；英文效果优先，其他平台也有限制。[说明][flow-styles] | 按上下文调整语气的官方声明。[功能页][typeless-home] | 已有本地应用配置：清理、渐进输入、粘贴、语气词；不是语义改写模型。 |
| 个人词典与替换 | 词语、错词替换、同步；整词匹配和长词优先。[说明][flow-dictionary] | 个人词典、纠错后自动添加的官方说明及客户端资源。 | 已有规则、自动学习、编辑撤销；优先验证短词误学、重复规则和学习反馈。 |
| 可复用片段 | 个人 / 团队 snippets。[说明][flow-snippets] | 未在本次证据中确认独立 snippets 管理。 | 已有本地片段和导入预览；要保留冲突与确认机制。 |
| IDE 上下文 | 可见变量识别；Cursor / Windsurf 文件引用；平台和 IDE 有限制。[说明][flow-ide] | 官网列出编码使用场景，不等于同等文件引用能力。 | 不默认读取整个屏幕或仓库；后续可选、局部、可审查的上下文词表更合适。 |
| 选中文本语音改写 | Command Mode，需付费；编辑失败还存在无反馈限制。[说明][flow-command] | Ask anything 可改写、解释、翻译和搜索。[说明][typeless-ask] | 已有 Ollama 本地草稿预览（轻润色 / 列表 / 摘要），不是自动选区替换或跨应用命令。保留先预览、再确认和撤销。 |
| 自定义改写提示 | Transforms 可改写选中的 1–1000 词、查看差异，支持自定义槽位和风格示例。[说明][flow-transforms] | Help me write 2.8.0 根据口述要求生成草稿。[版本说明][typeless-write] | 已有受限本地模型草稿；本轮新增外部 CLI 检测、隐私区分和受限适配，不默认加载私人上下文。 |
| 翻译与多语言 | 按一次听写推断主要语言，不是逐词识别；中英混说、Hinglish、部分语言自动识别有明确限制。[说明][flow-languages] | 自动语言识别、混合语言、多个翻译目标。[说明][typeless-translate] | 各语言走实际支持的本地模型；翻译不应冒充语音识别。“100+”不等于每种语言同等准确。 |
| 音频文件导入 | 官方说明不支持任意音视频文件导入转写；已有文本笔记导入是另一件事。[说明][flow-import] | 本轮未确认。 | 已有本地音频导入。可作为清晰差异，但须实测长文件、采样率、取消和断点恢复。 |
| 历史与恢复 | 历史搜索、复制、音频回放 / 问题反馈。[应用说明][flow-hub] | 2.4.0 起可选择跨设备历史云同步。[版本说明][typeless-sync] | 默认会话历史；可选持久化加密诊断。应提供不打开复杂设置的恢复入口。 |
| 随手笔记 | Scratchpad 独立笔记、窗口内多标签。[说明][flow-scratchpad] | Ask anything 的结果卡片不等于完整笔记系统。 | 可先做轻量本地草稿；不宜在本轮重建一个文档编辑器。 |
| 会议检测与提醒 | Windows / Mac、日历提醒、检测开关、稍后和关闭。[说明][flow-calendar] | 本机包未检出 meeting 字符串，不能据此断言没有该功能。 | 已有本地检测、提醒、忽略、开始前确认；重点回归按钮和去重。 |
| 日历 | Google / Outlook 取决于开放范围；有授权恢复流程。[说明][flow-calendar] | 未证实。 | 已有显式配置的日历能力；优先只读元数据，不默认修改邀请或发通知。 |
| 会议笔记层次 | 人写笔记、摘要、逐字稿分离。[说明][flow-notes] | 未证实完整会议笔记。 | 现有本地逐字稿、笔记和导出；修改摘要不能覆盖原始转写。 |
| 会议后总结 / 问答 | 云端摘要、带来源的会议聊天。[摘要][flow-summary] / [聊天][flow-chat] | Ask anything 不是已验证的全会议问答。 | 本地摘要需要证据回链和明确“生成内容”；不能伪造说话人或决定。 |
| 会议中听写 | 可继续普通听写，对自己口述文本做区分。[说明][flow-concurrent] | 未证实。 | 已有并行录音 / ASR 调度。继续测资源争用，不要为浮窗创建第二套音频引擎。 |
| 自动结束 | 通话结束、静音、倒计时、继续与恢复；存在不同结束理由。[说明][flow-concurrent] | 未证实。 | 已有自动结束逻辑。必须区分“会议静音”和“会议结束”，允许取消倒计时。 |
| 回声与设备故障 | 官方明确扬声器可能造成重复转写，建议耳机。[说明][flow-meeting-audio] | 未证实会议能力。 | 已有本地回声处理，仍需真实扬声器 / 双人重叠语音回归，不能宣称彻底消除。 |
| 隐私 / 离线 | 会议转写和摘要用云；离线录音后联网处理。[说明][flow-meeting-privacy] | Ask anything 需要网络；可选云同步是独立数据行为。[说明][typeless-ask] / [历史][typeless-sync] | ASR 和会议音频处理本地。模型下载、更新和用户配置的集成仍可能联网。 |
| 无障碍 | 文档列出键盘导航和已知缺口，包括浮条拖动依赖鼠标。[说明][flow-a11y] | 有 UI 语言 / 外观设置，全面读屏表现未测。 | 浮条非聚焦意味着不能作为唯一入口；保留快捷键、主窗和托盘的等价操作。 |
| 更新时机 | 活跃听写 / 会议延后更新。[说明][flow-updates] | 本轮未验证。 | 发布安装与录音生命周期必须互锁；本轮不自动发布或覆盖安装。 |
| 批量词典迁移 | 桌面实验性 CSV 词典 / JSON 片段导入，受订阅与组织策略限制。[说明][flow-bulk] | 官方提供词典 CSV 导入；本页未提供导出步骤。[说明][typeless-history] | 已有格式检查、冲突预览、明确确认、撤销。不能把读取竞品私人数据库当成迁移前提。 |
| 音频失败重试 | 历史与反馈流程见应用指南。[说明][flow-hub] | 可用同一音频 Retry、下载音频，并设置历史保留周期。[说明][typeless-history] | 音频诊断与文字诊断应分别同意；没有保留音频时不能显示虚假的“重新识别”。 |
| 自动个性化 | 上下文与自动学词为独立控制。[说明][flow-data] | 自动学习写作习惯，可查看进度并关闭；隐私承诺是厂商说明，未独立审计。[说明][typeless-personal] | 本地可审查规则比不可解释的风格学习更适合首期；每次学习应能看见来源与撤销。 |
| 团队词典 / 权限 | 个人 / 团队库、来源标记、批量分享、管理员范围和连接器权限。[词库][flow-team] / [策略][flow-policy] | 本轮未验证同等组织管理能力。 | 暂不复制企业管理和云同步。先把本地导入导出、冲突和备份做好，避免承担无关的账户服务。 |

## 容易被广告掩盖的边界

- 两家厂商的数据控制页均说明转写依赖云处理。关闭训练用途、关闭云端保存、仅在设备保存历史，是不同的选择；不能据“零保留”推断为离线推理。[Wispr 数据控制][flow-data] / [Typeless 数据控制][typeless-data]
- Typeless 的 History 快速指南仍写“历史仅在设备”，但 2.4.0 更新和数据控制页已明确可选云同步。本报告按“默认本地历史、可选云同步”记录，不把旧指南的一句话推广成绝对承诺。[历史指南][typeless-history] / [版本说明][typeless-sync] / [数据控制][typeless-data]
- Flow 的上下文帮助页对隐私模式、上下文上传和跨平台能力的文字并不完全一致；数据控制页明确云转写。没有受控网络实测，不能断言某账号实际上传的每个字段。本项目不照搬默认读屏，后续上下文增强需独立开关、范围预览和敏感字段排除。[上下文帮助][flow-context] / [数据控制][flow-data]
- Typeless Ask anything 的“编辑选区”与“解释选区”操作不同：前者直接替换，后者保留原文并展示结果。VocalCode 当前选择更保守的候选草稿，不具备跨应用选区改写，不能把按钮名相似当作功能相等。[操作指南][typeless-ask-guide]

## 小浮窗：建议规格

1. 可选常驻，默认不改变现有安装习惯；不用误以为麦克风一直开启。
2. 休息时只占一个小胶囊，悬停展开；录音状态提供明确的停止和取消。
3. 点击开始采用免提，仅一次明确操作启动，不能靠旧队列消息延迟启动。
4. 原生窗不获取键盘焦点，页面也不主动 focus。真正输入仍走现有目标验证层。
5. 每次控制请求绑定状态代次和短有效期；旧按钮、双击、模型忙碌、关闭后的消息都不能开始新录音。
6. 边缘位置限定在工作区内，适配任务栏、负坐标、多 DPI；可临时隐藏，设置页可找回。
7. 空闲全屏场景不打扰；正在录音不能隐藏唯一的录音提示。
8. 打开主窗是显式动作，和开始 / 停止分开。浮窗不显示完整私人听写内容。
9. 不把“处理完成”画成“输入成功”；失败时提供历史恢复路径。

## 本机实现线索，不是私有源码复刻

Wispr Flow 的发行包中存在独立 status / contextMenu / meeting_recorder / scratchpad 相关资源。主进程资源包含非激活显示、鼠标穿透切换、保存 dock edge 的调用。Typeless 的浮条窗配置可直接看到不可聚焦，另有可聚焦的 interactive card。这支持“控制条和编辑卡片分层”的设计选择，但无法证明服务端模型、全部运行分支或本账号已开放的功能。

读取的是 app.asar 里的发行 JavaScript，非官方完整源码。Typeless 部分内容已混淆；本次不绕过混淆或提取秘密。检出与否都不作为完整功能证据。原始资源不进入本仓库。

## 开源实现补充

另见 [Handy / VoiceInk 公开源码设计笔记](OPEN-SOURCE-NOTES.zh-CN.md)：固定提交、窗口焦点 / DPI、串行录音协调、流式终止、CLI 改写边界。Wispr Flow / Typeless 的发行资源检查不等同于这些开源仓库的源码审阅。

## 性能基线和已知问题

上一轮 Fish 合成音频、同机 SenseVoice CPU / 4 线程测试中，普通英文 36 秒样本的松键后结果等待由整段 760 ms 降至预解码 92 ms；技术词样本分段 WER 却从整段 9.52% 上升到 12.70%，更短渐进片段为 19.05%。这不是全面准确率结论：仅三个合成样本、单次结果、没有竞争产品同条件测试，且只测内部输出，不是桌面输入端到端。

因此优先实验分段上下文和更保守的提交边界；不能拿单个快样本作“所有语言更快更准”的广告，也不能用针对样本文本的硬编码替换修饰结果。

## 本轮落地与明确保留的缺口

- **已落地到本地开发工作区**：可选 Windows 常驻控制条、短效版本绑定的操作桥、历史恢复入口、Ollama / 已有 CLI 检测、默认本地的手动改写候选、逐次云端同意、编辑 / 接受 / 撤销和窄屏会议布局。没有复制竞品私有实现或更改正在使用的安装。
- **已做工程验证**：纯逻辑与前端合约、隐藏原生 WebView2 布局 / IPC、真实本地 ASR 的合成音频回放、虚构文本改写。准确率、资源与等待时间分别记录，不用“测试通过”掩盖合成样本和原生输入验收的区别。
- **仍需人工验收**：可见控制条的鼠标 / 触屏、不同 DPI 多屏、原生 Win32 / Electron / WebView2 目标以及真人语音。Mac 需要独立原生实现 / 编译验收；不能用 Windows 结果代替。
- **暂不直接做**：自动读整屏 / 整仓库、无预览覆盖选区、默认云历史、团队管理与自动执行 agent 工具。它们不是当前本地听写产品必须追平的条件。
- **下一步优先**：专业词与自然口音基准、不同级别 CPU 的英文模型对照、用户可见的真实端到端输入可靠性。[英文 CPU 评估方案](ENGLISH-CPU-NEXT.zh-CN.md) / [本机验收清单](LOCAL-ACCEPTANCE.zh-CN.md)。

[flow-bar]: https://docs.wisprflow.ai/articles/1790396454-move-and-dock-the-flow-bar-on-desktop
[flow-cleanup]: https://docs.wisprflow.ai/articles/4283510616-auto-cleanup-control-how-much-flow-edits-your-dictation-beta
[flow-transforms]: https://docs.wisprflow.ai/articles/8068950331-how-to-use-transforms-beta
[flow-languages]: https://docs.wisprflow.ai/articles/3191899797-use-flow-with-multiple-languages
[flow-import]: https://docs.wisprflow.ai/articles/7689167034-can-i-upload-audio-files-to-wispr-flow-for-transcription
[flow-handsfree]: https://docs.wisprflow.ai/articles/6391241694-use-flow-hands-free
[flow-mic]: https://docs.wisprflow.ai/articles/3566082841-fix-missing-first-words-in-transcriptions
[flow-styles]: https://docs.wisprflow.ai/articles/2368263928-how-to-setup-flow-styles
[flow-dictionary]: https://docs.wisprflow.ai/articles/4052411709-teach-flow-your-words-with-the-dictionary
[flow-snippets]: https://docs.wisprflow.ai/articles/4816874402-troubleshooting-guide-for-snippets-pasting-only-part-of-the-text-or-code
[flow-ide]: https://docs.wisprflow.ai/articles/6434410694-use-flow-with-cursor-vs-code-and-other-ides
[flow-command]: https://docs.wisprflow.ai/articles/4816967992-how-to-use-command-mode
[flow-hub]: https://docs.wisprflow.ai/articles/5096240724-navigating-the-wispr-flow-app-desktop-ios-and-android
[flow-scratchpad]: https://docs.wisprflow.ai/articles/9618237082-using-the-scratchpad-to-save-and-edit-notes
[flow-calendar]: https://docs.wisprflow.ai/articles/8955305188-meeting-detection-reminders-and-calendar-in-notetaker-beta
[flow-notes]: https://docs.wisprflow.ai/articles/9406970664-Meeting-notes-and-the-editor-in-Notetaker
[flow-summary]: https://docs.wisprflow.ai/articles/1422535682-flow-summaries-in-notetaker-beta
[flow-chat]: https://docs.wisprflow.ai/articles/1411703818-view-ai-answers-in-flow-chat-with-show-in-chat
[flow-concurrent]: https://docs.wisprflow.ai/articles/8175153619-dictating-during-a-meeting-with-notetaker-beta
[flow-meeting-audio]: https://docs.wisprflow.ai/articles/3089221553-troubleshooting-notetaker-recording-and-audio-beta
[flow-meeting-privacy]: https://docs.wisprflow.ai/articles/4497184932-notetaker-privacy-and-security-overview
[flow-a11y]: https://docs.wisprflow.ai/articles/3941699399-keyboard-and-screen-reader-accessibility-in-wispr-flow
[flow-updates]: https://docs.wisprflow.ai/articles/2363354003
[typeless-home]: https://www.typeless.com/
[typeless-settings]: https://www.typeless.com/help/quickstart/settings
[typeless-ask]: https://www.typeless.com/ask-anything
[typeless-write]: https://www.typeless.com/help/release-notes/windows/use-help-me-write-desktop
[typeless-sync]: https://www.typeless.com/help/release-notes/windows/keep-your-history-synced
[typeless-translate]: https://www.typeless.com/help/release-notes/windows/set-multiple-target-languages
[typeless-history]: https://www.typeless.com/help/quickstart/history-and-dictionary
[typeless-personal]: https://www.typeless.com/help/quickstart/personalization
[typeless-ask-guide]: https://www.typeless.com/help/quickstart/ask-anything
[typeless-data]: https://www.typeless.com/data-controls
[flow-data]: https://wisprflow.ai/data-controls
[flow-context]: https://docs.wisprflow.ai/articles/4678293671-Context-Awareness
[flow-bulk]: https://docs.wisprflow.ai/articles/8955301725-How-Do-I-Bulk-Import-Dictionary-Items-and-Snippets
[flow-team]: https://docs.wisprflow.ai/articles/9639977157-view-shared-dictionary-words-and-snippets-in-the-admin-portal
[flow-policy]: https://docs.wisprflow.ai/articles/7133384604-restrict-org-wide-snippet-and-dictionary-sharing-snippet-sharing-policy
