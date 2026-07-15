# Mosh 协议兼容性审计（2026-07-15）

## 结论摘要

本审计把 MoshCatty 与最新版 `mobile-shell/mosh`、Mosh 论文和 RFC 7253 逐项对照，并在用户提供的 Ubuntu 24.04 公网机器上，以系统自带的官方 `mosh-server 1.4.0` 为服务端，从本地 macOS 运行 MoshCatty 做真实互通测试。文中的“修复前”数据来自提交 `8cb4f53`，“修复后”数据来自本次工作树。

结论不是“完全重写失败”：AES-128-OCB3、方向位、UDP 基本封装、protobuf 字段、zlib、SSP 的编号状态、乱序状态重建、HostBytes 终端画面和预测的主要规则都与官方实现兼容，当前客户端可以连接官方服务端并完成双向输入与画面更新。

提交 `8cb4f53` 针对 #2121 的画面重建方向是正确的；后续对照又修复了时间与重传、断网恢复、端口跳转、关闭流程、ECN、分片和预测等多项核心缺陷。本轮继续补齐了官方的连接中提示、断线计时提示、恢复后清除提示、`Ctrl-^ .` 快捷退出，以及快速重复改字时的预测历史判断。修复后的关键结果如下：

| 场景 | 修复前 | 修复后 | 官方客户端参考 |
|---|---:|---:|---:|
| 空闲 10 秒上行包数 | 154 | 4 | 4 |
| 500 ms RTT 下相隔 50 ms 的两个按键，到达服务端的间隔 | 580 ms | 269 ms | 251 ms |
| 完全断网 65 秒 | 约 59.3 秒退出 | 保持在线并在恢复后继续 | 保持在线并继续 |
| 原 UDP 路径失效 | 报错退出 | 自动更换本地端口，约 9–10 秒恢复 | 约 8 秒恢复 |
| 120×40 初始终端 | 远端状态仍为 80×24 | 首个状态即为 120×40 | 120×40 |
| 正常退出 | 服务端进程 3 秒后仍存在 | 服务端同步结束 | 服务端同步结束 |

在公网官方服务端上，往返约 800 ms、双向各 10% 丢包、乱序和 1% 重复包同时存在时，带空格输入、退格纠错、中文/emoji、长行、窗口缩放和清屏后重画全部通过。另用逐字节记录程序核对了完整输入序列，发送与接收完全一致。追加的 180×70 大画面测试完整重建了至少 40 行、每行 140 字节的确定性低压缩率内容，并从加密数据报中确认服务端确实发出同一指令的多个分片；12 秒双向静默黑洞后，客户端会显示最后联系时间，同一会话恢复并继续执行命令后提示自动清除。

本轮追加对照后又补齐了 ECN 拥塞标记与惩罚、官方的 100 ms 延迟确认和 10 秒重传分段、随机 chaff，以及 `experimental` 预测模式。发布前仍应在目标 Windows 页面完成一次人工操作验收，并在真实公网 IPv6 上补一次接近 MTU 的大输出与静默黑洞测试。

## 审计范围与完成标准

范围：

- SSH 启动与 `MOSH CONNECT` 交接
- AES-128-OCB3、nonce、方向位、完整性校验与重放处理
- UDP 数据报布局、MTU 与分片
- SSP 状态同步、ACK、重传、状态裁剪与关闭流程
- 漫游、换网与端口跳转
- RTT、RTO、心跳与断网超时
- 终端状态模型与 HostBytes 画面
- 本地预测、确认、下划线和高延迟行为

完成标准：

- 每项判断有论文、RFC、官方源码或实际运行证据；
- 明确区分“兼容”“简化”“缺陷”“待实测”；
- 只有源码必然推出或实际复现的问题才标成缺陷；
- 审计结论完成后，所有已证实且可在本轮验证的缺陷都应修复并重新运行测试。

## 版本与一手资料

### 固定版本

