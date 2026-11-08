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

第三轮评审（frame 落地之后，针对此前未评审过的 `src/frame.rs`、`tests/frame_interop.rs`、
`tools/` 的 frame 部分）：

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F13 | **一个 15 字节的恶意帧可以让解码器 panic**：descriptor 里声明的 `content_size` 被直接交给 `Vec::with_capacity`，`u64::MAX` 触发 "capacity overflow"，进程直接崩。这违反了 crate 的公开承诺（"任何输入只会得到 `Err`，不会 panic"） | 不可信输入被当成分配请求（信任边界破口） | 声明值只作提示：`with_capacity(declared.min(1 MiB))`，超出部分靠增长；仍保留"内容大小与声明不符 → `InvalidFrame`"的校验。回归测试 `huge_declared_content_size_is_not_an_allocation_request` |
| F14 | 没有任何"任意字节都不 panic"的测试；F13 正是这类测试能抓到的 | 测试缺口 | 新增 `arbitrary_bytes_never_panic`：0/1/4/7/15/40/200/1000 字节的随机输入各 64 组，外加对合法帧做 500 次随机单比特破坏 |
| F15 | `frame::Opts` 同时装着格式偏好和 `par` 的旋钮（`level`/`workers`/`memory_budget`），文档没说为什么 | 读者要在两处猜"这块该谁管" | 文档写明：frame 是"格式 + 块编解码"的组合点，所以两者在这里汇合；`par::Opts` 由这里构造，避免它的 block size 与帧的块大小打架 |

第四轮评审（`examples/`、`tools/` 全部、`.github/workflows/ci.yml` —— 此前从未跑过，因为仓库还没有 remote）：

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F16 | **CI 文件根本无法解析**：`- name: tests (no C toolchain needed: fixtures are committed)` 里那个 `: ` 让 YAML 在 `name` 值中途看到键值分隔符。第一次 push 就会红 | 从不被执行的配置 = 未知的未知 | 名称去掉冒号；并**用 PyYAML 实际解析过**才提交 |
| F17 | **CI 的 oracle 编译漏了 `liblz4frame.c`** —— 加了 `frame`/`unframe` 子命令后没同步 CI（只更新了 `build_oracle.sh`），链接会失败 | 同一件事两处描述，改一处漏一处 | 两个平台都补上 |
| F18 | **frame 差分测试在 CI 里从没跑过**：差分步骤只跑 `cargo test --test interop`，而 `tests/frame_interop.rs` 需要 `LZ4_C_ORACLE`，没设就静默 skip | 静默跳过的门 = 没有门 | 差分步骤改为设 `LZ4_C_ORACLE` 跑**整个 suite**，两个测试文件都会真跑 |
| F19 | CI 的"oracle 与我们的 fixture 一致"这一步只是**打印**摘要，没有比较 | 检查不检查 | 改成与钉住值 `5663fe8f` 比较，并额外用 C 解 `frame/text-1.lz4` 与原始字节 `cmp` |
| F20 | oracle 的 `frame` 子命令用 `LZ4F_compressBound` + **只调用一次** `LZ4F_compressEnd`。一旦 `autoFlush=1`，footer 放不下，`compressEnd` 只写一部分——悄悄产出 9 字节的**畸形"空帧"fixture** | 流式 API 的返回值被当成"写完" | 改用 `LZ4F_compressFrameBound`，并按文档循环 `compressEnd` 直到返回 0；13 个 frame fixture 重新生成后与提交版本**逐字节相同** |
| F21 | fixture 的字节依赖 liblz4 的内部缓冲（`autoFlush` 没设），上游换缓冲大小就会静默改变 fixture | 未钉住的输入 | 显式 `autoFlush = 1`（F20 就是它暴露出来的） |
| F22 | `examples/parallel_probe.rs` 报的是**平均值**（总时间/轮数），而 `DESIGN.md` §5 写的方法论是"取最小"；这台机器上两者差到 2 倍。而且它只测了收集型 `compress`，`frame` 实际用的是 `compress_each` | 工具与文档不一致，测的不是发布路径 | 改成逐轮取最小；新增一行 `compress_each (Auto, streaming)` 的结果 |
| F23 | `build_oracle.sh` 头部没说清哪些 fixture 是**提交的输入**、哪些是生成的（`random.raw`、`frame/large.raw` 不可复现） | 文档缺失 | 头部写明 |

