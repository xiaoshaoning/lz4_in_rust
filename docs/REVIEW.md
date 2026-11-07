# 代码评审与后续计划

针对 0.0.1（`src/` 约 1.4 kloc，零 `unsafe`）的一次通读评审，按《A Philosophy of Software
Design》的检查项（深模块 / 信息隐藏 / 错误模型 / 重复 / 命名）过一遍，把结论固化成可勾选的
计划。设计决策本身在 `DESIGN.md`，这里只记**问题、证据、最小修法、验收方式**。

优先级顺序即清单顺序。`[x]` 表示已落地（含测试），`[ ]` 表示待办。

---

## 一、已修（本次评审，提交 `review:` 系列）

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F1 | `par::decompress_into` 接受"解出的字节数少于槽位"的块：返回 `Ok`，调用者缓冲区尾部留着旧字节 —— frame 解码器的静默数据损坏通道 | 在不该"定义掉错误"的地方定义掉了 | 新增 `par::decode_block()`：**一个地方**强制 `解出 == 槽位`，顺序路径与池路径共用；新增 `Error::BlockSizeMismatch { expected, got }`；回归测试 `short_block_is_rejected`（槽位 808、块只解出 20 → `Err`） |
| F2 | 块数量与布局不符时报 `Truncated`（像是输入被截断，实为调用者布局错） | Vague error / 掩盖信息 | `Error::BlockCountMismatch { expected, got }`，测试 `wrong_block_count_and_wrong_length_are_errors` |
| F3 | `expect("control")` / `expect("queue")`：一个 worker panic → 锁被毒化 → 其余 worker 连锁 panic，原始失败被埋 | 重复 + 掩盖原始错误 | `lock()` 忽略毒化（受保护数据是计数器/worker 数，两种状态下一致）；join 到 panic 时 `resume_unwind` 保留原始 payload |
| F4 | 拷贝辅助函数对长度恒定的切片做 `try_into().expect("wide")`：不可能失败、但一旦失败就在"绝不该 panic 的解码器"里 panic | 多余失败点 | 改 `copy_within` / `copy_from_slice`（长度由构造保证）；少 4 个 `expect`，实测解压不慢（≈6.2–6.3 GiB/s @8 线程） |
| F5 | `lib.rs` 把 `hc`/`frame` 写成 intra-doc 链接，但模块不存在（`cargo doc` 两条 warning） | 文档描述了不存在的结构 | 改表格 + 状态列；CI 加 `RUSTDOCFLAGS="-D warnings" cargo doc` |
| F6 | `decompress_into` 的**公开**文档里写实现细节（"96% 的匹配 ≤ 32 字节…"） | Implementation docs contaminate interface | 移进函数体的 `// Performance:`；接口注释只留契约 |
| F7 | `TABLE_SIZE` 没说明为什么是 8192（`byU16` 需要，`byU32` 只用一半） | 注释缺失 | 补一行说明 |

第二轮评审（R1/R2 落地之后，针对新代码）：

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F8 | `Control` 的文档写"growth happens between calls, which is where a new plan starts from the last measured best" —— **代码里没有任何东西跨调用保留 `best`**（每次 `Control::new` 都从 0.0 开始），文档描述了一个不存在的设计 | 注释描述了不存在的结构（比没注释更糟） | 文档改为"只能降、且需要 ≥2 个采样窗口才会动"；"跨调用持久化"登记为 R7 |
| F9 | 采样窗口是 `max(8×blockSize, 8 MiB)`（4 MiB 块 → 32 MiB），因此**输入小于约 64 MiB 时永远拿不到两个样本**，`Auto` 等于 `k0`（我自己 32 MiB 的基准语料正是这种情况）。DESIGN 只写了"每 32 MB 采样一次"，没写这个后果 | 策略的后果没写在代码旁边 | 在 `window` 处注明代价；REVIEW 记录；不缩小窗口（噪声免疫更值钱） |
| F10 | `DESIGN.md` 三处仍写 HC = "levels 3..12"/"3..=9"，而 R1 之后代码里 HC 从 **2** 开始（`LZ4HC_CLEVEL_MIN`）。文档与代码矛盾 | 文档漂移 = unknown unknown | 改成 2..=12（贪心 2..=9，optimal 10..=12），三处一并修 |
| F11 | `Level::acceleration()` 公开，但对高压缩档位没有意义（照样返回 1），调用方可能误用 | Overexposure | 改为私有；测试改成断言 `Level::new(1) == Level::FAST` |
| F12 | `block::compress_into(acceleration)` 与 `level::compress_into(Level)` 是两个公开的"使劲程度"入口，文档互不指认 | 两处说同一件事，读者要自己判断用哪个 | `block` 的文档加一句"多数调用方要 `crate::level`；这里是 `--fast=N` 对应的裸旋钮" |