| 对象 | 固定版本 |
|---|---|
| 官方 Mosh | `mobile-shell/mosh` `master`，提交 [`decd9b705eb81626f694335b8d5940538beb06da`](https://github.com/mobile-shell/mosh/commit/decd9b705eb81626f694335b8d5940538beb06da)，提交时间 2026-03-22；`git ls-remote` 已确认与远端 `master` 一致 |
| MoshCatty | 本地提交 `8cb4f53987e584ad9130d6e2edd225ac1f1b01a3`，提交时间 2026-07-15，标题 `fix(client): reconstruct remote states under high latency` |
| 实际互通环境 | Ubuntu 24.04.4 LTS、Linux 6.8、系统 `mosh 1.4.0`；另用 macOS Homebrew `mosh-server 1.4.0` 做时间戳与心跳定向复现 |
| 协议论文 | [Mosh: An Interactive Remote Shell for Mobile Clients](https://mosh.org/mosh-paper.pdf)，6 页，2012 |
| 加密规范 | [RFC 7253: The OCB Authenticated-Encryption Algorithm](https://www.rfc-editor.org/rfc/rfc7253) |
| 问题上下文 | [Netcatty #2121](https://github.com/binaricat/Netcatty/issues/2121) |

### 官方源码索引

- 启动器与握手：[scripts/mosh.pl](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/scripts/mosh.pl#L353-L465)、[mosh-server.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-server.cc#L430-L473)、[mosh-client.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-client.cc#L145-L194)
- 加密：[crypto.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/crypto/crypto.cc#L64-L281)、[crypto.h](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/crypto/crypto.h#L48-L153)
- UDP、时间戳、漫游与 MTU：[network.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/network.cc#L68-L111)、[network.cc 接收路径](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/network.cc#L427-L539)、[network.h](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/network.h#L105-L149)
- SSP 发送与定时：[transportsender-impl.h](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h#L45-L185)、[ACK 与重传](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h#L212-L369)
- SSP 接收：[networktransport-impl.h](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/networktransport-impl.h#L35-L180)
- 分片：[transportfragment.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportfragment.cc#L54-L194)
- protobuf：[transportinstruction.proto](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/protobufs/transportinstruction.proto)、[hostinput.proto](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/protobufs/hostinput.proto)、[userinput.proto](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/protobufs/userinput.proto)
- 终端完整状态与 HostBytes：[completeterminal.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/statesync/completeterminal.cc#L44-L175)、[terminaldisplay.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/terminal/terminaldisplay.cc#L40-L321)
- 预测：[terminaloverlay.h](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/terminaloverlay.h#L179-L311)、[terminaloverlay.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/terminaloverlay.cc#L431-L873)、[stmclient.cc](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/stmclient.cc#L275-L430)

### MoshCatty 源码索引

- [README 与启动边界](../README.md#what-is-moshcatty)
- [src/crypto.rs](../src/crypto.rs)
- [src/fragment.rs](../src/fragment.rs)
- [src/pb.rs](../src/pb.rs)
- [src/transport.rs](../src/transport.rs)
- [src/client.rs](../src/client.rs)
- [src/terminal.rs](../src/terminal.rs)
- [src/ansi_apply.rs](../src/ansi_apply.rs)
- [src/framebuffer.rs](../src/framebuffer.rs)
- [src/prediction.rs](../src/prediction.rs)
- [src/bin/mosh_client.rs](../src/bin/mosh_client.rs)
- [docs/prediction.md](prediction.md)

## 分类口径

| 分类 | 含义 |
|---|---|
| **明确兼容** | 线格式或状态语义与官方实现一致，并有测试、向量或实际互通证据 |
| **有意简化** | 源码或文档明确选择更小的实现；不妨碍基本互通，但功能、性能或安全加固不等同官方实现 |
| **已证实缺陷** | 与官方协议或核心行为矛盾，且可由源码必然推出或已经实际复现 |
| **需实网验证** | 已确认存在实现差异，但是否在真实网络触发、影响多大，依赖 MTU、丢包、重排、NAT、操作系统或换网过程 |

## 详细对照矩阵

### 1. SSH 启动与 `MOSH CONNECT`

| 项目 | 官方行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| SSH 启动 | 论文第 2 页和 `mosh.pl`：SSH 启动非特权 `mosh-server`，服务端分配 UDP 端口和随机 128 位密钥，输出 `MOSH CONNECT <port> <key>` 后 SSH 退出 | MoshCatty 只接受 `host + port + MOSH_KEY`，不启动 SSH；README 明确把 SSH bootstrap 交给 Netcatty/调用方 | **有意简化** | 这是模块边界，不是 UDP 协议缺失。MoshCatty 单独无法修复“没有收到 MOSH CONNECT” |
| 握手行解析 | 官方启动器只接受端口和 22 字符标准 Base64 密钥，并从 SSH 输出中提取远端地址 | MoshCatty 不解析该行；只在本地进程启动后读取环境变量 | **有意简化** | #2121 中 `no MOSH CONNECT` 发生在 MoshCatty 启动前；此时服务端看不到 UDP 包是预期结果，不能据此归因到 OCB/SSP |
| 首个 UDP 包 | 官方客户端拿到端口和密钥后开始 UDP；服务端以第一个通过认证的新包确定客户端地址 | `Client::dial` 创建 UDP socket、强制一次 `tick` 并立即发送；本地官方服务端实测可以收到并附着客户端 | **明确兼容** | 一旦启动交接完成，当前实现会真正发 UDP，不存在“已进入客户端但从不发首包”的普遍问题 |
| 地址与 locale 交接 | `mosh.pl` 处理 `SSH_CONNECTION`、IPv4/IPv6、locale、远端命令、颜色数和可选端口范围 | MoshCatty CLI 不做这些；依赖上层传入地址，并固定提供网络客户端能力 | **有意简化** | 需要由 Netcatty bootstrap 保证目标地址和服务端 locale 正确 |
| 密钥环境变量 | 官方 `mosh-client` 读入后立即 `unsetenv("MOSH_KEY")` | MoshCatty 读入后立即清除 | **明确兼容** | 密钥不再继续留在子进程环境中 |

### 2. AES-128-OCB3、nonce 与重放

| 项目 | 官方/RFC 行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| 算法参数 | RFC 7253 的 `AEAD_AES_128_OCB_TAGLEN128`：16 字节 AES key、16 字节 tag；Mosh 使用 12 字节 nonce、空 AAD | 16 字节 key、16 字节 tag、12 字节 nonce、空 AAD | **明确兼容** | RFC Appendix A 空 AAD 向量、不同长度向量和 mosh-go 双方向向量全部在单元测试中通过 |
| nonce 形状 | 官方 `Nonce(uint64)` 为 `00 00 00 00 || dir_seq(大端)` | `nonce_for` 完全相同 | **明确兼容** | 官方 1.4.0 真实服务端已能解密并接受当前客户端数据报 |
| 方向位与包序列 | 64 位序列字最高位表示 `TO_CLIENT`，低 63 位为序列；进程内从 0 全局递增；收到反方向包必须拒绝 | 方向、收包检查和进程级全局序列均与官方一致 | **明确兼容** | 定向测试覆盖同进程多 Transport 不复用序列、双方向和反方向拒绝，可阻止把发送方向的数据报反射回来重放 |
| tag 校验 | OCB 在输出明文前验证 128 位 tag | 解密后以常量时间比较 tag，不匹配返回失败 | **明确兼容** | 篡改测试通过；错误包不会进入 SSP 解码 |
| 乱序与重放 | 官方会解密旧序列包并把 payload 交给幂等 SSP，但旧序列不能更新 RTT 或漫游目标 | 当前接受未见过的乱序序列；最近 512 个相同序列会直接丢弃；旧序列不更新 RTT | **明确兼容** | 512 项缓存是额外去重，不破坏官方状态幂等语义；超过窗口的旧包仍由状态号/分片号去重 |
| 会话密钥用量 | 官方每个方向共享同一 key；累计加密达到 `2^47` 个块即终止，避免 OCB 安全界限下降 | 当前同样在达到边界前停止会话 | **明确兼容** | 单元测试覆盖临界点，避免同一密钥跨越规定上限 |
| Base64 密钥格式 | 官方严格要求 22 字符、规范编码，拒绝非规范尾位 | 当前同样只接受规范的 22 字符编码 | **明确兼容** | 空白、填充符、非规范尾位和错误长度都会拒绝 |

### 3. UDP 数据报布局、MTU 与分片载荷

完整数据报布局为：

```text
8 字节 dir_seq（明文，同时构造 nonce）
+ OCB 密文：
    2 字节发送时间戳
    2 字节时间戳回声
    8 字节 instruction_id
    2 字节 final + fragment_num
    zlib(TransportInstruction protobuf) 的一个分片
+ 16 字节 OCB tag
```

| 项目 | 官方行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| 字段顺序与大小端 | `dir_seq`、两个 16 位时间戳、10 字节分片头和密文/tag 顺序固定，整数为大端 | 编解码顺序与大小端完全一致 | **明确兼容** | 真实官方服务端互通已覆盖该组合，不只是同实现自测 |
| IPv4 1280 MTU | 官方 IPv4：`1280 - 20(IP) - 8(UDP) - 12(连接头) - 16(OCB) - 10(分片头) = 1214` 字节分片载荷 | 固定 `MAX_FRAGMENT_PAYLOAD = 1214` | **明确兼容** | 与官方 IPv4 默认值精确一致，适合当前用户提供的公网 IPv4 测试机 |
| IPv6 1280 MTU | 官方保守扣除 40 字节 IPv6、16 字节扩展头和 8 字节 UDP，分片载荷为 1178；遇 `EMSGSIZE` 退到 500 字节应用 MTU | IPv6 使用 1178 字节载荷；系统报告数据报过大时退到 462 字节分片载荷 | **明确兼容** | 单元测试确认最终数据报不超过 IPv6 最小 MTU；Linux、macOS 与 Windows 的过大错误均有对应处理 |
| 接收上限 | 官方单个加密数据报上限 2048 字节 | 接收缓冲区约 16 KiB，单次重组和解压另有限额 | **明确兼容** | 能接收官方合法数据报；更大的接收缓冲本身不改变线协议 |
| ECN | 官方把外发 UDP 标为 ECT(0)，读取 CE，并把回声时间戳减 500 ms 让对端降速 | 当前在 Linux、macOS 和 Windows 可用时执行相同行为，系统不支持辅助数据接口时保留普通 UDP 回退 | **明确兼容** | macOS 和 Ubuntu 的 IPv4/IPv6 回环实测 ECT(0)/CE 均可收发；时间戳惩罚有定向测试，接收缓冲也按系统批量合并上限配置，Windows 目标构建通过 |

### 4. SSP 状态、ACK、重传与分片

| 项目 | 官方行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| 协议版本与 protobuf | 版本 2；字段 1–7 依次为版本、old/new/ack/throwaway、diff、chaff | 手写 codec 使用同一字段号、类型和 proto2 varint/bytes 规则 | **明确兼容** | Host/User/Transport 消息均有往返测试，官方服务端实测接受 |
| 版本检查 | 官方要求 `protocol_version == 2`，否则终止当前协议处理 | 当前严格要求版本 2 | **明确兼容** | 缺失、零或其他版本都会拒绝，单元测试已覆盖 |
| zlib 与分片头 | protobuf 先 zlib，再加 8 字节 ID 和 15 位序号/1 位 final | 相同 | **明确兼容** | 大数据、反序分片和多分片往返测试通过 |
| 接收状态幂等性 | 先处理 ACK；拒绝已有 `new_num`；必须持有 `old_num`；从基线克隆、应用 diff；按 `throwaway_num` 裁剪 | `Transport` 保存编号，`TerminalView` 保存对应完整 framebuffer + pen + echo_ack 快照，处理顺序与官方一致 | **明确兼容** | 当前提交解决了高延迟下多个新状态共同引用旧基线时的重复画面；这正是 #2121 字符重复的关键状态模型条件 |
| 乱序状态 | 官方可按 `old_num` 在已保存分支上应用，并按编号排序；旧的完整状态不能倒退当前画面 | 当前保存分支快照，旧状态可作为未来基线但不回退已显示画面 | **明确兼容** | 覆盖高 RTT 下 `0→1`、`0→2` 并行分支 |
| ACK 与 throwaway | ACK 只向前；发送端用 ACK 裁剪；接收端在克隆旧基线后再执行 throwaway | 当前相同 | **明确兼容** | 避免 ACK 已接受但本地基线已被提前删除 |
| 出站状态窗口 | 官方保留多份发送状态，目标约每 RTT 两帧，允许多个未确认状态在途 | 当前保留最多 32 个在途状态，并从最近已确认基线构造累积输入；窗口压缩后会基于新确认重建剩余输入 | **明确兼容** | 500 ms RTT 下两个按键到达间隔从修复前 580 ms 降到 269 ms，官方客户端为 251 ms；单向确认丢失超过 32 个状态及随后部分恢复也有回归测试 |
| 空 ACK / 心跳状态号 | 官方每 3 秒发送空 ACK 时仍创建一个新的 `new_num`，因此接收方把它记作新远端状态并刷新活动时间 | 当前每 3 秒心跳同样创建新状态号 | **明确兼容** | 5 秒服务端空闲限制的定向测试不再误退出；公网空闲 10 秒上行包数为 4 |
| ACK 延迟与空闲包量 | 官方数据 ACK 通常延迟不超过约 100 ms，空心跳 3 秒 | 当前同样使用 100 ms 延迟确认和 3 秒空心跳；8 ms 主循环只负责调度，不会提前确认 | **明确兼容** | 公网官方服务端实测空闲 10 秒上行 4 包，与官方客户端一致；持续入站流量仍可能影响某一次主循环的具体调度时间 |
| 重传调度 | 官方按 `SRTT/2`（20–250 ms）发送新帧，活动期按 RTO + 100 ms ACK delay 重传；连续 10 秒没有新的完整远端状态后退到 3 秒节奏 | 当前使用相同的发送、延迟确认和两段重传节奏，并保留多份在途状态；残缺分片和旧分支不会延长活动重传阶段 | **明确兼容** | 100 ms、10 秒和 3 秒边界均有定向计时测试；连续输入和空闲包量实测接近官方 |
| 随机 chaff | 官方每条 TransportInstruction 加 0–16 个随机字节，降低固定长度特征 | 当前从系统随机源生成相同范围的 chaff；随机源不可用时安全退回空值 | **明确兼容** | 字段号和长度范围与官方一致，编码和公开协议测试通过 |
| instruction_id | 官方只要发送一条新的线指令就使用新的 ID | 当前每条线指令也分配独立递增 ID | **明确兼容** | 单元测试覆盖同一状态的后续 ACK/重发不会复用旧分片组 ID |
| 分片重组 | 官方同一 ID 可乱序到达；遇到任何不同 ID 都放弃当前不完整指令并切换，不按 ID 大小提前过滤 | 当前执行相同切换规则；完整的旧编号指令仍交给 SSP 状态号判断是否有用 | **明确兼容** | 定向测试覆盖同 ID 乱序、新旧 ID 切换，以及较新完整状态先到、较旧完整分支后到的路径 |
| 状态队列上限 | 官方接收队列超过约 1024 后开始节流，之后每 15 秒最多再接收一个状态，不丢弃仍可能被引用的中间状态 | 当前使用相同的 1024 起始窗口和 15 秒放行节奏，终端快照另有 128 MiB 总量与尺寸限制 | **明确兼容** | 定向测试覆盖首次越界、节流拒绝、15 秒后的再次放行和至少 300 个并行状态；大画面仍由独立内存上限保护 |
| 关闭握手 | 官方用 `new_num = UINT64_MAX` 做双向关闭确认，并有限次重试 | 当前在 EOF、信号和远端关闭时执行同样的双向关闭确认 | **明确兼容** | 公网官方服务端实测客户端退出后服务端同步结束；单元测试覆盖双方确认 |

### 5. 漫游与换网

| 项目 | 官方行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| 服务端重新定位客户端 | 官方服务端只用通过认证、且序列号不旧的数据报更新客户端 IP/端口；论文第 2 页也以此定义漫游 | 当前客户端持续使用递增序列和同一会话 key 向服务端发包 | **明确兼容** | 官方服务端具备重定位逻辑，当前客户端的线数据满足其条件 |
| 客户端 socket 与端口跳转 | 官方客户端无往返成功一段时间后换本地 UDP 端口，并保留旧 socket 最多 60 秒 | 当前无成功往返 10 秒后更换本地端口，最多保留 10 个 socket，并在恢复后裁掉旧 socket | **明确兼容** | 公网屏蔽原通道后两次测试均自动恢复，约 9–10 秒 |
| 临时 UDP 错误 | 官方记录发送错误但继续循环，保留恢复机会 | 当前忽略可恢复的 UDP 收发错误，并在数据报过大时降低分片大小 | **明确兼容** | 原路径失效不会再直接结束会话 |
| 长断网后恢复 | 官方已建立会话默认不会因客户端 60 秒无包而退出；服务端网络超时只有显式配置时生效 | 当前只对首次连接保留 15 秒限制，已建立会话可持续等待恢复 | **明确兼容** | 公网完全断网 65 秒后客户端仍在线，恢复网络后同一会话继续工作 |

### 6. RTT、RTO、心跳与超时

| 项目 | 官方/论文行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| 时间戳字段 | 每包带 16 位发送时间；可选回声以 `0xFFFF` 表示“无回声” | 字段和 sentinel 相同 | **明确兼容** | 大小端和回绕计算均与官方线格式一致 |
| 回声校正 | 论文第 2 页和 `network.cc`：回送时间戳前，加上该时间戳在本地等待的时长，使发送方算出的 RTT 不包含对端等待发包的时间 | 当前同样加上本地持有时间，并只使用一次回声样本 | **明确兼容** | 定向单元测试确认 300 ms 本地持有不会被算进 RTT |
| RTT 过滤 | 官方忽略 `R >= 5000 ms`；首次样本设 `SRTT=R, RTTVAR=R/2`，随后使用 1/8 与 1/4 平滑 | 过滤边界和平滑公式相同 | **明确兼容** | 5 秒及以上异常样本不会污染公开 SRTT 和预测显示阈值 |
| RTO 范围 | 官方 `SRTT + 4*RTTVAR`，钳在 50–1000 ms；首次测量前为 1000 ms | 当前公式、边界和 1000 ms 初始值相同 | **明确兼容** | 首轮与测量后的重传节奏均有定向测试 |
| 线时间戳时钟 | 官方优先使用单调时钟，避免系统时间回拨 | 当前线时间戳和内部超时都使用单调时钟 | **明确兼容** | 系统时间校正不会污染 RTT 样本 |
| 客户端永久超时 | 官方仅在最初连接超过约 15 秒时进入关闭；已建立会话可持续等待恢复 | 当前行为相同 | **明确兼容** | 65 秒完全断网恢复测试通过 |
| 本地轮询延迟 | 官方基于 `select` 事件循环 | 当前 socket 为非阻塞，主循环每 8 ms 调度发送，外层有 2 ms 短暂让步 | **有意简化** | 不再有原先 20 ms 阻塞读；持续大量入站包仍可能让单轮处理变长，Windows ConPTY 需继续测按键到首像素时间 |
| 连接与断线提示 | 官方首次等待 250 ms 后显示连接提示；超过 6.5 秒没有收到最新完整远端状态时显示最后联系时间；仅上行超过 10 秒无确认时显示最后回复时间 | 当前使用相同计时基准、阈值、优先顺序和时间格式，在同一 framebuffer 顶行绘制并在恢复后还原原画面 | **明确兼容** | 公网官方服务端 12 秒双向静默测试确认提示出现、会话不退出、恢复后提示清除；残缺分片和迟到旧分支不会错误刷新联系时间 |
| 本地快捷退出 | 官方默认 `Ctrl-^ .` 退出、`Ctrl-^ ^` 输入字面控制符，并支持 `MOSH_ESCAPE_KEY` | 当前支持相同退出、字面输入、可打印键行首限制和环境开关；刚启动时可打印键也不会被误认作行首命令；不在 ConPTY 子进程中实现 Unix 作业暂停 | **明确兼容（暂停除外）** | 跨平台退出不会把快捷键泄漏到远端；暂停是有意省略，避免停死宿主终端 |

### 7. 终端状态模型

| 项目 | 官方行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| 终端所有权 | 官方服务端运行完整 UTF-8 终端模拟器；客户端接收从旧 framebuffer 到新 framebuffer 的 HostBytes 画面差异 | 当前不重跑远端应用的完整终端语义，而是解释官方 Display 生成的 HostBytes，并构建本地 framebuffer | **明确兼容** | 这是 Mosh 协议本身的分工，不需要在客户端再实现一个任意 VTE |
| HostMessage | `HostBytes=扩展 2/字段 4`、`Resize=扩展 3/字段 5/6`、`EchoAck=扩展 7/字段 8` | 字段完全一致，未知字段可跳过 | **明确兼容** | 官方服务端的画面、尺寸和 late ACK 均已在真实连接中解码 |
| 初始画面尺寸 | 官方客户端用本地终端的实际尺寸初始化远端完整状态，并立即发送同尺寸 resize | 当前用调用方传入的实际尺寸初始化远端完整状态 | **明确兼容** | 公网官方服务端以 120×40 启动的测试通过，单元测试也直接检查初始状态 |
| 并行 SSP 分支 | 官方每个远端状态包含完整 `Complete`；diff 必须应用到声明的旧状态 | 当前 `RemoteTerminalState` 保存 framebuffer、ANSI pen/carry 和 echo_ack，按状态号克隆 | **明确兼容** | 这是当前提交最重要的正确性改动，避免把两个都基于状态 0 的画面差异连续叠加造成重复字符 |
| HostBytes 指令语义 | 官方 Display 可能输出 Bell、OSC 0/1/2/8/52、SGR、CUP、CR/LF/BS、ECH/EL、滚屏区、光标/鼠标/粘贴模式 | 当前解析器覆盖这些官方 Display 生成序列，并保持跨分片 CSI/OSC/UTF-8 carry | **明确兼容** | 单元测试覆盖颜色、宽字符、组合字符、滚屏区、超链接、剪贴板、标题和分段序列；官方服务端 shell 画面实测可显示 |
| 本地重绘 | 官方客户端用同一 framebuffer 加预测 overlay，再生成最小终端 diff | 当前同样先更新 host framebuffer、确认预测、叠加 overlay，再对 last_shown 生成单一 diff | **明确兼容** | 消除了“原始 HostBytes 与预测字符同时直写”导致的双字符路径 |
| 剪贴板清空 | 官方检测到剪贴板从有值变为空值时，仍发送一个空 OSC 52 来清除本地值 | 当前同样输出空 OSC 52 | **明确兼容** | 最小状态转移单元测试确认清空指令存在；标题和图标的 `None` 语义保持不变 |
| bit-identical 输出 | 官方根据 terminfo 能力选择 ECH/BCE/smcup/rmcup 等 | 当前使用自己的 ANSI diff 和固定 xterm 风格 alternate-screen 序列 | **有意简化** | 目标是单文件跨平台和 ConPTY；单元格语义对齐，但输出字节不保证与官方逐字节相同 |
| 安全上限 | 官方主要依赖内部队列；当前版本没有同样的 1000×1000/100k-cell/128 MiB 限额组合 | 当前对尺寸、状态数和总快照内存设置硬上限 | **有意简化** | 属于防御性限制；超大终端会主动报错而不是继续分配 |

### 8. 本地预测与下划线

| 项目 | 官方/论文行为 | 当前 MoshCatty | 分类 | 判断与影响 |
|---|---|---|---|---|
| 基本模型 | 论文第 3 页：本地预测必须可撤销；当前 epoch 先 tentative，至少一个预测得到服务器确认后才展示该 epoch | `prediction_epoch=1`、`confirmed_epoch=0`，按 credited-correct 确认或 kill epoch | **明确兼容** | 初始预测不一定立即显示是官方安全策略，不等同“预测完全没运行” |
| early ACK 与 late ACK | SSP ACK 只表示收到输入；服务端约 50 ms 后通过 `EchoAck` 证明终端已实际处理，Pending 预测必须看 late ACK | 当前分别保存 `acked_by_remote` 和 `echo_ack`，Pending 只由 late ACK 解除 | **明确兼容** | 避免网络早 ACK 过早把错误预测当正确 |
| 自适应显示 | 官方首次 RTT 测量前以 250 ms 启动，之后以 `send_interval=ceil(SRTT/2)` 判断：高于 30 ms 开启、降到 20 ms 且无活跃预测时关闭 | 当前首次同样使用 250 ms，之后向上取整 SRTT/2 并用相同 20/30 ms 阈值 | **明确兼容** | 定向测试覆盖 61 ms 与 161 ms 的奇数边界，避免少 1 ms 导致预测或下划线延迟开启 |
| 下划线 | 官方 80/50 ms hysteresis 控制 flagging；Really big glitch 也会强制下划线 | 当前阈值和 glitch 规则相同 | **明确兼容** | 大致相当于 RTT 超过 160 ms 时未确认预测应出现下划线；最终视觉仍需目标高延迟 Windows 环境核验 |
| 单一画面路径 | 官方把 overlay 应用到 framebuffer 后只输出一次差异 | 当前 HostBytes → host_fb → Confirm → Overlay → Diff，预测字符不再另一路直写 | **明确兼容** | 这是防止 `ls` 显示为 `lls` 的必要结构条件；现有回归测试明确检查无双写 |
| 按键规则 | 官方对普通单宽字符、DEL、CR、左右箭头、末列、插入/覆盖、未知单元和错误 epoch 有细致规则 | 当前逐项移植主要规则，并有大量边界测试 | **明确兼容** | 主路径对齐；宽字符/组合字符选择不预测而进入 tentative，优先正确性 |
| bulk paste | 官方超过 100 字节重置预测 | 当前相同 | **明确兼容** | 避免大段粘贴生成错误本地画面 |
| 预测生命周期 | 官方以帧确认和 glitch 机制管理，没有额外墙钟过期；长时间未确认会强制显示并加下划线 | 当前相同，只由 late ACK / 远端画面验证清理预测；隐藏状态也会持续检查等待时间 | **明确兼容** | 定向测试确认低延迟隐藏预测跨过 250 ms 会显示、跨过 5 秒会加下划线，超过 15 秒仍保留未确认输入 |
| 单元历史模型 | 官方每个条件单元保留 `original_contents` 历史，避免快速重复改成同一个值时把旧预测误算成新证据 | 当前保留同样的坐标历史，并在插入、覆盖、退格和末行重画时延续 | **明确兼容** | 定向回归确认重复覆盖不会错误确认尚未证明的预测批次 |
| 多组预测光标 | 官方可同时保留多个 epoch 的预测光标；新的 tentative 光标不能遮掉上一组已确认光标；预测组失败时追加当前远端光标作为确认锚点 | 当前同样按 epoch 保留和依次应用预测光标，失败时保留远端位置，并按 late ACK 独立清理 | **明确兼容** | 定向回归确认回车后的新光标尚未证明时，旧的已确认光标仍留在正确位置，失败后的下一次确认也不会误判旧光标 |
| 模式集合 | 官方支持 adaptive/always/never/experimental | 当前支持相同四种模式；experimental 会立即显示预测，并只丢弃单个错误预测 | **明确兼容** | 环境变量解析、即时显示和错误单元隔离均有回归测试 |
| 目标环境体验 | 官方规则和本地单元测试已对齐 | Ubuntu 隔离网络已经覆盖最高 800 ms RTT、丢包、重排和重复包；尚缺 Windows + ConPTY 的真实渲染链路 | **需实网验证** | 网络和协议层回归已经通过，但 #2121 的最终主观体验仍应在目标 Windows 页面完成一次录像与按键时延验收 |

## 已执行验证

### 1. 官方资料核对

- Mosh 论文 6 页全部抽取并渲染；人工检查了协议、RTT 和预测所在页面，未只依赖文本提取。
- RFC 7253 只用于 OCB3 参数、nonce 唯一性和 Appendix A 向量，不把它错误扩展为 Mosh 的状态同步规范。
- 官方仓库用 `git ls-remote` 重新确认了 `master` 的提交号，避免拿陈旧 clone 做比较。

### 2. 自动测试和构建

- `cargo test --all-targets`：290 项通过、0 项失败；四个需要真实服务端的 live test 默认 ignored，并已再次从本机连接该 Ubuntu 官方服务端逐项执行通过；
- `cargo check --target x86_64-pc-windows-gnu --all-targets` 通过；Ubuntu 上用项目最低支持版本 Rust 1.75 从零编译并跑完全部测试；
- `cargo build --release` 成功；
- 新增回归覆盖时间戳、心跳、并行输入、反向确认长期丢失后的持续输入、真实初始尺寸、带最终画面的双向关闭、剪贴板清空、严格版本和密钥格式、IPv6 分片上限、加密用量边界；
- 官方服务端成功认证本地构建的客户端，双方能交换状态、输入和画面。
- 最新改动另在本机 Ubuntu 虚拟机的官方 Mosh 1.3.2 服务端重跑四项真实会话，输入去重、缩放、大画面分片和 12 秒静默恢复全部通过；与公网 Ubuntu 24.04 / Mosh 1.4.0 的既有结果形成跨版本互通证据。

### 3. 修复前的隔离网络基线

修复前在 Ubuntu 隔离链路中跑了四种网络条件，每种执行 6 类操作：带空格输入、退格纠错、中文/emoji、200 字节长行、窗口缩放、清屏后重画。24 组都收到了正确结果摘要，没有缺失或错误结果。

这套旧脚本在读到第一个正确摘要后就进入下一项，因此它不能单独证明“摘要只出现一次”。“并行状态不能重复画面”由专门的单元测试证明；修复后的公网测试又增加了逐字节输入核对，避免把旧脚本的证据范围说得过大。

### 4. 本地 MoshCatty 到公网 Ubuntu 官方服务端

最终拓扑与真实产品一致：本地 macOS 运行 release 版 MoshCatty，Ubuntu 机器只运行系统官方 `mosh-server 1.4.0`。

| 场景 | 结果 |
|---|---|
| 120×40 首次连接、执行命令、退出 | 画面和尺寸正确，服务端同步结束 |
| 空闲 10 秒 | 上行 4 包，与官方客户端一致 |
| 完全断网 65 秒后恢复 | 客户端始终存活，同一会话恢复 |
| 屏蔽原 UDP 路径 | 自动更换本地端口，两次约 9–10 秒恢复 |
| 500 ms RTT 连续输入 | 两个按键到达间隔 269 ms；官方客户端 251 ms；修复前 580 ms |
| 约 800 ms RTT、双向 10% 丢包、乱序、1% 重复 | 6 类交互全部得到正确结果 |
| 同一恶劣网络下逐字节记录输入 | 完整发送序列与服务端收到的字节完全一致 |
| 180×70 大画面、50 行 × 140 字节快速输出 | 最终画面至少 40 行逐行完整，结束标记正确；解密抓包确认同一服务端指令包含多个分片 |
| 双向静默丢包 12 秒后恢复 | 客户端显示最后联系时间且不中止；恢复后同一会话继续执行命令、提示清除并完成关闭确认 |
| IPv6、实际 MTU 1280 | 隔离链路两端分别运行 MoshCatty 和官方服务端，连接、中文/emoji、退格和退出均正确 |
| IPv6 MTU 1280 + 约 800 ms RTT + 双向 10% 丢包/乱序/重复 | 中文/emoji、退格和短命令连续三次得到正确结果；该脚本不用于证明输出只出现一次 |

恶劣网络交互包括空格、退格、中文/emoji、长行、真实窗口缩放和清屏后重画。测试脚本最初把实际 ESC 控制键误当成命令文字发送，导致最后一项失败；改为与人工输入一致的反斜杠文本后，同一固定随机种子的网络条件稳定通过。这个失败属于测试方法错误，未计入产品缺陷。

### 5. 修复前后差分

| 场景 | 修复前 | 修复后 | 官方客户端 1.4.0 |
|---|---|---|---|
| 空闲 10 秒 | 上行 154 包 | 上行 4 包 | 上行 4 包 |
| 500 ms RTT 连续输入 | 580 ms | 269 ms | 251 ms |
| 完全断网 65 秒 | 约 59.271 秒退出 | 恢复后继续 | 恢复后继续 |
| 原路径失效 | 报错退出 | 约 9–10 秒换端口恢复 | 约 8.020 秒恢复 |
| 退出 | 服务端进程 3 秒后仍存在 | 服务端同步结束 | 服务端同步结束 |

修复前的关闭测试只证明 `mosh-server` 进程仍存在，不能进一步断言其子 shell 的精确状态，因此本报告只保留进程层面的结论。

### 6. 定向最小复现和修复验证

- 时间戳：修复前把约 300 ms 的本地持有时间算进服务端 RTT；修复后单元测试确认这段等待被扣除；
- 心跳：修复前官方服务端设置 5 秒空闲限制会误退出；修复后每 3 秒生成新状态，空闲测试与官方客户端一致；
- 初始尺寸：修复前 120×40 会被保存为 80×24；修复后首次状态即为 120×40；
- 剪贴板：修复前从有值变为空值生成 0 字节；修复后会发出明确清空序列；
- 安全边界：错误版本、非规范密钥和达到加密用量上限都会拒绝或结束会话。

## 与 Netcatty #2121 的对应关系

| #2121 现象 | 本审计能确认的边界 |
|---|---|
| `no MOSH CONNECT` | 这是 SSH bootstrap/输出解析阶段，发生在 MoshCatty 网络客户端启动前。Netcatty 当前桥接代码的分段、无换行和退出收尾测试全部通过；又按真实产品路径连续启动 30 次，30 次都完成 SSH 交接、进入远端命令行并正常退出，内部连接行没有泄漏。当前未复现剩余竞争，但仍需在报告问题的 Windows 机器上做最终确认 |
| 偶发连接后又断 | 已修复建立会话后 60 秒退出、无效心跳和临时网络错误直接退出；公网 65 秒完全断网和旧路径失效都能恢复。SSH 启动阶段的偶发失败属于上层握手桥，应继续由 Netcatty 单独覆盖 |
| 高延迟下仍像 SSH | 已修正时间估算和连续输入发送；500 ms RTT 下结果从 580 ms 降到 269 ms，接近官方客户端 251 ms |
| 字符重复 | 完整状态重建和单一画面路径继续保留；恶劣公网测试和逐字节核对均通过，未发现重复回归。目标 Windows/ConPTY 仍应保留一次端到端验收 |
| 偶尔有下划线、但不稳定 | 20/30 和 50/80 ms 阈值与官方一致；late ACK 语义也正确。剩余体验必须结合真实客户端 SRTT、预测 epoch 和服务端 echo_ack 采样判断，不能只看最终截图 |

## 后续发布检查

本轮已完成时间戳、心跳、长断网、端口切换、临时网络错误、连续输入、初始尺寸、会话关闭、剪贴板、协议版本、分片编号、IPv6 分片、安全用量和密钥处理的修复与验证。

正式发布前还建议完成两项产品层验收：

1. 在 Windows ConPTY 中实际输入、缩放、清屏和切换网络，观察最终画面与按键手感；
2. 在真实公网 IPv6 上跑接近 MTU 的大输出和静默黑洞场景，补足当前隔离链路短交互没有覆盖的范围。

## 最终判断

MoshCatty 的核心方向可以保留，不需要大范围重写。对照官方实现后发现的核心差异已经修复，并在“本地客户端连接公网 Ubuntu 官方服务端”的真实拓扑下通过了高延迟、丢包、乱序、重复包、长断网和换端口测试。

从协议互通和 Linux/macOS 实测看，本轮结果已经达到可交付水平。剩余风险集中在 Windows 最终显示链路，以及真实公网 IPv6 的大输出与静默黑洞；不再是本次已经复现的核心连接、输入或拥塞缺陷。
