# 戴尔风扇控制（Dell Fan Control）

为 Dell G15 5515 Ryzen Edition 编写的 Linux 原生风扇控制软件，Rust 加 iced 图形界面，只通过内核 `alienware_wmi` 驱动提供的接口工作

- 不使用 acpi_call，不直接读写 EC，不运行 root 守护进程
- 风扇控制总开关默认关闭，关闭时风扇完全交还 Dell 固件
- 均衡、性能、自定义、智能四种模式

## 当前状态

开发版 `0.1.0-dev.1`，尚未发布正式版

| 项目 | 状态 |
| --- | --- |
| 温度、转速、boost 读回 | 本机实测可读 |
| boost 写入后风扇是否响应 | 尚未在真机验证，需要用户授权首次写入 |
| boost 置 0 是否完全交还固件 | 尚未在真机验证 |
| 性能模式（ppd 切换 performance） | 代码完成，真机未验证 |
| 智能算法 | 仅在模拟模型中验证，参数尚未用真实数据校准 |
| 崩溃与 SIGKILL 后的恢复 | 单元测试与模拟验证，真机未验证 |

真机验证结果以 [software-dev-notes/dell-fan-control](https://github.com/csjcsl666/software-dev-notes/tree/main/dell-fan-control) 中的 TESTING.md 为准

## 工作方式

```
iced 界面（普通用户）
   │  Unix socket 0600，SO_PEERCRED 校验 uid，类型化 JSON 请求
后台服务（systemd 用户服务，普通用户）
   │  只写 fan1_boost 与 fan2_boost       │  档位只通过 power-profiles-daemon 的 D-Bus 切换
alienware_wmi hwmon（udev 规则只把这两个文件交给该用户）
```

- `fanN_boost` 是内核定义的 0 到 255 附加量，作用在固件基准转速之上，它不是转速百分比，0 不代表停转，255 也不保证达到标称最高 RPM
- 性能模式请求 power-profiles-daemon 切换到 performance，G 系列笔记本上内核会同时开启 G-Mode，它也会放宽 CPU 与 GPU 功耗限制
- 关闭总开关或彻底退出时，后台把本软件写过的 boost 写回 0 并读回核对，再把档位恢复为本软件切换之前的值，如果档位已被别的程序改过就不再改回

## 失效保护

| 情况 | 谁来恢复 |
| --- | --- |
| 关闭界面窗口 | 不恢复，后台按已启用的策略继续运行 |
| 界面或托盘点击彻底退出 | 后台自身恢复并读回核对，然后以退出码 0 结束，systemd 不会重新拉起 |
| 关闭总开关 | 后台立即恢复，之后不再周期性写入 |
| 后台崩溃、SIGKILL、卡死 | systemd 的 `ExecStopPost` 独立运行 `dell-fan-control restore`，卡死由 `WatchdogSec` 发现 |
| 注销 | 用户服务停止，同上 |
| 断电或恢复失败 | 覆盖记录保留在 `~/.local/state/dell-fan-control/applied.json`，下一次启动先恢复 |

udev 规则只负责文件权限，进程退出时不会自动清除 boost，内核驱动也没有超时自动恢复机制，因此恢复依赖上面的覆盖记录与 systemd 生命周期

## 安装

需要 Rust 工具链、`systemd` 用户会话，可选 `rsvg-convert` 生成 PNG 图标

```bash
scripts/install.sh                 # 编译 release 并安装到 ~/.local/opt/dell-fan-control
sudo scripts/install-system.sh     # 一次性安装 udev 规则，自定义与智能模式需要
```

安装后可在应用启动器中搜索 戴尔、风扇、Dell、Fan 打开，登录自启在界面的设置中开启，默认关闭

## 卸载与回滚

```bash
scripts/rollback.sh                # 切回上一个安装版本
scripts/uninstall.sh               # 先交还固件再删除文件，加 --purge 同时删除配置
sudo scripts/uninstall-system.sh   # 删除 udev 规则并把两个文件交还 root
```

## 命令行

```bash
dell-fan-control                   # 打开界面
dell-fan-control status            # 查看后台状态，--json 输出完整数据
dell-fan-control enable on|off     # 总开关
dell-fan-control mode balanced|performance|custom|smart
dell-fan-control custom cpu 128    # 自定义模式 boost
dell-fan-control quit              # 彻底退出并恢复固件控制
dell-fan-control restore           # 按覆盖记录恢复，systemd 停止后自动调用
dell-fan-control probe             # 只读检测硬件
dell-fan-control --simulate daemon # 模拟笔记本，不访问真实硬件
```

## 智能模式

分段线性曲线、双时间尺度 EMA、升降双阈值迟滞、降速保持时间、非对称步长、临界温度保护，CPU 与 GPU 各自计算散热需求后再协调输出，界面可查看每一层的中间值

设计参考了 CoolerControl、NBFC-Linux、thinkpad-fan-control、thinkfan、awcc-linux 与 Cryo 的算法思路，没有复制它们的代码，参考位置与取舍见 [SMART-CONTROL.md](https://github.com/csjcsl666/software-dev-notes/blob/main/dell-fan-control/SMART-CONTROL.md)

## 兼容性

- 目标机型 Dell G15 5515，BIOS 1.31.0，CachyOS 内核 7.2，Hyprland
- 理论上适用于 `alienware_wmi` 提供 `fan1_boost`、`fan2_boost` 的其他 Dell G 系列与 Alienware 机型，但未验证
- GPU 功耗通过 NVML 读取，没有 NVIDIA 驱动时不显示

## 已知限制

- boost 的真实效果尚未在本机验证，这是正式版前的必过项
- 内核文档提示部分机型只有在 custom 档位下 boost 才稳定生效，本软件不写 custom 档位，如果本机实测需要再调整
- 默认曲线与临界阈值是初始值，尚未经过真实温度数据校准
- 透明窗口的底层设置在下次打开界面时生效

## 许可证

GPL-3.0-or-later，见 LICENSE
