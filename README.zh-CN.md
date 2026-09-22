<p align="center">
  <img src="docs/assets/vocalcode-community.svg" alt="VocalCode：你的声音，留在你的电脑。" width="100%" />
</p>

<p align="center"><strong>本地听写与会议记录，为 CPU 桌面设备设计。</strong></p>
<p align="center"><a href="README.md">English</a> · 简体中文</p>
<p align="center">维护者：<a href="https://github.com/wudaming00">Daming Wu</a> · <a href="LICENSE">AGPL-3.0-only</a> · <a href="MAINTAINERS.md">联系与参与</a></p>

> 这是 **早期源码预览版，不是可直接替换现有安装的正式版本**。
> 自有桌面源码以 AGPL-3.0-only 开放；社区构建无需激活，但仍须遵守下文的数据隔离警告。
> 签名安装包和原生设备验证尚未完成，详见 [正式版本准备清单](PUBLICATION_BLOCKERS.md)。

[![Community checks](https://github.com/wudaming00/vocalcode-community/actions/workflows/community-ci.yml/badge.svg)](https://github.com/wudaming00/vocalcode-community/actions/workflows/community-ci.yml)

## 我是谁，为什么做这个

我是 **Daming Wu**，VocalCode 的维护者。最初做它，是想直接说出给编程工具的长提示词，
而不是每一句都用键盘打出来。后来逐步加入了本地会议记录、个人词典和可找回的听写历史。

我希望语音识别留在自己的电脑上，不强制独立显卡，也让用户清楚知道录了什么、保存了什么。
准备开放源码，是想把精力更多放在产品本身，让真实问题、语言测试和改进能共同积累。

欢迎一起把原项目做好：提供可复现的错误、测试不同语言和设备、改善键盘操作，或者提交小而可靠
的修复。不写 Rust 也能参与。参阅 [近期方向](ROADMAP.md)、[贡献指南](CONTRIBUTING.md)
和 [维护者与联系方式](MAINTAINERS.md)。独立分支也是开源允许的选择，不强制贡献回本项目。

## 可以做什么

- **对着电脑说话，文字进入输入框**：按住快捷键开始听写，保留前台窗口及焦点安全校验。
- **在本地记录会议**：麦克风与系统音频、音频导入、转写检索、笔记、书签与导出。
- **让词典适应你的工作**：手动词典、自动纠错学习，以及可编辑、可撤销的学习提示。
- **找回未成功输入的文字**：会话历史；可选的本地持久化诊断、容量控制与导出。
- **选择适合的模型**：支持不同语言的本地模型路线，不要求独立显卡。

社区构建开放全部本地功能，无需账户、付费或激活码。录音仍须你确认并取得参与者许可。

## 自行构建

先按 [构建文档](BUILDING.md) 安装 Rust、原生编译工具及运行环境，再执行：

```sh
git clone https://github.com/wudaming00/vocalcode-community.git
cd vocalcode-community
cargo build -p vocalcode-app --release --locked --features community
```

候选目录默认开启社区模式。原有私库仍需显式指定该参数，不会改变现有正式发行版。
目前没有公开的社区版安装包。预览版仍共用原有用户数据目录，建议使用独立的操作系统用户测试，
不要直接覆盖正在使用的版本。

## 隐私与真实边界

识别模型下载完成后，语音识别和会议音频处理在设备本地运行，这些处理流程不上传音频或转写。
模型下载需要网络；用户明确配置的外部日历等集成也可能访问网络。社区版不请求激活、试用验证
或付款，暂时关闭原付费版本的自动更新，避免更新回收费版。

多人同时说话和扬声器回声仍可能影响会议准确率。渐进输入是实验功能，语言模型支持列表也不等于
每一种语言都经过完整的母语者测试。GitHub Actions 的 Windows/macOS 构建不能替代干净环境、
真实录音设备及系统权限验证。部分界面仍有旧的 Pro/授权联网文案，社区功能并不因此收费或锁定。

## 参与与许可证

参阅 [贡献指南](CONTRIBUTING.md)、[安全说明](SECURITY.md) 和
[公开前检查清单](PUBLICATION_BLOCKERS.md)。请用合成数据复现问题，不上传真实会议、凭证或私人日志。

VocalCode 自有桌面源码采用 **AGPL-3.0-only**，允许符合条款的商业使用；不是“禁止商用”。
分发软件、通过网络提供修改版服务时有相应源码提供义务，但普通听写文字和会议记录不会因为使用
软件就需要公开。详见 [许可证正文](LICENSE)、[许可范围](LICENSING.md) 和 [品牌说明](BRANDING.md)。
模型、原生库及其他第三方组件保留各自许可证，详见 [第三方声明](THIRD-PARTY-NOTICES.txt)。

社区支持以维护者和贡献者可投入的时间为限，不承诺响应时限、修复日期或 SLA。这不代表对既有付费
购买条款作出变更。公开反馈中不要上传私人录音、凭证或尚未修复漏洞的敏感细节。
