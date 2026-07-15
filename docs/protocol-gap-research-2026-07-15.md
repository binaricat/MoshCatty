# MoshCatty 协议差异补充研究（2026-07-15）

## 后续修复状态

本报告记录的是以 `f2bb6ad` 为固定点的发现。随后已经逐项落地并验证：

| 发现 | 当前状态 |
|---|---|
| 首个 UDP 状态没有窗口尺寸 | 已修复；线数据单元测试和 Ubuntu 官方服务端启动时 `stty size` 均直接得到请求尺寸 |
| 1 MiB 接收上限低于官方 4 MiB | 已修复；收发、重组和解压都执行官方 4 MiB 边界，分片编号不会回绕，系统 UDP 接收队列也同步扩大；Ubuntu 官方 1.3.2 服务端实测单条压缩指令 1,446,920 字节并完整恢复 |
| 首次连接超时直接结束 | 已修复；现在先完成官方关闭流程，定向测试覆盖单向路径 |
| ACK 到达时间误作成功往返时间 | 已修复；保存所有发送状态（含空心跳）的原始时间，并拒绝从未发送或已经裁掉的状态确认 |
| 缺少崩溃转储和密钥清理保护 | Unix 已按官方禁用 core dump；AES key schedule、OCB 派生掩码和临时密钥均在销毁时清理；Windows 转储策略仍需在正式安装包上验收 |
| 一次轮询会无限收空 UDP 队列 | 已修复；每轮总收包量有硬上限，并公平照顾漫游前后的路径；洪泛回归和官方超大分片画面同时通过 |
| SSH 与 UDP 可能选中同一域名的不同地址 | 已修复；Netcatty 交接 SSH 实际地址并保留原主机名，MoshCatty 尝试全部候选，认证后锁定有效路径 |
| 缺少 prospective resend 选择 | 已修复；现在按官方的 RTO+ACK delay 时窗选择最近发送基线，并在小于 1000 字节、增量差不足 100 字节时优先从已确认基线可靠重发 |

因此，本报告中各节的“当前代码”描述应理解为修复前证据；最终发布判断以主审计报告和最新测试结果为准。

## 结论

本报告专门复查现有 [协议审计](protocol-audit-2026-07-15.md) 没有承认的差异。固定对照版本为：

