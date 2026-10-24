# Dell Fan Control

**项目状态：已冻结 / Frozen — 存在重大散热安全风险**

本项目因严重风扇控制安全风险无限期暂停维护、功能开发与发布，历史版本不建议安装或运行，冻结期间禁止真机测试、重新部署及重新启用后台控制

2026-10-25 曾发生持续高温、疑似风扇接近停转和 12 次真实看门狗超时后的反复重新接管，软件退出且 Boost 读回 0/0 后散热异常仍持续，关机冷却并冷启动后风扇恢复

根本原因尚未确认，现有证据不能证明软件直接造成 EC 故障，也不能排除相关风险

原 README 中的架构、控制机制、智能算法、权限模型、部署设计、恢复机制、开发历史和技术经验已分类保存在 [技术文档目录](https://github.com/csjcsl666/software-dev-notes/tree/main/dell-fan-control)

事故时间线、证据分级和未确认事项见 [2026-10-25 散热事故报告](https://github.com/csjcsl666/software-dev-notes/blob/main/dell-fan-control/THERMAL-INCIDENT-2026-10-25.md)，原始日志、coredump 元数据与 SHA256 证据仅在本机保存，未公开上传

仓库保留历史源码与提交，最后的历史开发版本为 `0.1.0-dev.1`，未发布正式版

许可证：GPL-3.0-or-later，见 [LICENSE](LICENSE)