第五轮评审（`tests/fuzz_diff.rs`、`cli/` 整个 crate、workspace 接线 —— 都是 pass 4 之后写的）：

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F24 | **`lz4r` 不带参数时会挂在 stdin 上**（终端里看起来像死机），而且会往终端里吐压缩后的二进制。lz4 本身两条都拒绝 | 非交互场景的默认行为错；用户看不到发生了什么 | 新增 `check_terminals()`（纯函数，可单测）：无输入 + stdin 是终端 → 报错；压缩结果写终端 → 报错；`-f` 可越过。`is_terminal()` 在 MSRV 1.75 内 |
| F25 | `-c` 同时给了输出文件名时**静默忽略**该名字，用户以为写了文件 | 静默丢弃用户输入 | 打到 stderr 的警告："-c writes to stdout; the output name is ignored" |
| F26 | `decompress_all` 的循环靠 `consumed` 前进；若某天解码器返回 0 就是死循环 | 潜在挂死（当前不可能发生，但没有守卫） | 加一行守卫 → `Err("decoder made no progress")` |
| F27 | `tests/fuzz_diff.rs` 里 `Fixture.dir` 字段没人读，靠 `let _ = &fx.dir;` 压警告；辅助函数叫 `main_oracle`（它不是 main） | 垃圾字段 + 掩耳盗铃的写法 + 误导的名字 | 删字段、删那行、改名 `oracle_path` |
| F28 | release 时版本号写在两个 `Cargo.toml` 里（改动放大） | Change amplification | `[workspace.package] version`/`rust-version`，两个 crate 用 `version.workspace = true`；许可证仍按 crate 分开（BSD vs GPL，这是有意不同） |

第五轮**核实过、没问题**的两件事：

- `cli/` **不会**被打进根 crate 的发布包（`cargo package --list` 里 0 个 `cli/` 条目），Cargo 自动排除 workspace 成员——所以不需要额外 `exclude`。
- 端到端测试用的 `target/cli-tests/<name>` 目录天然隔离（每个测试一个子目录），并行跑不打架。

第六轮评审（`src/hc.rs`，M3a 落地之后）：

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F24 | **`countBack` 的 4 字节快路径是死代码**：循环条件写反（`floor + back < -2`，C 是 `back + floor > 3`），所以每次计数都走逐字节循环。修好条件后立刻暴露第二个 bug：C 的 `LZ4HC_NbCommonBytes32` 在小端机上是 **`clz >> 3`**（数的是紧邻当前位置的高位字节），我用了 `ctz`，于是匹配长度算错、roundtrip 直接失败。逐字节版本一直掩盖着它 | 死代码掩盖错误；"看起来等价"的位运算方向错误 | 条件改成 `back + floor > 3`；取 `leading_zeros()` 并注明为什么不是 trailing；`cargo test`（debug）+ 差分 fuzz 双证 |
| F25 | 候选预检里 `matchPtr - lookBack` 会 **usize 下溢**（debug 下 panic，release 下回绕）：C 在这里读的是缓冲区之前的内存，本来就没有定义 | 潜在 panic（违反"任何输入不 panic"） | 显式 `near >= look_back` 判定，不可读时视为"预检不确定"、交给后面的 4 字节比较决定；注释写明 C 在此越界读 |
| F26 | `pattern_segment` 每个候选**调用两次**（`.is_some()` 之后 `.unwrap()`），level 9 热路径成本翻倍 | Repetition（同一个决定做两次） | 一次 `if let Some(...)` |
| F27 | `hc::compress_into` 把越界等级**夹到 9**，即"悄悄按另一个等级压"——正是 `level.rs` 明令禁止的行为 | 掩盖错误 | 越界直接返回 `Error::Unsupported`（调用方必须自己路由） |
| F28 | `NB_SEARCHES` 有 10 项但只有 3..=9 可达（前四项属于 `lz4mid`） | 死数据 + 索引与等级的关系不明显 | 7 项，按 `level - LEVEL_MIN` 索引，注释说明 |
| F29 | `best.len.max(1)` 恒等；C 里的 `assert(matchLength >= MINMATCH)` / `assert(longest >= 1)` 没有对应断言 | 无效防护 + 缺失不变式 | 删掉 max(1)；补两个 `debug_assert!` |