- 官方 Mosh：`mobile-shell/mosh` `decd9b705eb81626f694335b8d5940538beb06da`（2026-03-22，审计时与 `origin/master` 一致）；
- MoshCatty：`f2bb6ad7070d4f81ffd5c3592c98c94c75a39950`；这是补充研究开始时的固定对照点；
- 论文：[Mosh: An Interactive Remote Shell for Mobile Clients](https://mosh.org/mosh-paper.pdf)，重点是第 2 节 SSP；
- 加密规范：[RFC 7253](https://www.rfc-editor.org/rfc/rfc7253)，重点是 4.2、5.1 节。

复查得到四个可以直接由源码推出的用户行为缺陷、一个高延迟性能差异和一个安全加固缺口：

| 优先级 | 新发现 | 判断 |
|---|---|---|
| P0 | 首个 UDP 状态没有携带窗口尺寸 | 明确缺陷，会让远端程序以错误尺寸启动 |
| P0 | 接收上限只有 1 MiB，官方实现为 4 MiB | 明确互通缺陷，合法大状态会被冻结 |
| P1 | 首次连接超时直接结束，不执行官方关闭流程 | 明确生命周期缺陷，单向故障可遗留远端会话 |
| P1 | ACK 到达时才刷新“最后成功往返” | 明确计时缺陷，会延后断线提示和端口跳转 |
| P2 | 没有官方的 assumed-receiver / prospective-resend 选择 | 不破坏正确性，但 ACK 回程丢失时会重复发送越来越大的输入历史 |
| P2 | 没有官方客户端的禁用崩溃转储保护 | 安全加固缺口，不是 RFC 7253 线协议不兼容 |

其中前四项与现有审计中的“首个状态即为真实尺寸”“首次连接永久超时行为相同”“最后回复/端口跳转时间相同”“安全上限不影响官方合法状态”等表述冲突，建议在修复后同步更正原报告。

## 资料边界

Mosh 没有一份单独的“SSP wire RFC”。RFC 7253 只规定 OCB 认证加密算法，不能用来证明 SSP 的状态编号、关闭、压缩上限或重传策略。SSP 这些行为的一手依据是 Mosh 论文和官方实现。

RFC 7253 要求同一密钥下 nonce 不得重复，并规定认证失败必须判为无效密文。当前实现的方向位、nonce、128 位 tag、空 AAD 和认证失败处理已经在原审计覆盖，本轮没有发现新的 RFC 7253 偏差。本报告不重复原审计已经充分覆盖的项目。

## 1. 首个 UDP 状态没有携带窗口尺寸

### 官方证据

官方客户端在创建网络对象以后、进入第一次 `network->tick()` 以前，就把 resize 放进当前用户状态：

- [`stmclient.cc:260-269`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/stmclient.cc#L260-L269)：创建 `NetworkType`，随后立即 `push_back(Parser::Resize(...))`；
- [`stmclient.cc:553`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/stmclient.cc#L553)：第一次真正发送发生在后续主循环的 `network->tick()`。

服务端若 SSH 没有提供有效 PTY 尺寸，会先使用 80×24，并明确期待首个客户端状态覆盖它：

- [`mosh-server.cc:421-432`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-server.cc#L421-L432)；
- 服务端收到任何第一个新用户状态后都会放行子进程，即使这个状态的 UserStream 为空：[`mosh-server.cc:761-807`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-server.cc#L761-L807)、[`mosh-server.cc:851-857`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-server.cc#L851-L857)。

### 当前代码

- [`client.rs:158-179`](../src/client.rs#L158-L179) 创建 Transport、强制发送，然后在 `dial_with_size` 返回前调用 `flush_ticks()`；此时 `actions` 为空且 `dirty=false`；
- [`transport.rs:397-407`](../src/transport.rs#L397-L407) 在没有待发状态时仍创建一个新的空状态并发送；
- CLI 直到 `dial_with_size` 返回后才调用 resize：[`mosh_client.rs:83-84`](../src/bin/mosh_client.rs#L83-L84)。

`TerminalView::new(cols, rows)` 只把本地保存的远端 framebuffer 初始化为该尺寸，不会生成发给服务端的 `UserInstruction::Resize`。因此现有单元测试只检查本地 framebuffer 尺寸，不能证明首个线状态包含 resize。

### 用户影响

当 SSH bootstrap 没有 PTY、PTY 报告 0×0，或上层传来的 PTY 尺寸和实际窗口不一致时，官方服务端会先用 80×24 放行远端子程序。resize 虽可能在几毫秒后的第二个状态到达，但 shell 启动脚本、`tmux`、全屏程序等已经可能读到错误的初始尺寸、完成一次错误布局，甚至错过它依赖的首次 `SIGWINCH` 时序。

### 建议测试 seam

1. 用本地 UDP 假服务端接收 `Client::dial_with_size(120, 40)` 的数据报；
2. 以真实会话密钥解密、按官方分片格式重组、zlib 解压并解析第一个 `TransportInstruction`；
3. 断言第一条 `UserMessage` 已含 120×40 resize，且它前面没有空的新状态；
4. 在 Ubuntu 上再用官方 `mosh-server` 的“无 SSH PTY”启动方式，远端子程序一启动就记录 `TIOCGWINSZ`，应直接得到 120×40，而不是先得到 80×24。

## 2. 当前会拒绝官方合法的 1–4 MiB SSP 指令

### 官方证据

- 官方压缩器的固定输出缓冲是 `2048 * 2048`，即 4 MiB：[`compressor.h:38-52`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/compressor.h#L38-L52)；
- 压缩和解压都实际使用这个 4 MiB 缓冲：[`compressor.cc:40-53`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/compressor.cc#L40-L53)；
- 官方先拼完同一 instruction 的全部分片，再解压并解析：[`transportfragment.cc:129-147`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportfragment.cc#L129-L147)。官方接收路径没有 1 MiB 的额外门槛。

这不是 RFC 7253 的要求，而是官方 Mosh 的实际互通边界。

### 当前代码

- [`fragment.rs:17`](../src/fragment.rs#L17) 把压缩后完整 instruction 上限设为 1 MiB；[`fragment.rs:113-121`](../src/fragment.rs#L113-L121) 超出后直接清空重组状态；
- [`transport.rs:717-733`](../src/transport.rs#L717-L733) 对解压结果再次设置 1 MiB 上限。

所以有两种官方合法数据会被静默丢弃：压缩体超过 1 MiB，或压缩体较小但解压后的 protobuf 在 1–4 MiB 之间。

### 用户影响

大窗口、低压缩率大画面、一次包含大量终端单元变化的状态，可能被当前客户端永久丢弃。客户端不会进入该远端状态，也不会确认它；服务端会继续重传。用户看到的通常是画面冻结、网络流量持续增加，直到后续某个较小且可从当前基线应用的状态到来；如果后续状态引用被丢弃的状态，画面可能一直无法前进。

现有 180×70 测试不足以覆盖这个边界：它只证明“会分片”，没有让压缩体或解压结果跨过 1 MiB。

### 建议测试 seam

- 构造解压后为 1.25 MiB、压缩体远小于 1 MiB 的合法 instruction，必须接受；
- 构造压缩体为 1.25 MiB、解压结果小于 4 MiB 的合法多分片 instruction，乱序发送后必须重组；
- 构造解压结果超过 4 MiB 的压缩炸弹，仍应拒绝；
- 在 Ubuntu 官方服务端输出固定种子的低压缩率画面，使单条官方 instruction 明确跨过 1 MiB，抓包解密记录压缩前后尺寸，并确认画面最终完整、状态得到 ACK、没有无限重传。

## 3. 首次连接超时没有执行官方关闭流程

### 官方证据

- 官方客户端等待首个服务端状态超过 15 秒后调用 `network->start_shutdown()`，而不是立即退出：[`stmclient.cc:536-544`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/stmclient.cc#L536-L544)；
- 主循环继续运行到关闭得到确认或关闭重试超时：[`stmclient.cc:519-527`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/stmclient.cc#L519-L527)；
- 官方关闭重试和超时判断位于 [`transportsender-impl.h:373-385`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h#L373-L385)。

服务端的已连接会话网络超时默认是 0（关闭）：[`mosh-server.cc:393-406`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-server.cc#L393-L406)。60 秒退出只适用于从未收到任何客户端状态的服务端：[`mosh-server.cc:943-948`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-server.cc#L943-L948)。

### 当前代码

- [`client.rs:331-335`](../src/client.rs#L331-L335) 在 15 秒时直接 `mark_dead`；
- [`client.rs:415-418`](../src/client.rs#L415-L418) dead 后不再 tick；
- CLI 检测到 dead 后跳出循环，并明确跳过最后的 `graceful_shutdown`：[`mosh_client.rs:111-117`](../src/bin/mosh_client.rs#L111-L117)、[`mosh_client.rs:239-241`](../src/bin/mosh_client.rs#L239-L241)。

### 用户影响

最典型的触发方式是单向网络故障：客户端到 Ubuntu 的包可达，服务端已经收到状态并放行 shell，但服务端到客户端的回包被防火墙、错误路由或临时网络问题挡住。15 秒后当前客户端直接结束；服务端因为已经收到过客户端状态，不适用“60 秒无人连接”退出条件，远端 shell 可能长期残留。

### 建议测试 seam

使用非对称 UDP harness：接收并认证客户端上行包，但丢弃所有下行包。通过可注入时钟跨过 15 秒后，断言客户端开始发送 `new_num = UINT64_MAX` 的关闭状态并按官方边界重试，直到确认或超时；不能在第 15 秒直接停止 UDP。Ubuntu 端同时记录 `mosh-server` 和子 shell 是否及时退出。

## 4. 最后成功往返使用了 ACK 到达时间，而不是被确认状态的发送时间

### 官方证据

- 官方在处理 ACK 后，把“被确认发送状态原本的发送时间”交给连接层：[`networktransport-impl.h:80-84`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/networktransport-impl.h#L80-L84)；
- getter 返回已确认发送状态保存的 timestamp：[`transportsender.h:156-160`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender.h#L156-L160)；
- 这个时间直接用于“最后回复”提示和端口跳转：[`stmclient.cc:296-305`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/stmclient.cc#L296-L305)、[`network.cc:389-399`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/network.cc#L389-L399)。

### 当前代码

- [`transport.rs:535-540`](../src/transport.rs#L535-L540) 在 ACK 到达时直接写 `Instant::now()`；
- [`client.rs:211-216`](../src/client.rs#L211-L216) 和 [`client.rs:384-388`](../src/client.rs#L384-L388) 用它决定断线提示和 10 秒端口跳转；
- 空 ACK/heartbeat 状态没有保存在 `outbound_states`（[`transport.rs:397-407`](../src/transport.rs#L397-L407)），因此当前结构甚至无法找回所有被确认状态的原始发送时刻。

### 用户影响

一个很迟才到达、但确认的是旧状态的 ACK，会被当前实现当成“刚刚完成了一次成功往返”。这会让“最后回复”提示变晚，也会把更换本地 UDP 端口再推迟 10 秒。网络切换、NAT 映射变化、严重不对称排队时，这正好会延长原本应该尽快恢复的阶段。

### 建议测试 seam

使用可注入单调时钟：T0 发送状态，T0+8 秒才送达其合法 ACK。断言 `last_roundtrip_success` 仍等于 T0 附近的发送时间；到 T0+10 秒应允许端口跳转，而不是从 ACK 到达时重新计 10 秒。普通输入状态和空 heartbeat 都要覆盖。

## 5. ACK 回程丢失时会反复发送累积输入

### 官方证据

Mosh 论文第 2.3 节说明，发送端会根据链路条件选择帧率，并利用任意两个对象状态间的 diff，让接收方尽快追到当前状态，而不必逐字节重放每个中间状态。

官方实现会把 RTO+ACK delay 内最近发送的未确认状态暂时当作接收方已经拥有的状态：

- [`transportsender-impl.h:73-104`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h#L73-L104)；
- [`transportsender-impl.h:255-278`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h#L255-L278)。

随后它会比较“从假定状态增量发送”和“从已知确认状态可靠重发”的长度，选择更合适的基线：[`transportsender-impl.h:395-415`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h#L395-L415)。

### 当前代码

- [`transport.rs:197-207`](../src/transport.rs#L197-L207) 每个新状态始终以 `acked_by_remote` 为 old state；
- [`client.rs:419-427`](../src/client.rs#L419-L427) 每次把 `acked_action_count` 以后的全部输入重新编码。

因此当前做法是正确性优先的保守简化，不会凭空漏按键，但没有官方的带宽优化。

### 用户影响

当上行数据能到服务端、回程 ACK 长时间丢失，同时用户持续输入或粘贴时，每个新状态会重复携带全部未确认输入。编码内容、分片数和网络流量随未确认输入增长，可能反过来增加排队和延迟。现有“500 ms RTT 两个按键”测试无法覆盖这种长期非对称故障。

### 建议测试 seam

在 Ubuntu 隔离链路只丢 server→client ACK、保留 client→server 数据，持续输入 60 秒。对当前客户端和官方客户端使用相同输入、固定 RTT/丢包种子，解密统计：客户端 UDP 总字节、每状态 diff 大小、最大分片数、远端收到按键的延迟。先用数据确认增长曲线，再决定是否移植官方选择算法；修复必须同时证明按键字节完全一致。

## 6. 缺少官方的崩溃转储保护

### 官方证据

- 官方客户端在读取参数和会话密钥以前就禁用 core dump：[`mosh-client.cc:108-113`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/frontend/mosh-client.cc#L108-L113)；
- Unix 实现把 `RLIMIT_CORE` 软限制设为 0：[`crypto.cc:284-304`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/crypto/crypto.cc#L284-L304)；
- 官方 OCB context 析构时还会清理密钥上下文：[`crypto.cc:155-167`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/crypto/crypto.cc#L155-L167)。

### 当前代码

- [`mosh_client.rs:26-70`](../src/bin/mosh_client.rs#L26-L70) 会从环境删除 `MOSH_KEY`，但进程启动时没有禁用崩溃转储；
- [`crypto.rs:26-34`](../src/crypto.rs#L26-L34) 保存 AES key schedule 和派生块，没有销毁时清零；项目也没有使用 secret zeroization 库。

### 用户影响

如果系统开启 core dump、容器运行策略允许转储，或 Windows 错误报告生成进程转储，崩溃文件可能把会话密钥、解密后的终端内容和输入保存到磁盘。攻击者已经能读取这种转储时通常也拥有较高本机权限，但官方 Mosh 明确把它当作值得阻断的敏感数据落盘路径。

### 建议测试 seam

- Ubuntu：以非零 `ulimit -c` 启动客户端，读取密钥后检查 `/proc/<pid>/limits`，`Max core file size` 应为 0；
- 单元测试：把密钥材料封装成可观测的 secret wrapper，验证销毁路径执行清零；
- Windows：根据 Netcatty 的实际发布方式验证 WER/minidump 策略，不应照搬 Unix API 后就宣称跨平台完成。

## 一个不应先当作用户缺陷的 ACK 健壮性差异

官方只接受仍存在于 `sent_states` 的 ACK；已经裁掉的状态号会被忽略：[`transportsender-impl.h:348-370`](https://github.com/mobile-shell/mosh/blob/decd9b705eb81626f694335b8d5940538beb06da/src/network/transportsender-impl.h#L348-L370)。当前只检查 `acked_by_remote < ack_num <= sent_num`（[`transport.rs:535-556`](../src/transport.rs#L535-L556)），而状态号在实际发送前就已分配，队列也可能裁掉中间状态。

这是明确的实现差异，但正常官方服务端无法确认从未收到的状态，迟到确认已发送但被本地裁掉的状态也不必然造成错误。因此本轮不把它列为已证实的普通用户缺陷。建议增加“未发送状态号 ACK”和“已裁状态号 ACK”的定向测试，并在修正状态保存模型时一并收紧校验。

## 建议的 Ubuntu 验证顺序

1. **先测首包 resize**：最小、确定性最高，可以直接推翻或确认现有报告的“首次状态真实尺寸”；
2. **再测 1–4 MiB 边界**：分别覆盖压缩体和解压体，避免只测多分片却没跨过门槛；
3. **再测单向网络**：同一个 harness 同时验证 15 秒后的关闭状态、旧 ACK 的时间戳以及 10 秒端口跳转；
4. **最后跑 60 秒持续输入基准**：与 stock mosh 使用相同网络种子对比流量和延迟，确认 prospective resend 的实际收益；
5. 每项修复后，重跑现有全部单元测试、真实官方服务端双向交互、长断网恢复、IPv4/IPv6 分片和正常关闭，防止修复新边界时破坏已通过路径。

## 最终判断

当前实现的 OCB3、基本 UDP 格式和 SSP 状态重建仍然可以保留；没有证据支持“大范围重写”。本报告发现的首包尺寸、4 MiB 官方边界、首次超时关闭、ACK 成功时间、长时间 ACK 回程丢失下的累积重发和 Unix 崩溃转储保护均已在后续改动中修复。剩余发布验收集中在 Windows + ConPTY 实机和公网 IPv6 近 MTU/黑洞场景。