---

## 二、计划清单（待办）

### 结构（frame 落地前必须拍板，因为 frame 是它们的下一个消费者）

- [x] **R1 `Level` 与 `acceleration` 合一**（信息泄漏，最高优先）
  `par` 曾有两处写死 `compress_into(…, 1)`；HC 一来，"1 走 fast，2..12 走 HC"会同时出现在
  `par`/`frame`/CLI。
  **修法**：`src/level.rs` 持有这条映射（`Level::is_fast()` ⟺ `LZ4HC_CLEVEL_MIN = 2`）与
  `level::compress_into(src, dst, Level)`；`par::Opts` 增加 `level: Level`。
  **验收**：`level::tests::fast_level_matches_the_c_fixture`（`-1` 与 C 的默认输出逐字节相同）、
  `level_flows_through_the_pool`（池里也走同一条路）、`high_levels_report_the_gap_instead_of_guessing`
  （2..12 报 `Unsupported` 而不是偷偷按 1 压）。

- [x] **R2 `par::compress` 物化所有块**（峰值内存 O(输入)，与 §9 的 `k × blockSize × 2` 预算矛盾）
  frame 编码器需要的正是"每块压完立刻写出去"。
  **修法**：新增 `par::compress_each(src, opts, |index, bytes| …)`，**按输入顺序**交付，
  窗口受 `workers + 1` 块约束；`compress` 变成它的收集型包装。错误类型泛型
  （`E: From<Error> + Send`），让 frame 用自己的 IO 错误类型而不被压平。
  **验收**：`compress_each_delivers_in_order_exactly_once`（顺序 + 只交付一次 + 与收集型一致）、
  `compress_each_propagates_the_sink_error_without_hanging`（sink 在 index 2 报错 → 立刻返回，
  不挂死；同时验证 codec 错误能转进调用者的错误类型）。
  **实测**：改造后吞吐不变（32 MB、4 MB 块、8 worker：压缩 4.0–4.5x、解压 3.0–3.9x）。

- [ ] **R3 `Error::Unsupported(&'static str)` 会成为 frame 的垃圾桶**
  （保留位、未知 block size、legacy 帧都会往里塞字符串）。
  **修法**：定调"*种类*用命名变体，字符串只做消息"。frame 落地时新增
  `ReservedBits`, `UnsupportedBlockSize(u8)`, `LegacyFrame` 等；`Unsupported` 保留给真正的
  "本移植还没有"（如 HC 未移植）。
  **验收**：frame 的错误测试按变体匹配，而不是匹配字符串。

- [ ] **R4 两段 worker 骨架重复**（compress：原子游标 + join 回收；decompress：mutex 栈原地写）。
  合并时顺手把 `Control.k` 拆成 `AtomicUsize`：现在它与统计量共用一把锁，每个 worker 每块都要抢一次锁只为读一个数。
  队列形状不同是**有理由的**（压缩不能预分配全部输出块，否则破坏内存预算），但
  "admit k / 计时 / report / join / 取消"这套骨架可以合成一个 `Scheduler`。
  **修法**：把 `Control` 收进一个 `Scheduler`（`admit(id) -> bool`、`report(bytes, secs)`、
  `run(job)`），两处只留各自的队列与结果形态。
  **时机**：在 frame 解码器成为第三个消费者**之前**做，否则要改三处。
  **验收**：`par` 现有的 12 个测试全绿，且两个函数各减少 ~15 行。

- [ ] **R5 `plan_with` 的 `avail` 参数是公开的**（只为测试和受限主机）
  **修法**：文档写明，或把 `plan_with`/`compress_*_with`/`decompress_*_with` 归入
  `#[doc(hidden)]` + 一个公开的 `plan_on(opts, len, cores)`。避免调用方随手编一个核数。
  **验收**：公开文档里不再出现"avail"作为推荐用法。