同时把**有意缺省的三个 C 分支**写进模块文档：外字典（`dictCtx`/`usingExtDict`）、`fillOutput`（`destSize`）、`favorDecSpeed`（frame 层选项）——它们的缺失是决定，不是遗漏。

第七轮评审（M3b 落地的 `lz4mid` + optimal parser + `chainSwap`）：

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F30 | **模块文档还停在 M3a 之前**：开头写"两种策略"，结尾写"Not here yet: level 2 和 10..=12"，中间还说 `sparsestChain` 是死代码——三句全与两小时后的代码矛盾 | 注释描述了不存在的结构（与 F8 同类，且这次是我自己刚写下的） | 文档重写成一张"等级 → 策略 → 判定依据"的表，列出四个**有意缺省**的 C 分支（外字典、`fillOutput`、`favorDecSpeed`、`suspend`），并说明为什么它们的缺失等于不可达 |
| F31 | `short_inputs_are_all_literals` 只覆盖了 hash chain；`lz4mid` 与 optimal 各自的 `len < MIN_LENGTH` 早退分支**完全没测** | 新策略未覆盖（三条早退路径） | 三种策略各取一个等级（2 / 9 / 12）跑同样的 0、1、12 字节用例 |
| F32 | C 里的 `assert(pos8 < ipIndex)` / `assert(pos4 < ipIndex)` 没有对应断言（`lz4mid` 的哈希表只应存过去的位置） | 缺失不变式 | 补 `debug_assert!`；顺带把小缓冲区与等级越界的测试从单一等级扩到三种策略 |

| F33 | 模块文档里写 `[`encode_sequence`]`（intra-doc 链接）指向一个**私有**函数 → `cargo doc -D warnings` 报 `private_intra_doc_links`。本地没发现因为那次提交我只跑了 clippy + test，**跳过了 doc 这一步**；CI 抓到的 | 检查流程漏了一步（不是代码问题） | 改成反引号（不链接）；并把结论记下来：`cargo doc` 必须和 clippy/fmt/test 一起跑，否则"文档门"形同虚设 |

第七轮**核实过、没问题**的：

- `lz4mid` 的 `end_index - 2 < ilimit` 守卫与 C 一致，它同时保证 `ADDPOS8(ip-2)` 的 8 字节读在界内（`ip < len - 6`）。
- optimal 的价格表尺寸 `OPT_NUM + TRAILING_LITERALS + 1` 覆盖 `add_match` 写的最后一个尾随单元；`add_match` 的调用点都用 `new.len + cur < OPT_NUM` 把关。
- `sparsest_chain` 只在 `match_length == best.len`（即本次迭代真的找到匹配）时被调用，所以 `best.len ≥ MINMATCH`，`end = best.len - 3 ≥ 1`，循环必然有界。
- 三种策略的输出都在 C fixture、差分 fuzz（等级 2/3/9/12）、CLI 端到端三处判等，且 fuzz 在 **debug 构建**下也通过（`debug_assert` 全程开启）。

第八轮评审（M4c 流式重写 + 可移植性扫尾，都是前几轮之后写的）：

