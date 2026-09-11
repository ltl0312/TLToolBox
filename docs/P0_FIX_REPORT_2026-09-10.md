# TLToolBox P0 整改记录（2026-09-10）

- **上游依据**：[`CODE_AUDIT_2026-09-10.md`](./CODE_AUDIT_2026-09-10.md) 第 5 节「改进优先级路线 · P0 — 立即」
- **整改范围**：S1 / S2 / S4 / S5 共 4 项（S3 / S6 及全部 M / L 级项属 P1 / P2，本次未动）
- **版本**：`v0.6.0` → 拟随 `v0.6.1` 发布
- **验证**：`cargo fmt --check` ✅ · `cargo clippy --all-targets -- -D warnings` **零告警** ✅ · `cargo test` **275 项全绿**（含 5 项 `#[ignore]` 联网回归）✅

## 结论速览

| 项 | 问题 | 状态 | 核心落点 |
| --- | --- | --- | --- |
| **S1** | 检查更新永久失效（状态码误读） | ✅ 已修复 | `update.rs` 补 `WINHTTP_QUERY_FLAG_NUMBER`；**另发现并修复第二个叠加缺陷** |
| **S1-b** | 请求头块含尾部 NUL → `WinHttpSendRequest` 返回 `E_INVALIDARG` | ✅ 已修复（整改实测新发现） | `update.rs::request_headers` 剥离终止符 |
| **S2** | 一键释放端口可终止系统关键进程 | ✅ 已修复 | 终止入口强制闸门 + UI 系统行不可释放 |
| **S4** | 守护线程创建失败 → 停机永久挂死 | ✅ 已修复 | 创建失败即回传错误 + 带超时 join |
| **S5** | FFI 回调无 panic 边界（panic 即 abort 进程） | ✅ 已修复 | 新增 `ffi_guard`，5 处回调全部包裹 |

---

## S1 · 检查更新永久失效

**原判定**：`WinHttpQueryHeaders` 未带 `WINHTTP_QUERY_FLAG_NUMBER`，头值按 ASCII 字符串写回并被当作 `u32` 误读（`"200"` → `3_158_066`），`Some(200)` 永不命中。

**处理**：
1. `src/update.rs` 新增 `imp::STATUS_CODE_QUERY_FLAGS = WINHTTP_QUERY_FLAG_NUMBER | WINHTTP_QUERY_STATUS_CODE` 并用于 `query_status_code`（拆成常量是为了让单测能离线守卫"标志位被误删"这一回归动作）。
2. 把「状态码 + 响应体 → 结论」的映射抽成纯函数 `interpret_response(status, body) -> Option<String>`，与网络无关、可单测。
3. 把整条 WinHttp 管线收敛为单一 `imp::run_pipeline<T>(handler)`，生产路径与测试探针**共用同一份**会话 / 连接 / 请求 / 发送 / 接收 / 读头 / 读体代码——旧结构正是因为单测只覆盖尾部纯函数而让 S1 长期潜伏。

**S1-b · 整改实测新发现的第二个缺陷（同属 S1 的 P0 范围）**

新增的联网回归测试一跑即暴露：**修复状态码标志位后仍然拿不到任何结果**。逐步诊断（临时探针，已删除）定位到：

```text
Send A (头切片含尾部 NUL, len=61) -> Err(E_INVALIDARG 0x80070057)   ← 生产代码原形态
Send B (无额外头)                  -> Ok(())
Send C (头切片不含 NUL, len=60)    -> 通过参数校验
```

`WinHttpSendRequest` 的 `dwHeadersLength` 语义是"头块字符数"，而 windows-rs 把 `Some(&[u16])` 直接映射为 `(ptr, len)`；旧代码传入的 `to_wide_units(...)` 切片**带 NUL 终止符**，WinHttp 判定长度区间内出现 NUL，直接返回 `E_INVALIDARG`——**请求根本发不出去**。

修复：新增 `imp::request_headers()` 统一剥离终止符。修正后在真实环境实测：

```text
Send (no-NUL)      -> Ok(())
ReceiveResponse    -> Ok(())
QueryStatusCode(NUMBER) -> Ok(()), status=200   ← 状态码标志位修复同时被 vivo 验证
BODY_LEN=5379  tag_name 可定位
```

**验证证据**：
- 离线：`ascii_status_read_as_dword_is_not_http_semantic`、`status_code_query_flags_must_include_number_flag`、`request_header_block_has_no_trailing_nul`、`interpret_response_maps_status_codes_conservatively`；
- 联网（`cargo test --lib -- --ignored`）：`live_status_code_is_read_as_number_not_ascii_string`、`live_fetch_latest_tag_runs_end_to_end` **均实测通过**；两条测试对 `E_INVALIDARG` 显式判失败，避免把代码缺陷伪装成"网络不可用"跳过。

---

## S2 · 「一键释放端口」可终止系统关键进程

