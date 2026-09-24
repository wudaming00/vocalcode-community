# 英文长听写：下一轮 CPU 模型评估

核查日期：2026-09-23。这是候选和实验设计，不是已经跑过所有模型的排名。本轮实测仍为本机现有 SenseVoice；没有下载或更换用户模型。

## 先分清四种延迟

- 冷启动：从选择模型到可开始识别，包含读取权重和创建运行时。
- 首次可用文字：从开始说话到第一段**可提交**文字，不把可能撤回的预览混进去。
- 松键等待：从停止录音到最终完整结果；后台预解码能降低它，但不意味着总计算量减少。
- 原生输入完成：最后文字真正到达目标输入框。本轮内存回放不包含这一段。

还要分别记录总 CPU 时间、峰值工作集、峰值提交内存、正确率、技术词 / 否定 / 数字错误和丢段。不要把 GPU 批量吞吐、下载文件大小或厂商一句“实时”直接转换成这几项。

## 候选顺序

| 候选 | 为什么值得测 | 集成和证据边界 |
| --- | --- | --- |
| 现有 SenseVoice int8 | 已接入、已有基线；先测语言提示和线程，排除调度问题 | 本轮九条合成样本不能代表所有英文口音；切片过短有技术词退步风险 |
| Parakeet TDT 0.6B v3 | 英文与欧洲语言专长，带大小写、标点与时间戳 | 本仓库已有 sherpa 适配器，但本机未安装权重。官方 25 种语言不含中文 / 日文 / 韩文 / 印地语。不能把 A100 的长音频条件承诺给普通 CPU。[模型卡](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) |
| Moonshine English Small / Medium Streaming | 原生流式架构值得用于“边说边处理”，英文分别 123M / 245M 参数；较小 Tiny Streaming 为 34M | 当前官方表给出浮点参考结果，量化成绩不应直接沿用；本仓库尚无对应流式状态适配器。优先独立探针，不能只替换模型文件。[官方模型表](https://moonshine-voice.readthedocs.io/en/latest/models/available-models/) |
| Whisper small.en / base.en 量化基线 | CPU 可运行的成熟英文对照；可衡量轻量化与准确率取舍 | `whisper.cpp` 是另一运行时，不能用其结果证明当前 sherpa Whisper 转换性能；同音频、同线程和相同暖机策略再比较。[运行时说明](https://github.com/ggml-org/whisper.cpp) |

不立即全语言改默认：英文专用模型不能负责混合中文；更高准确率模型可能增加下载、冷启动与内存。现有中文用户的 SenseVoice 选择应保留，后续以显式语言 / 模型选择或可撤销建议处理。

## Moonshine 有两个容易混淆的地方

1. sherpa 文档中的 `moonshine-base-en-quantized-2026-02-27` 使用 offline recognizer，页面的实时示例带 VAD / simulated-streaming；这不自动等于 Moonshine Voice 的 Medium / Small Streaming 架构。需核对模型图和流式状态 API，而不是只看“v2”名称。[sherpa 模型页](https://k2-fsa.github.io/sherpa/onnx/moonshine/models-v2.html)
2. 新旧模型许可证与量化产物不同。当前官方说明多数新模型为 MIT，旧的部分非英文非流式权重有 Community 限制；实际引入时锁定版本、下载哈希和随权重附带的许可证。本轮没有引入这些权重。量化文档还说明 frontend 图与权重分离、使用带日期目录，不能把新旧缓存文件混装。[许可证说明](https://github.com/moonshine-ai/moonshine#license) / [量化说明](https://moonshine-voice.readthedocs.io/en/latest/models/quantization/)

Moonshine 官方 benchmark 特别区分句尾结果延迟和离线吞吐，并说明平台、温度和运行次数影响数字；这也是本轮保留重复回放、披露后台负载的原因。其“句子结束”依据 VAD，不能直接与我们的“用户松键”时间作横向速度广告。[测量定义](https://moonshine-voice.readthedocs.io/en/latest/using/benchmarks/)

## 建议下一轮的最小实验

1. 在隔离模型缓存中固定权重和运行时版本；不改用户默认模型，不启动麦克风。
2. 至少准备 30 条经授权且人工校对的自然英文，覆盖不同口音、轻声、技术词、数字、否定、自我修正；另有中英混合和静音 / 噪声负样本。当前 Fish 合成音频仅作为可重复工程回归。
3. 每个候选先 2 / 4 线程、冷 / 暖各测，再按不同 CPU 分档；不因本机高端 16 核表现就给所有电脑开 8 线程。
4. 普通松键交付与渐进交付分开评价。流式模型只在明确定稿后追加，不把用户已收到的文字反复回删。
5. 先排除崩溃、幻觉、重复、漏段和失控内存，再决定准确率 / 延迟的取舍。保存原始结果，不给特定样本加替换词来降低 WER。
6. Win32、Electron、WebView2 的可见空白输入框单独验收；模型成绩不代替输入安全与焦点测试。

当前实现与已完成数据见 [验证记录](RESULTS.zh-CN.md)。