| # | 问题 | 红标 | 处置 |
|---|---|---|---|
| F36 | **`-T4` 在流式路径上其实只跑了一个线程**：`Encoder` 的批大小是 `block_size × workers`，而 `par` 的"多线程值不值得"门槛要求输入 ≥ 8 个块——`-T2/-T4` 的批不够，策略直接退回 1 个 worker。之前的 benchmark 测的是整缓冲区 `par::compress`，所以从没暴露 | 静默退化（用户以为并行） | 批大小抽成 `batch_bytes(block_size, workers)`：单线程仍是 1 块，多线程至少 8 块；并有单测钉住"多线程的批必须满足门槛"。实测 64 MiB：`-T1` 711 ms → `-T4` 442 ms，且各线程数输出完全相同 |
| F37 | `Encoder::finish` 在**写完 end mark 和内容校验之后**才发现声明长度不符：产出一个"自称完整、但 descriptor 撒谎"的帧，然后才报错 | 错误发现的时机错了（已经写出去的东西无法收回） | 校验移到 end mark 之前：不匹配就什么都不写完整，调用方看到的是可检测的截断；错误信息提示"文件是否在被读取时变化" |
| F38 | `hc::compress_into` 没有检查 `MAX_INPUT_SIZE`，但它把位置当 `u32` 用（格式的索引 + 64 KiB 前缀）。>2 GiB 的直接调用会静默回绕（release）或 panic（debug） | 与 `block` 的实现不一致，且违背"任何输入只返回 Err" | 加同样的入口检查；并加一条断言不变式的测试：`MAX_INPUT_SIZE + 前缀偏移 < u32::MAX` |
| F39 | CLI 的 `optimal_block_size(…, len as usize)`：`u64 → usize` 的 `as` 在 32 位主机上会截断（>4 GiB 文件） | 32 位下的静默截断 | 改 `usize::try_from(len).unwrap_or(usize::MAX)` |

第八轮**核实过、没问题**的：

- `Decoder` 的窗口管理：每块解码前 drain 到最多 64 KiB 历史，`served_from/to` 只在服务完一块后才被下一次解码覆盖（先消费后压缩，索引不会失效）。
- `Encoder::flush_batch` 的 `par::compress` 一次一批 → 内存 = 批 + 压缩结果，仍是预算量级。
- `finish` 消费 `self` 并返回 `W`：错误路径上 writer 被丢弃，这是 `io::Result<W>` 的自然结果，已在文档说明。
- 分块无关性（写 1/7/4K/64K/整块、读 1/3/1000/1M）与"各线程数输出完全相同"两条性质在测试里钉住。

第三轮**只记录、不修**的小问题（工具/性能，不影响正确性）：

- `tools/c_oracle.c` 的 `unframe` 在错误路径上没调 `LZ4F_freeDecompressionContext`（工具里的
  泄漏，无所谓；真要干净就在每个 `return 1` 前清一次）。
- `frame::decompress` 每块都 `resize(start + block_size)` 到恰好大小，于是**每个满块的最后
  ~32 字节走的是精确拷贝路径**而不是宽搬运（解码器要求 `end + 32 <= dst.len()`）。块长
  4 MiB/64 KiB 时这点比例可以忽略，所以不动；只有将来做流式解码、块很多时再看。
- `frame::decompress` 只解码第一帧并返回 `consumed`，多帧遍历靠调用方循环；这是有意的
  接口选择（`Decoded.consumed` 就是为此存在），等 CLI 需要 `--list` 时再考虑加 helper。

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

- [x] **R3 错误分类**：frame 落地时新增了 `Error::InvalidFrame(&'static str)`（描述符/结构字段
  非法，字符串只说明是哪个字段），`Unsupported` 只留给"本移植还没做"（HC、字典、未知块大小）。
  下一步若变体继续膨胀，再拆成命名变体（如 `ReservedBits`），字符串保持"消息"角色。
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

- [ ] **R8 frame 的三个已知缺口**（M4c，见 `DESIGN.md` §6）
  ① `Read`/`Write` 适配层（现在整帧在内存里）；② 字典（frame 声明 dictID 时明确报错，而不是解错）；
  ③ 写 *linked* 帧时其实没有跨块引用（block 层还没有字典），比值等同 independent——帧合法、
  `lz4` 能解，但没有吃到 linked 的收益；④ 有 content size 时解压可以并行（块边界已知），现在串行。

### 工程与测试