- [ ] **R6 `par` 的 5 个公开常量**（`MIN_PARALLEL_BLOCK`、`BLOCKS_PER_WORKER`、`SMALL_HOST_CORES`、
  `DEFAULT_MEMORY_BUDGET`、`MAX_BLOCK_SIZE`）
  CLI 帮助文本确实要用，但应在文档里声明它们是**策略**，不是调用方可调的旋钮；
  若要可调，就统一成 `Opts` 的字段。
  **验收**：`par` 的模块文档明确"这些是策略常量"。

- [ ] **R7 自适应只在单次调用内生效**（F8 暴露的真缺口）
  现在：`Control` 每次调用新建，`best` 不跨调用；只能降、不能升。
  要做到文档原先声称的行为，需要**有状态的控制器**：`Opts` 是 `Copy`/无状态的，所以要么
  让 `par::compress_each` 接受 `&mut Adaptation`（frame 编码器持有它，跨块/跨文件复用），
  要么把控制器上提到 frame 层。
  **验收**：连续处理 N 个文件后，第二个文件的起步 worker 数等于第一个文件测到的最优；
  以及"降了之后负载恢复，能自己加回去"。
  **时机**：frame 编码器出现时（它才是"跨调用"存在的地方）。

### 工程与测试

- [ ] **E1 Miri**：`rustup toolchain install nightly && cargo +nightly miri test`。
  零 `unsafe` 下主要抓 std 层的别名/UB 误用，便宜保险。
- [ ] **E2 随机差分 fuzz**：现在只有 10 个固定 fixture + 单块全截断/全比特翻转。
  补一个"随机长度/随机分布 → 我们压 → C 解 → 比字节"和反方向的批量循环（`tools/fuzz_diff.sh`，
  用已编译的 `c-oracle`，CI 里跑几百轮）。
- [ ] **E3 平台覆盖**：`i686`（32 位）、大端（全程 `from_le_bytes`，理论安全但未验证）、
  >4 GiB 输入与多帧拼接。
- [ ] **E4 `rust-version = 1.75` 未实测**（只在 1.94 编过）。CI 加一个 `cargo +1.75 check`。
- [ ] **E5 文档测试**：0 个 doctest。给 `lib.rs` 加一段可编译的用法示例，等于多一个测试。
- [ ] **E6 `#![deny(missing_docs)]`**：公开项基本都有文档，打开这个开关可防止回退。

### 里程碑（详见 `DESIGN.md` §6）

- [ ] **M3 `hc`（levels 2..=12）**：hash chain 贪心（2..9）+ optimal parser（10..12）。
  R1 已把入口留好：实现 `hc::compress_into` 后只需在 `level::compress_into` 里接上。
- [ ] **M4b `frame`**：header/FLG/BD/HC(XXH32)、block header、block/content checksum、
  linked/independent、64 KiB–4 MiB 块、skippable frame；编码器直接消费 `par::compress_each`。
- [ ] **M5 CLI**：`-1..-12 -d -c -t -B# -BD -T# --single-thread --list …`（GPL-2.0-or-later，
  与上游 `programs/` 一致，独立目录 + `COPYING`）。

---

## 三、做得好、别在重构里弄坏的部分

1. **解码器的 fast/checked 双段切分 + "越界写安全"的论证**（`docs/DESIGN.md` §2）：
   输出顺序产生、匹配只回指 `op` 之前、后续 run 必然覆盖多余字节 —— 这是整个代码库里最深
   的一处设计，也是纯 safe 能跑到 C 的 0.6–0.7x 的原因。
2. **`plan_with` 是纯函数**：策略因此有 12 个不依赖线程、不依赖机器的确定性测试。
3. **`DESIGN.md` 记录了两种"看起来更优但实测更慢"的写法**（长度阶梯 0.59x、窗口切片 1.00x），
   避免后来者重试。
4. **`Error` 保持单一枚举、`Copy`、`PartialEq`**：调用方一个 `?`，测试可以直接 `assert_eq!`。
   （frame 的 IO 错误走泛型 `E: From<Error>`，不要把 `io::Error` 塞进这个枚举——那样会失去
   `Copy`/`PartialEq`。）