**处理**（闸门与展示选项彻底解耦）：
1. `killer.rs` 新增 `PortError::ProtectedProcess { pid, reason }`（无 Win32 码、非 UIPI、文案自解释）与 `is_protected()`。
2. 新增闸门判定 `is_protected_target(pid, name, port) = is_kernel_owner(pid) || is_system_service_entry(pid, name, port)`。
3. `kill_process_and_release_port` 在 `OpenProcess` **之前无条件**执行闸门：命中即 `Err(ProtectedProcess)` 并写 `warn` 审计，**永不触碰终止 API**。镜像名取**终止时刻**的实时查询结果（`QueryFullProcessImageNameW`），而非 UI 送来的陈旧缓存名——顺带在身份层面化解了 S3 的 PID 复用错配风险。
4. `scanner::SYSTEM_RESERVED_PORTS` 由私有提升为 `pub`，供闸门复用同一份清单（单一事实源）。
5. UI 侧：`PortEntryItem` 新增 `protected` 字段（装配层用与闸门同源的 `is_protected_target` 计算），受保护行**不渲染释放按钮**，降级为「系统进程」静态标签 + 悬停说明。界面禁用与入口拒绝永不出现口径分叉。
6. 闸门拒绝**不发布 Toast**（避免与装配层的通用失败提示重复形成双反馈）；装配层照常写审计并弹通用失败提示。

**验证证据**：`protected_target_covers_kernel_images_and_reserved_ports`（内核态 / 8 个系统镜像 / 9 个保留端口全部命中，普通开发进程放行）、`gate_rejects_system_targets_without_touching_terminate`（用保留端口 445 验证拒绝发生在 `OpenProcess` 之前）、`protected_process_error_model_is_self_describing`、`gate_rejection_publishes_no_toast`。

---

## S4 · 守护线程创建失败 → 停机永久挂死

**处理**（`icon_locker/daemon.rs`）：
1. `run_pump` 的就绪通道由 `Sender<isize>` 改为 `Sender<Result<isize, String>>`；`CreateWindowExW` 后**立即判空 / 判错**，失败即回传 `Err(含 Win32 错误码或 HRESULT)` 并 `return`，**不进入 `GetMessageW`**——从根上消灭"无窗口线程永久阻塞在消息泵"的形态。
2. `spawn` 收到 `Err` 时先 `join` 收尸（此时线程已返回，join 瞬时完成）再把错误向上抛；模块 `start()` 据此保持"停止态"并告警，不再出现"显示运行中却毫无显示器响应"的假象。
3. 新增 `STOP_JOIN_TIMEOUT`（2s）与 `join_with_timeout` 辅助函数；`Drop` 改为**带超时 join**，超时即告警放弃——`stop()` 不再可能在 async 上下文里永久阻塞 Tokio worker 与应用收尾。

**验证证据**：`stop_join_timeout_is_bounded`（上限有界且非零）、`join_with_timeout_never_blocks_forever`（结束线程正常 join；卡死线程到点返回 `false` 且不长时间阻塞）、`watchdog_spawn_stop_drop_is_bounded`（真实守护线程 spawn→stop→drop 全链路有界完成）。

---

## S5 · 全部 FFI 回调缺少 `catch_unwind` 边界

**处理**：
1. 新增 `src/ffi_guard.rs`：唯一原语 `guard_ffi(context, f)` —— 用 `catch_unwind(AssertUnwindSafe(..))` 截停 panic，返回 `Option<R>`；**兜底本身再包一层 `catch_unwind`**，保证日志订阅者 panic 时兜底不会成为新的爆点。`lib.rs` 注册 `pub mod ffi_guard`。
2. 5 处 `extern "system"` 回调入口全部包裹，并给出各自的**无害降级返回值**：

| 位置 | 降级语义 |
| --- | --- |
| `popup_blocker::win_event_proc` | 放弃本次事件（不下发 `WM_CLOSE`） |
| `topmost_manager::win_event_proc` | 放弃本次事件（丢一拍前台纠偏 / 最小化解置顶） |
| `topmost_manager::enum_proc` | 返回 `TRUE` **继续枚举**（最多丢一条候选窗口） |
| `icon_locker` `EnumDisplayMonitors` 回调 | 返回 `TRUE` 继续枚举 |
| `icon_locker` 守护线程 `window_proc` | 转发 `DefWindowProcW`（最多丢一拍防抖） |
| `tray` 消息泵 | 单轮迭代整体包裹，panic 后**继续下一轮**（最多丢一次菜单 / 图标事件） |

3. 托盘一处额外做了结构重整：把消息泵单轮迭代抽成 `TrayManager::pump_once()`，`run()` 以 `guard_ffi` 包裹每轮迭代——这样 `DispatchMessageW` 中展开的 muda / tray-icon **第三方**窗口过程的 panic 同样被截停（这是该项目此前完全无覆盖的部分）。

**验证证据**：`ffi_guard` 自身 4 个单测（正常透传、panic 截停、String 载荷截停、截停后调用方继续运行）。Cargo.toml 已在 release profile 注释中声明刻意保留 `panic = "unwind"`，`catch_unwind` 在该构建形态下有效。

---

## 未纳入本次整改（P1 / P2，状态如实记录）

- **S3**（PID 复用 TOCTOU）：**已显著缓解但未闭环**。S2 的闸门会在终止前用**实时**镜像名复核身份，系统性风险高的目标（内核态 / 系统镜像 / 保留端口）已被挡下；但"普通开发进程 PID 被复用于另一个普通进程"的场景仍需 P1 的完整方案（重新枚举确认该 PID 此刻仍持有该端口，或比对镜像名不一致即中止）。
- **S6**（提权重启后单实例空窗）、M1 / M2 / M4① / M6 / M7 与全部 L 级项：按路线留待 P1 / P2。
- 审计报告本身未做任何修改（保持只读产物属性），整改以本文件为记录。

---

*本文件为整改产物，随代码变更一并入库。*