- [ ] **E1 Miri**：`rustup toolchain install nightly && cargo +nightly miri test`。
  零 `unsafe` 下主要抓 std 层的别名/UB 误用，便宜保险。
- [x] **E2 随机差分 fuzz**（`tests/fuzz_diff.rs`）：6 种数据形状（单字节重复 / 短周期 /
  文本 / 噪声 / 文本夹随机字节 / 随机长游程）× 12 个边界长度（0、1、4、12、13、64、4096、
  65535、**65547**（byU16 表切换）、65548、100k、300k）的固定网格，加 `LZ4_FUZZ_ROUNDS=n`
  个随机用例。每个用例四判：块字节与 C **相同**、C 能解我们的块、我们解 C 的块（fast 0 级
  与 HC 9 级）、frame 双向（64 KiB / 4 MiB、独立块 / linked、带 checksum 与 content size）。
  失败会打印 seed 与用例（`LZ4_FUZZ_SEED=` 复现）。实测：72 个固定用例 + 三个不同 seed 各
  60 轮全绿；CI 里 Linux 跑 150 轮、Windows 80 轮。
- [x] **E3 平台覆盖**：CI 分别跑 **i686-unknown-linux-gnu**（`gcc-multilib`，真的执行测试：覆盖
  `hc` 里那些 `u32` 索引）和 **s390x** 的 `cargo check`（没有大端 runner 可跑；格式层全程
  `from_le_bytes`，所以"能编"是能诚实主张的上限）。多帧拼接已在 frame 单测里覆盖；>4 GiB 仍未测。
- [ ] **E4 `rust-version = 1.75` 未实测**（只在 1.94 编过）。CI 加一个 `cargo +1.75 check`。
- [x] **E5 文档测试**：`src/lib.rs` 顶部加了 block + frame 两个 API 的示例，`cargo test` 会跑它。给 `lib.rs` 加一段可编译的用法示例，等于多一个测试。
- [x] **E6 `#![deny(missing_docs)]`**：打开后抓到 **34 个**未文档化的公开项（主要是错误枚举的
  字段、`Opts`/`FrameInfo`/`Plan` 的字段、几个方法），逐个补了有信息量的说明（单位、方向、
  谁来决定），构建即门。

### 里程碑（详见 `DESIGN.md` §6）

- [x] **M3a `hc` 3..=9**（`src/hc.rs`）：hash chain 插入/搜索、`_Search2`/`_Search3` 两段前瞻、
  `OPTIMAL_ML` 截断、level 9 的 pattern analysis（`countPattern`/`reverseCountPattern`/
  `patternSegment`）。验收：committed fixture `text-l9.lz4`/`numbers-l9.lz4` 逐字节相同，
  差分 fuzz 在 6 种数据形状 × 12 个边界长度 × 等级 {3,6,9} 上逐字节相同（三个 seed 复核）。
  **踩到的坑**（都写进了提交信息）：C 的 `encodeSequence` 通过指针把 `ip`/`anchor` 一起推进，
  第一版只推进了 `ip`，于是"跳过 1–2 字节"的路径把字面量吞掉，输出环环错位、
  最终 `anchor > ip` panic；把签名改成 `&mut ip, &mut anchor` 后与 C 同构。
- [x] **M3b `hc` 2 与 10..=12**：`lz4mid`（两张平面哈希表：4 字节与 7 字节，各 2^14 桶）+
  optimal parser（价格表 `OPT_NUM=4096`、`PRICE_SCALE=4`、`TRAILING_LITERALS`、反向回溯路径）
  + `chainSwap` 的 `sparsestChain`（M3a 里因为驱动传 `chainSwap=0` 而没有实现，optimal 需要它）。
  `favorDecSpeed` 未实现（参考 CLI 默认为关，且它是 frame 层偏好），模块文档写明这是缺失而非猜测。
  验收：`text-l12.lz4` fixture 逐字节相同；差分 fuzz 覆盖等级 {2,3,9,12}；CLI 端到端
  `lz4r -2/-10/-11/-12` 与 `lz4` 逐字节相同。**0.1.0 的三个判据（frame / HC / CLI）全部达成。**
- [ ] **M4b `frame`**：header/FLG/BD/HC(XXH32)、block header、block/content checksum、
  linked/independent、64 KiB–4 MiB 块、skippable frame；编码器直接消费 `par::compress_each`。
- [x] **M5 CLI**（`cli/`，独立 crate，**GPL-2.0-or-later** + 自己的 `COPYING`）：
  `-1..-12 -d -z -c -t -f -q -v -B4..-B7 -BD -BX --no-frame-crc --content-size
  -T# --single-thread --rm -h -V`、bundled 短选项（`-9f`、`-B4D`）、`-` 作 stdin/stdout、
  多帧文件全解、原子写出（`.tmp` + rename）、`--rm` 仅在成功后删源。
  尚未做：`-m/--multiple`、`--list`、`-b`、legacy `-l`、字典；整文件读入内存（等 M4c 的
  `Read`/`Write` 适配层）。

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

## 第 9 轮：M4d A 步新增代码（`block::Encoder`、`frame::compress` 改道、oracle 新命令）

按《A Philosophy of Software Design》的红旗清单过了一遍，修掉的按"根因"归并：

| # | 红旗 | 位置 | 处置 |
|---|---|---|---|
| R40 | Repetition / Information leakage | `block.rs` `Encoder`：把"偏移 → 内存"的映射（`candidate < start` ? 历史 : 本块；`index - base`）**抄了 5 份**（`is_match`、`count_across`、`match8`、回退扩展、`next_match`） | 收敛成一个 `seg() -> Option<(&[u8], usize)>`，其它地方一律问它。上一轮三个 bug 全出在这 5 份副本的算术上，这就是根因 |
| R41 | 潜在 panic（unknown unknown） | `match8` 先算 `index - base` 再判 `index >= base`；表里留一条过期项、debug 构建下就会 panic | 顺序交给 `seg()`（先判窗口再算偏移） |
| R42 | 正确性（>2 GiB 流） | `advance()` 只在窗口满时重编号，`current` 会一直涨到溢出 u32；C 的 `renormDictT` 在 2^31 就重编号 | 一次位移、两个理由（窗口满 / 偏移空间用掉一半），并加 `debug_assert` 守住 `base >= delta` |
| R43 | Vague name | 字段 `dict` 装的其实是**历史**（可能是上一个块），而 crate 里 `with_dict`/`-D` 才是"字典" | 改名 `history`；`load_dict` 保留（对应 C 的 API 名） |
| R44 | Comment repeats code / 名字与事实不符 | `/// Index of dict[0]`（改名后过时）；`frame::compress` 的文档声称 fixture 覆盖了"linked 路径"（并没有） | 都按事实重写 |
| R45 | 未文档化的接口契约 | `Encoder::compress_into` 没说 `dst` 容量要求和"块会变成历史"；`new` 没说 acceleration 会被夹到 1 | 补接口注释（Before-implementation 的那种） |
| R46 | 误导性工具（工具即文档） | `c_oracle blockdict2` 把"字典"和"两个块"放在不同缓冲区，`dictSize` 涨过字典长度后 C 读越界内存 | 删掉该模式；`DESIGN.md` §11 写明跨块判据只能用真帧，并记下为什么 |
| R47 | Overexposure（**未处置，需要决定**） | `block::Encoder` 是 `pub`，但主路径（历史累积）已知不正确 | 三个选项：(a) 做完它；(b) 从库里删掉（git 里有，`535b5d1`）；(c) 现状。`pub(crate)` 不可行——没人用就是 dead code，CI 的 `-D warnings` 不过。建议在 0.2.0 之前 (a) 或 (b)，不要带着一个"知道不对的公共 API"发版 |
| R48 | 与 C 的已知分歧（**未处置**） | 历史末尾不足 4/8 字节的候选：C 会读过字典缓冲区末尾（未定义），安全 Rust 只能判为"不匹配" | 已在代码注释里写明；接进 frame 层之前要在 `DESIGN.md` 里定案（这会影响 linked 帧的逐字节相同性） |
