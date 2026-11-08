# lz4_in_rust — 设计

> 实现状态与用法见根目录 `README.md`；这份文档记录**决策与依据**（含实测数字与踩过的坑）。

把 C 版 LZ4（`D:\Projects\github\lz4`，v1.11.1）**完全移植**到 Rust，并且**绝对安全**：
`#![forbid(unsafe_code)]`，没有裸指针，任何输入（包括损坏的、恶意构造的）都只会得到
`Err`，不会 panic、越界或挂起。

## 已定的总决策

| 决策 | 选择 | 理由 |
|---|---|---|
| 安全 | 纯 safe，全 crate 禁 `unsafe` | C 用"指针永远合法"的不变式换速度；Rust 用**显式索引不变式**换同样的速度（见 §2）。安全不是事后补的包装层 |
| 范围 | 完全移植：block / HC 3..12 / frame / CLI，且**输出与 C 逐字节相同** | "兼容"降到"同字节"，任何差异都是移植 bug，而不是调参差异——这让差分测试变成判等，而不是判近似 |
| 速度 | **零 unsafe 不破例**：接受纯 safe 的解压上限（match 密集 0.6–0.7x C，长匹配 ~0.9x），不用 unchecked 拷贝层、不用 SIMD intrinsics | 已实测确认：阶梯拷贝 0.59x、窗口切片 1.00x，safe 路径的剩余空间只有 5–10%；要再上一个台阶必须开 unsafe，已明确放弃 |

不做：FFI/ABI 兼容（不做 liblz4 的 drop-in 替身）、`lib/lz4file.c` 的 `FILE*` 层
（用 `Read`/`Write` 适配层替代）、已废弃的 `LZ4_decompress_fast*`（无源大小、不可验证）。
legacy frame（`lz4 -l`，旧 v1 格式）**只做解码**，不做编码——它的编码器连 C 自己都标了 deprecated。

---

## 1. 分层（按"谁知道什么"切，不按执行顺序切）

```
  frame    header/flags/block size/checksum/dictID/linked blocks   ← 唯一知道 XXH32 的人
    │  只依赖 block 的两个函数
  block    token / offset / 扩展长度 / 64K 与 64K 以上的表形切换
    │
  hc       hash chain + optimal parser（levels 2..12；10..12 走 optimal）
  xxhash   XXH32（frame 的 header/block/content 校验）
```

对外接口（深模块：调用者只需知道三行）：

```rust
block::compress_into(src: &[u8], dst: &mut [u8], acceleration: u32) -> Result<usize, Error>
block::decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize, Error>
frame::{FrameEncoder, FrameDecoder}::compress_into / decompress_into   // M4
```

`compress_into` 用 `src`/`dst` 切片而不是 C 的 `(ptr, len, capacity)` 三件套；
`dst.len()` 就是容量，`compress_bound()` 是唯一的"够不够"查询。
容量不足**不是**循环里的判断，而是入口一次判断（"把错误定义掉"），因此编码热循环
里没有任何容量检查。

---

## 2. 解压热路径：绝对安全怎么拿到速度

结构上把解压分成两段（这是 C 的做法，也是唯一能让纯 safe 跑起来的结构）：

```
loop {
    while ip + 48 <= src_len && op + 64 <= dst_len {   // 快速段：整个序列的读写都由
        ...                                            // 入口两个不等式保证，序列内
    }                                                  // 零边界检查、零 Result
    // 检查段：一个序列，每一次读写都验证（长度扩展、长匹配、块尾、错误输入）
}
```

快速段的两个不等式覆盖一次序列的全部访问：最多读 `token + 1 个长度字节 + ≤32 字面量
+ 2 字节 offset`（48 足够），最多写 `32 字面量 + 32 匹配`（64 足够）。覆盖实测分布：
96% 的匹配 ≤ 32 字节、95% 的 offset ≥ 32、99% 的字面量 < 15。剩下 ~13% 的序列落到检查段。

**为什么"多写"是安全的**：宽搬运固定 32 字节，会写到一次 run 的逻辑结尾之后。输出是
从头往后产生的，`op` 之后的字节还没被写；而匹配只能引用 `op` 之前的字节，且后续 run 的
写必然从 `op` 起覆盖 ≥8 字节 —— 所以这些多余字节永远读不到，也一定在被"变成输出"之前被
覆盖。测试里 `dst` 预填 `0xAA` 而不是 0，就是为了抓任何"依赖未写字节"的错误。

**为什么小 offset 仍然正确**：offset ≥ 32 才用一次 32 字节读（源全在 `match_at` 之前）；
offset 16..32 用 2×16 字节、8..16 用 4×8 字节（每次读的源都已写好）；offset < 8 退回
**倍增** `copy_within`（chunk = min(written, remaining)，`O(log len)` 次 memmove），
精确复现格式要求的"重复模式"语义。

试过并**测掉**的两种写法（都是 0.6–1.0x，没有采用）：

| 尝试 | 结果 |
|---|---|
| 按长度阶梯选 8/16/32 字节搬运（省写带宽） | 0.59x —— 分支变多，反而更慢 |
| 每轮先切 48/64 字节窗口切片，让内部偏移变成常量 | 0.98–1.00x —— LLVM 本来就已经消掉了这些检查 |

### AVX2 与"零 unsafe"

- **`std::arch` intrinsics（`_mm256_loadu_si256` 等）是 `unsafe fn`，与 `#![forbid(unsafe_code)]` 冲突，本移植不使用。**
- 但宽搬运是 `[u8; 32]` 的安全数组拷贝，**编译器会按目标特性生成 1 条 ymm（AVX2）或 2 条
  xmm（SSE2）指令**。所以 AVX2 支持 = 编译期选择：
  `RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release`，代码里仍是零 unsafe。
- 实测收益：match 密集语料 0.63x → 0.64x（噪声内），长匹配语料 0.88x → 0.89x。**LZ4 解压
  的瓶颈是每序列的分支和边界检查，不是搬运宽度**，所以 AVX2 在解码上只值几个百分点。
- 运行时 CPU 分派（`is_x86_feature_detected!` + 调用 `#[target_feature]` 函数）在 safe
  Rust 里做不到：调用带 `target_feature` 的函数需要 `unsafe`。所以只能整 crate 编译期选定。

## 3. 逐字节相同的那些坑（移植中真实踩到的）

1. **`byU32` 用的是 5 字节哈希**（64 位机上 `LZ4_hash5`：`(seq << 24) * 889523592379 >> 52`）。
   照 `LZ4_hash4` 的风格写 4 字节哈希 → 200 KB 输入差 3.5%，序列数 19852 vs 15771。
   这是最容易漏的一条：文档里没写，只有读 `LZ4_hashPosition` 才看得见。
2. **小于 `LZ4_64Klimit`（65547 B）的输入用 `byU16` 表**：多一个哈希位（13 bit / 8192 槽），
   同样的 `hash4`。表形影响输出，所以要两条实例（`run::<true>` / `run::<false>`）。
3. **"探测下一个位置"用的是 `ip-1` 的候选**：`ctx = table[hash(ip-1)] + 1`，且要求
   `match8(ctx, ip)`（8 字节相等）才采用——用来打断周期数据上的 4 字节短匹配链。
   C 里对应 "Fill table, and test next position"。
4. `mflimit` 是 `iend - MFLIMIT + 1`（允许的最后一个**匹配起点**），差一就换输出。
5. 匹配长度上限是 `matchlimit = len - LASTLITERALS`；`count_match` 先按 8 字节比较，
   不同时用 `trailing_zeros()/8` 定位第一个不同字节。

结论：**现在的 `level 0`（fast 档，含 acceleration）输出与 C 逐字节相同**，
`tests/interop.rs::level_0_output_is_byte_identical_to_c` 是判等断言，不是近似断言。

---

## 4. 现状

| 模块 | 状态 |
|---|---|
| `xxhash` | ✅ 一次性 + 流式，对 C 的 7 个语料 + 4 个标准向量判等 |
| `block::decompress_into` | ✅ 解 C 压的所有 level（0/9/12）、所有分布；损坏输入只返回 `Err` |
| `block::compress_into` | ✅ 逐字节等于 C（level 0 / acceleration 任意） |
| `level` | ✅ `-1..-12` → 解析器的唯一映射（`src/level.rs`，4 个单测） |
| `par`（并行块引擎 + 自适应线程数） | ✅ `plan`/`Control` 纯函数 + 池 + 按序流式交付；15 个单测 |
| `frame`（流式 `Encoder: Write` / `Decoder: Read`，无字典） | ✅ `src/frame.rs` + `src/frame/tests.rs`；15 个单测 + 5 个与 C 的差分测试 |
| `hc`（2 = lz4mid、3..=9 = hash chain、10..=12 = optimal） | ✅ `src/hc.rs`；每个等级都与 C 逐字节相同（fixture + fuzz 双证） |
| `frame` | ⬜ M4 |
| CLI | ⬜ M5 |

测试：`cargo test` 53 个测试。核心是与 C 的**差分**：固定语料（不需要 C 工具链，语料已提交在
`tests/fixtures/`；`LZ4_C_ORACLE=target/c-oracle.exe cargo test` 还会跑"C 解我们的输出"）。

---

## 5. 实测（这是判断"要不要用 unsafe"的依据）

方法：C 与本移植交替运行、每方取 30 次的最优值、重复 9 轮取**中位数比值**（这台笔记本
的睿频在 0.3–1.0x 之间漂移，单次绝对值不可比）。同一进程内、同数据、同 block API。

语料 A：1.2 MB 真实文本（`lib/*.c`+`doc/*.md`+`tests/unit/*.c`），96 515 序列，
平均匹配 11 B、平均字面量 1.5 B、95% 的 offset ≥ 32。

| | C | 本移植（SSE2 基线） | 本移植 `-C target-cpu=x86-64-v3` |
|---|---|---|---|
| 压缩输出 | 443 395 B | **443 395 B（逐字节相同）** | 同 |
| 解压（中位数比值） | 2251 MiB/s | **0.63x**（单轮 0.31–1.00x） | 0.64x |
| 压缩 | — | 0.66–0.94x | — |

语料 B：8 MB 强重复（长匹配，memmove 友好）：解压 **0.88x**（单轮最高 1.09x）。

结论：
1. 已经比 C 快的场景：长匹配/低压缩比数据（memmove 比 C 的 8 字节 wildcopy 快），
   单轮可到 1.1x，但中位数仍 0.88x，不能说"稳定更快"。
2. **纯 safe 的天花板就是 0.6–0.7x（match 密集）**。差距的来源正好是 C 没有的那部分：
   每序列 2–4 个边界检查 + panic 分支。这不是写法问题——阶梯版、窗口版都试了并测掉了。
3. 要真正**稳定**超过 C，只剩一个杠杆：把快速段的读写换成 unchecked（一个 20–30 行的
   `unsafe` 拷贝层，边界在入口检查一次；lz4_flex 就是这条路）。这违反"零 unsafe"，
   所以需要你决定（见 §7）。

## 6. 里程碑

| # | 内容 | 判据 |
|---|---|---|
| M0 ✅ | crate 骨架 + C 差分脚手架 + `xxh32` | `cargo test` 绿；xxh32 与 C 判等 |
| M1 ✅ | `block::decompress_into` | 解 C 全部语料；损坏输入不 panic |
| M2 ✅ | `block::compress_into` + `compress_bound` | 与 C **逐字节相同**；C 能解我们的输出 |
| M3a ✅ | `hc` 3..=9（hash chain + 两段前瞻 + level 9 的 pattern analysis） | `-3..-9` 与 C 逐字节相同（committed fixture + 差分 fuzz） |
| M3b ✅ | `hc` 2（`lz4mid`）与 10..=12（optimal parser + 价格表 + chainSwap） | `-2`/`-10..-12` 与 C 逐字节相同（fixture、差分 fuzz、CLI 端到端三处判等） |
| M4a ✅ | `par`：并行块引擎 + 策略（`plan` / `Control` / 池） | 输出与线程数无关；小主机/小块/小输入自动单线程；见 §9 |
| M4b ✅ | `frame`：descriptor/blocks/两个 checksum/linked+independent/skippable，复用 `par::compress_each` | 我们的帧与 C **逐字节相同**；C 能解我们的帧；linked 多块帧能解（含跨块引用） |
| M4c ✅ | `frame` 流式：`Encoder<W: Write>` / `Decoder<R: Read>`，内存 O(块×worker)；`--content-size` 只在长度已知时写入 | 文件/管道任意大小；分块大小无关（帧字节相同）；CLI 端到端 8 MiB + 管道测试 |
| M4d | 字典（`createCDict` 语义）、写 linked 帧时真的跨块引用 | 与 `lz4 -D` 互通 |
| M5 | CLI：`-1..-12`、`-d`、`-c`、`-t`、`-B#`、`--list` | 能替代日常 `lz4`；`lz4 -t` 通过 |

---

## 7. 待你拍板

1. **AVX2 的落地方式**：现在只能整 crate 编译期指定 `RUSTFLAGS="-C target-cpu=x86-64-v3"`，
   收益 ~1–2%（解码瓶颈是分支不是搬运宽度）。要不要固化进 `.cargo/config.toml`/`Makefile`？
   固化后二进制在没有 AVX2 的老 CPU 上会非法指令，所以默认我倾向**不固化**，只在文档里写。
2. HC 的判等基准：`-9`/`-12` 也要求逐字节相同（要照抄 `optimal parser` 的每个分支，
   以后就无法改进 HC），还是"能互解 + 压缩率差 <0.5%"？
3. 下一个里程碑：先 HC（M3）还是先 frame（M4）+ CLI（M5）？frame 能立刻让 CLI 可用。
4. 要不要按 M0/M1/M2 分三个 commit？

## 8. 并发

目前**单线程，0 个后台线程**：`src/` 里没有 `thread`/`spawn`/`Mutex`/`async`，
`Cargo.toml` 没有任何依赖，bench 测的就是单核吞吐。

### 为什么 block 层不并行

LZ4 的并行度只在 frame 层：一个 block 内部有匹配依赖（回指最多 64 KiB），压缩和解压都是
串行链。C 也一样——`programs/lz4io.c:1758` 明确要求 worker > 0 时必须 `LZ4F_blockIndependent`，
即先放弃跨块匹配换取并行。

### 旧探针实测（静态分片，32 MB 文本）——用于说明"为什么不能靠启动微基准"

单线程基线在这台机器（i7-1260P：4 P-core + 8 E-core，12 线程）上会因睿频在 **3 倍范围内
漂移**，所以下面的加速比是同一进程内、同一轮测出的比值，仍只能当量级看：

| 线程 | 压缩 | 解压 | 说明 |
|---|---|---|---|
| 2 | 1.8–2.0x | 1.3–2.0x | 接近线性 |
| 4 | 2.2–3.2x | 1.3–3.1x | 4 个 P-core 时接近线性，落到 E-core 就掉 |
| 8 | 3.4–5.5x | 1.7–4.0x | 被 E-core 和内存带宽拉住，收益递减 |

**早先"解压不涨"的结论是错的**：那是每次 `thread::spawn` 的开销（30–80 µs/块）造成的；
换成常驻池后解压同样接近线性。

### 真正重要的约束：job 大小

同一份 32 MB 文本，按固定 job 大小切块（4 线程）：

| job 大小 | 体积代价 |
|---|---|
| 4 MB | +0.01% |
| 1 MB | +0.03% |
| 256 KB | +0.14% |
| 64 KB | **+13.7%** |

所以 M4 的 MT 方案：worker 池（`available_parallelism()`）＋ **job = 4 MB 块**（`-B7`）＋
`blockIndependent`；64 KB 块不要并行（比值损失吃掉全部收益）。

### 自适应线程数：能做，但别靠"开机跑个微基准"

`examples/parallel_probe.rs calibrate` 就是那种做法（k=1..8 各测一遍，取"距最优 15% 内的最小 k"）。
**实测证明它不可靠**（32 MB 文本，10 轮取最优）：

| k | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| compress | 1.00x | 1.41x | 1.87x | 1.96x | 2.74x | **3.34x** | 3.22x | 2.61x |
| decompress | 1.00x | 1.21x | 1.21x | **2.13x** | 1.23x | 1.88x | 1.94x | 1.79x |

曲线**非单调**（5 线程比 4 慢、8 比 6 慢），相邻 k 的抖动 ≈ ±40%，和真实增益同量级。
原因：这颗 i7-1260P 是 4 P-core + 8 E-core 的大小核，加上睿频/温度墙；而
`available_parallelism()` 在这台机器上返回 **16**（实际 12 线程）——虚拟机/容器里也不可信。
更差的版本（3 轮、遇到第一个 <15% 就停）会选出"2 个线程""1 个线程"，纯噪声。

### 结论：用反馈，不用微基准；具体规则见 §9

1. **动态任务队列**（`AtomicUsize` 游标）而不是静态分片：慢核/E-core/后台占用自然被吸收，
   也不需要猜对线程数。
2. **跨窗口持续反馈**：吞吐量本来就在算，按块边界评估，带滞回，一次只动一个 worker。
3. **物理门槛用相对量表达**（核数、`blockSize`、`input`），不写死本机数字。

### M4 的接口含义

- `FrameEncoder`：可选 worker 池，`nb_workers` 语义对齐 `lz4 -T#`，> 0 时强制
  `blockIndependent`（否则拒绝，和 C 一致）。
- `FrameDecoder`：只在 `blockIndependent` 帧上并行；`linked` 帧（默认）保持串行。
- 解压并行的额外代价：输出必须按块收集再按序拼接，峰值内存 = 块数 × 块大小。


## 9. 并行优化策略（M4 落地用）

**原则：不写死任何"本机数字"。** 只有格式常量（`blockSize ≤ 4 MB`）、内存预算和相对阈值；
所有绝对量都从 `available_parallelism()` 和 `blockSize` 推导，因此在 1 核容器、4 核老机器、
16 核服务器、被 `taskset`/cgroup 限流的进程上都是同一套逻辑。

目标函数：端到端 wall time 最小；约束：① 体积损失 < 0.5%；② 输出与线程数无关；③ 峰值内存
≤ `k × blockSize × 2`。

### 决策顺序（先做大杠杆，别先调线程数）

| 步 | 决策 | 规则（可移植） |
|---|---|---|
| 0 | 要不要 MT | `avail ≥ 2` **且** `blockSize ≥ 1 MB` **且** `input ≥ 8 × blockSize` 且用户未显式指定 → 否则单线程。三条都对应用户可见的物理事实：核太少、块太小（体积代价）、job 太少（开销比收益大） |
| 1 | **job 大小**（最大杠杆） | `job = blockSize`（格式决定，默认 `-B7`=4 MB）；`blockSize < 1 MB` 时**不开 MT**，而不是偷偷用小块并行 |
| 2 | 块独立性 | MT 时强制 `blockIndependent`（与 `lz4 -T#` 一致，`lz4io.c:1758`）；`-BD`（linked）拒绝 MT |
| 3 | 初始线程数 | **小机用满核**：`k0 = if avail ≤ 4 { avail } else { max(1, avail - (1 + avail/8)) }`；再夹到 `[1, min(blocks, budget/(2×blockSize))]`。大机上后半段与参考 `LZ4IO_defaultNbWorkers()` 同式 |
| 4 | 反馈调整 | 每个采样窗口评估一次，带滞回，一次只动 ±1，区间 `[max(1,k0/2), min(2×k0, avail)]` |
| 5 | 调度 | 常驻池 + `AtomicUsize` 游标扫 job 数组（自平衡，不猜大小核） |

`k0` 在大机上取参考实现的同一条公式，是为了 CLI 可对比；`avail ≤ 4` 的特例是实测结果：
2 核机器上"留 1 个核"等于直接退回单线程（1.0x），而用满 2 核实测 **1.59–1.66x**——小机没有
可省的余量，OS 抢占本来就会让出时间片。

### 反馈循环（全部 safe std，10 行）

```text
window = max(8 × blockSize, 8 MiB)     # 相对量：块越大越少采样
state: k = k0, best = 0, cooldown = 0, elapsed = 0, bytes = 0

每写完一个 block（块边界，不在块中间）:
    elapsed += 本块耗时; bytes += 本块输入大小
    if bytes < window: return
    now = bytes / elapsed
    if now > best * 1.10: best = now
    elif now < best * 0.80 and cooldown == 0: k = max(k_lo, k - 1); cooldown = 3
    elif cooldown == 0:                       k = min(k_hi, k + 1)
    cooldown = cooldown.saturating_sub(1)
    elapsed = 0; bytes = 0
```

- 只与**淡入的高点**比较（不比上一个窗口），一次噪声不会让 k 回跳；
- 阈值 10%/20% 远大于相邻 k 的噪声幅度（本机实测相邻 k 抖动 ≈ ±40%，所以更小的阈值无意义）；
- 调整频率 ≤ 1 次/窗口，代价可忽略。

### 内存预算（默认 64 MiB，`--memory-budget` 可调，语义对齐 liblz4 的 `memoryBudget`）

`k ≤ budget / (2 × blockSize)`：4 MB 块 → 上限 8 个 worker（= 64 MB 峰值）。16 核机器上不做
`k = 16 × 4 MB × 2 = 128 MB` 的默认开销；要大吞吐由用户显式给预算。

### `-T` 语义（与 1.11 参考一致，别自己发明）

| 旗标 | 行为 |
|---|---|
| 默认 / `-T0` | auto：`k0` + 反馈 |
| `-T#` | 固定 `#` 个 worker，跳过反馈 |
| `--single-thread` | 1 个线程，等价当前实现（`-BD` 时与 `-T1` 输出可能不同，与参考一致） |

### 其它必须一起做的优化（否则 MT 会被别处卡住）

1. **零拷贝写出**：块压完直接按序写进 frame 输出，不做第二遍拼接。
2. **写盘串行化**：单线程按序写，worker 只负责压/解（避免多线程争同一个 `Write`）。
3. **解压只在独立块帧并行**；linked 帧串行——但此时默认 4 MB 块 + §2 的 memmove 路径
   本来就能到 10–20 GiB/s。
4. **不做** CPU 亲和性 / P-core 探测 / NUMA 绑定：safe std 拿不到，收益远小于游标队列自平衡。

### 验收矩阵（必须多环境，不能只在一台机器上验证）

| 用例 | 判据 |
|---|---|
| 32 MB 文本，`-T1/-T4/-T8` | 输出**逐字节相同**（与线程数无关）；`-T1` 等于单线程结果 |
| 同上，吞吐 | `-T4` ≥ 2.5x `-T1`（压缩），解压 ≥ 2x（在 ≥4 核的机器上） |
| `taskset -c 0`（1 核） | `avail = 1` → 自动单线程，与关掉 MT 完全一致 |
| `taskset -c 0-1`（2 核） | 不劣于单线程（≥1.0x），否则策略退化 |
| 8 MB / 1 MB 输入 | 不满足 `input ≥ 8×blockSize` 时不开 MT；开了也不劣于单线程 |
| `-B4`（64 KB 块） | 自动单线程（体积代价 +13.7% 不可接受） |
| 后台抢占（占一半核跑 `yes`） | 反馈循环 2 个窗口内降 k，吞吐不低于静态 k 的 90% |
| 比值 | `-T4 -B7` 相对单块 < 0.5% |
| 内存 | 峰值 < `k × blockSize × 2` |

### 实现状态（`src/par.rs`）

接口（深模块：线程池、队列、控制器都在里面）：

```rust
pub enum Workers { Auto, Fixed(usize), Single }
pub struct Opts { workers: Workers, block_size: usize, memory_budget: usize, level: Level }
pub fn plan(opts, input_len) -> Plan { workers, block_size, blocks }              // 纯函数
pub fn compress_each(src, opts, sink: |index, bytes|) -> Result<(), E>           // 按序流式
pub fn decompress_into(blocks, out: &mut [u8], opts) -> Result<(), Error>
pub fn decompress(blocks, original_len, opts) -> Result<Vec<u8>, Error>          // 便利包装
// 另外两个 `_with(…, avail)` 变体与 `plan_with` 只给测试用，因此是 crate 私有的：
// 想指定线程数的调用方用 `Workers::Fixed`（CLI 的 `-T#`），而不是自己编一个核数。
pub const MAX_BLOCK_SIZE: usize = 4 << 20;   // 格式事实（`-B7`）；其余四个策略常量是 pub(crate)
```

- 队列：`AtomicUsize` 游标，worker 取号即走。压缩用"完成窗口"（`Mutex<BTreeMap>` + `Condvar`）
  交给调用线程**按序**流式取走，窗口 ≤ `workers+1` 块 —— 这就是 frame 编码器要的形状；
  解压用 `Mutex<Vec<(usize, &mut [u8])>>` 发放互不重叠的输出切片 → **零 unsafe、零热路径锁**。
- 解压**不自己分配**输出：`decompress_into` 拿调用者的缓冲。之前 `decompress` 每次
  `vec![0; 32MB]` 让"单线程"路径看起来慢了 21%（清零/缺页），改成复用后回到 1.00x。
- 控制器只能在一个调用内**降** k（线程已建），升 k 发生在调用之间（下次从上次的高点起算）。
- 实测（32 MB 文本，4 MB 块 = 8 jobs，`examples/parallel_probe.rs`）：

| 配置 | 压缩 | 解压 | 体积 |
|---|---|---|---|
| block API 单块 | 441 MiB/s | 2459 MiB/s | 15 145 816 B |
| `Single` | 443 (**1.01x**) | 2449 (**1.00x**) | +0.01% |
| `Fixed(2)` | 766 (**1.74x**) | 3904 (**1.59x**) | +0.01% |
| `Fixed(4)` | 1243 (**2.82x**) | 6038 (**2.46x**) | +0.01% |
| `Fixed(8)` | 1460 (3.31x) | 6559 (2.67x) | +0.01% |
| `Auto`（8 jobs → 8 workers） | 1367 (3.10x) | 6205 (2.52x) | +0.01% |

（同一台 4P+8E 笔记本，绝对值随睿频漂移；这里看比值。）

### 已验证的退化行为（`taskset` 实测，不是推断）

| 场景 | 结果 |
|---|---|
| `taskset -c 0`（1 核） | `Auto` → **1 worker**，与 `Single` 同速；`Fixed(8)` 在 1 核上是 0.75x（说明门槛必要） |
| `taskset -c 0-1`（2 核） | `Auto` → **1 worker**（参考公式 `k0 = 2-(1+0) = 1`），但 `Fixed(2)` 实测 **1.62x** |
| 小块（4 KB） / 小输入 | `plan` 直接返回 1 worker |
| 输出与线程数无关 | `Single/Fixed(1,2,3,8)/Auto` 的块字节完全相同（单测） |

**已定（选 b）**：`avail ≤ 4` 用满核。`taskset` 实测（32 MB 文本，8 jobs）：

| 核数 | `Auto` 选的 worker | 压缩 | 解压 |
|---|---|---|---|
| 1 | 1 | 0.99x（= 单线程） | 1.15x |
| 2 | 2 | **1.59x** | **1.66x** |
| 4 | 4 | **3.41x** | **2.50x** |

代价：`avail ≤ 4` 时默认线程数与 `lz4` 不同（它给 1–3 个），这是自觉的取舍——小机上
CLI 默认行为对我们更有利。大机（≥ 5 核）仍与参考同式。

参考点（仅供对照，不作为默认值来源）：在 4 P-core + 8 E-core 的 i7-1260P 上，
`k0 = 12 - 2 = 10`，实测最好落在 4–6（受内存带宽和 E-core 拖累），
即反馈循环应当把 k 从 10 降到 5–6 附近——这正是它在别的机器上要独立完成的工作。

## 10. 代码评审

评审结论、已修项与后续计划清单都在 **`docs/REVIEW.md`**（含每项的证据、最小修法与验收方式）。
这里不重复，只保留一条判断：纯 safe 的上限（§5）和 frame 落地前的结构债（`level` 模块、
`compress_each`）都已处理，剩下的都是 E1–E6 的工程项与 M3/M4b/M5 的里程碑。


## 11. M4d 编码半边：用字典压缩 / 让 linked 帧真的链接（设计与计划）

**目标**：`frame` 的 `BlockMode::Linked` 真正跨块引用，`lz4r -D dict -1 file` 与
`lz4 -D dict -1 file` 输出**逐字节相同**。解码半边已完成（§4/§10），所以这一半只关乎写。

### 为什么不能"顺手加个参数"

参考实现的字典压缩是**有状态**的：`LZ4_loadDict` 把字典的位置灌进哈希表、`currentOffset`
前移 64 KiB，之后每个块都用同一个 `LZ4_stream_t` 调 `LZ4_compress_fast_continue`；块的候选
可能落在**两个缓冲区**（字典 / 当前前缀），索引空间靠 `dictBase = dictionary + dictSize - startIndex`
的偏移换算统一。我们的 `block::compress_into` 是**无状态**的（每次新建表），所以这不是加参数，
而是新增一种编码器。

### 接口（先写接口注释，再实现）

```rust
// block.rs
pub struct Encoder { /* 哈希表 + 字典状态，相当于 LZ4_stream_t */ }
impl Encoder {
    pub fn new(acceleration: u32) -> Encoder;
    /// `LZ4_loadDict`：只保留最后 64 KiB，位置索引从 currentOffset 起。
    pub fn load_dict(&mut self, dict: &[u8]);
    /// `LZ4_compress_fast_continue`：压缩一个块，可引用字典 / 之前压过的块。
    /// 返回压缩大小；调用方负责把它们按序写出。
    pub fn compress_into(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize, Error>;
}
```

`frame::Encoder` 在 `BlockMode::Linked` 时持有这个 `block::Encoder`，**每块只是再调一次
`compress_into`**：状态自己推进（块成为历史，窗口裁到 64 KiB），`load_dict` 只在
"用户给了独立字典文件"时调一次 —— 每块重新 `load_dict` 会清表并打断索引连续性，那不是 C 的行为。

C 的 linked 帧走的是 `withPrefix64k`（`dictEnd == source`，历史紧邻当前块），`lz4 -D` 走
`usingExtDict`（两个缓冲区）。两者在我们的实现里共用同一个索引空间（历史 + 当前块），区别只在
R48 那处越界读（见 §11 末节）。

### 要移植的 C 函数（按依赖顺序）

1. `LZ4_renormDictT`（`currentOffset` 逼近 2 GiB 时重整表）——只在超长输入时才触发，但**必须**
   一致，否则索引不同、输出不同。
2. `LZ4_loadDict_internal`（`_ld_fast`）：`resetStream` → `currentOffset += 64 KiB` →
   循环 `p += 3` 写表。
3. `LZ4_compress_generic` 的 `usingExtDict` 维度：
   - 前缀/字典两套 `match` 定位与 `lowLimit`；
   - 匹配在字典里时"数到字典末尾再从前缀继续数"（`matchLength` 的双段计数）；
   - `_next_match` 的 `inDict` 分支与 8 字节预检的字典边界；
   - `prefixIdxLimit` / `dictSmall`（字典 < 4 字节或表项早于窗口时跳过）。
4. `LZ4_compress_fast_continue` 的状态推进：`dictSize += inputSize`、`currentOffset += inputSize`、
   以及"输入与字典重叠"的裁剪（frame 层不会重叠，可 assert）。
5. HC 侧（levels 3..=12 的 linked）：`LZ4HC_InsertAndGetWiderMatch` 的 `usingExtDict` /
   `usingDictCtx` 分支 + `LZ4HC_Insert` 的跨缓冲区插入。**这是第二个大块**，建议与第 3 步分开做。

### 不可省略的取舍

- **linked 模式无法用 `par`**：块之间有依赖，必须串行。CLI 在 `-BD` + `-T#` 时的行为要与参考一致
  （`lz4` 本身把 workers 降为 0）。
- 逐字节相同意味着不能"改进" extDict 的候选顺序；和 HC 一样，接受"永远跟随 C"。

### A 步：帧的 linked 写路径（已完成）

`block::Encoder`（前缀模式的 `LZ4_compress_fast_continue`）落地，`frame::Encoder` 在
`Linked` + level 1 时逐块串行压缩；输入 150000 字节、64 KiB 块的三块 linked 帧与 C 的
`LZ4F_compressBlock_continue` **逐字节相同**（fixture `frame/large-b4-linked-plain.lz4`，
8766 字节，比 independent 少 4.2%）。

试做时踩的坑（第一次做 A 步时失败的原因，均为索引空间算术）：

1. **重编号后 `base` 不能动**：丢掉最旧的 `delta` 字节时，保留的字节仍在 `base`，只有
   `current` 减去 `delta`；改动 `base` 会让 `current == base + history.len()` 破掉。
2. **索引空间必须从一个非零值开始**（`HISTORY_START = MAX_DISTANCE + 1` = `LZ4_loadDict` 的做法），
   否则"已丢弃的表项"饱和到 0 之后会**冒充**仍然有效的第 0 字节。
3. **回退（catch up）只在"搜索"路径做**：`ip-1` 探测命中后 `ip == anchor`，参考实现是跳过回退
   的（`goto _next_match`），照抄回退就会把上一个匹配的尾部吞掉（`anchor > ip` panic）。
4. `LZ4_isDictSmall` 在这条路径上永远为假（它需要"部分字典"），因此 `prefix_idx_limit` 那套
   分支在帧路径里不需要 —— 等 `loadDict` 落地时再补。

`-D` 的**编码**（`lz4 -BD -D`）随后也做完了：`block::Encoder::load_dict`（`LZ4_loadDict`：
reset、`currentOffset += 64 KB`、保留最后 64 KiB、每 3 字节填表）+ `frame::Encoder::with_dict`。
判据是两个 fixture 都逐字节相同：单块 `frame/text-dict.lz4`（2931 字节）与三块
`frame/large-b4-dict.lz4`（7549 字节）；40 KB 与 64 KB 两种字典也都试过。

注意 `LZ4_isDictSmall` 分支**不需要**：它只为"部分加载的字典"服务，而 `seg()` 对落在窗口之外的
索引一律返回 `None`（等于参考实现的 `prefixIdxLimit` 拒绝），两者效果相同 —— 这也是把索引空间
从两个窗口处开始（`HISTORY_START`）换来的。

仍然没做：
1. HC 档（2..=12）的 linked 搜索（B 步）。
2. **独立块** + 字典（参考实现走 `usingDictCtx`：每块用 `dictCtx` 的两张表查候选）。
3. HC 档 + 字典（同理需要 B 步）。

这三件都没做之前，`-BD` 在 level ≥ 2、以及 `-D` 不配 `-BD` 时，CLI 会明确报错/提示，而不是写出
一个"看起来有字典、其实没用上"的帧。

### A 步试做记录（代码已删，结论保留）

试做过一版 A 步（git 历史 `535b5d1`、`bdbdb0a`）：首块（`loadDict` + 一次 `compress_into`）与 C
**逐字节相同**，跨块历史累积不对 —— 同样两块 64 KiB，C 的第二块 2208 字节，那一版 5886。按评审
结论从库里删掉了：一个已知不对的公共 API 比没有更糟。留下的结论：

1. **索引→内存的映射只能有一份实现**（`seg(index) -> Option<(&[u8], usize)>` 形态：`index < current`
   走历史，否则走本块）。试做版把它抄了 5 份，三个 bug 全出在这 5 份副本上：匹配长度从候选**处**
   开始数（应从候选 + 4 处）、回退扩展比较候选**处**的字节（应比较候选**前**一个字节）、跨段计数
   读错了缓冲区。
2. `renormDictT` 有**两个**触发理由、同一个位移操作：窗口满，以及 `current` 逼近 2^31。只做前者，
   >2 GiB 的流会把 `u32` 的 `current` 溢出。位移不改变任何距离，所以不改变任何决策。
3. **调试方法**：把两边的块解析成 `(literals, offset, matchLen)` 序列逐条比，第一个分歧点就是 bug
   位置。试做版的三个 bug 都是这样找到的，比看输出总大小快得多。
4. 跨块状态推进的判据只能是**真帧**（多块 linked 帧，prefix 模式）；`c_oracle blockdict`（单块 +
   字典）可留作首块的判据。

### 验证计划（先建 oracle，再写代码）

1. `c_oracle blockdict <dict> <in> <out>`：`LZ4_loadDict` + 一次
   `LZ4_compress_fast_continue`（首块即字典块）。**注意**：只用它验证首块。
2. 跨块只能用真帧：`lz4 -BD`（或 `c_oracle frame 1 4 1 0 1 0`）产出的多块 linked 帧。不能自己
   拼"`loadDict` + 两个不连续缓冲区"的 oracle（试做版有 `blockdict2`，已删）：那样 `cctx->dictSize`
   会超过字典缓冲区的长度，`dictEnd` 指到分配之外，C 自己就在读越界内存 —— 输出的 3902 字节不是
   有效参考，试做时曾据此误判。
3. 帧级：`c_oracle frame-dict`（已存在）产出的 fixture，要求我们的 linked 帧与它**逐字节相同**；
   再在 fuzz 里加"随机字典 + 随机数据"的一轮。
4. 失败模式：字典不足 4 字节、字典 > 64 KiB、块 < 13 字节、块与字典完全无匹配。

### 分两步落地

| 步 | 内容 | 判据 |
|---|---|---|
| A | fast 档（level 1）的 extDict 编码器 + frame 的 linked 写路径（fast 档） | 首块对 `blockdict`、多块对 `lz4 -BD` 的整帧，逐字节相同 |
| B | HC 档（2..12）的 extDict 搜索 | 同上，用 `-BD -9` 的 fixture |

在 A、B 都完成之前，`-BD` 写出的帧**仍然不会跨块引用**（帧合法、`lz4` 能解，只是没吃到
linked 的收益），CLI 会就这件事给一句提示。

### 历史末尾不足 4/8 字节的候选（R48 定案）

C 在字典边界上直接 `LZ4_read32`/`LZ4_read64` 候选位置，字典位于缓冲区末尾时会**读过界**。两种模式
结局不同，必须分开处理：

- **`withPrefix64k`（linked 帧）**：历史紧邻当前块，读过的字节就是**本块开头**，有定义 → **必须复现**。
  实现时把历史与本块看成一条连续流：4/8 字节的比较跨边界拼接，匹配计数越过边界后继续在本块里数
  （试做版的 `count_across` 就是这个思路，方向是对的）。
- **`usingExtDict`（`lz4 -D` 压独立字典文件）**：读过的是该分配之外的内存 → C 自己的行为未定义，
  同一输入换个分配器就可能不同。这里**不追求逐字节相同**：安全 Rust 判为"不匹配"。这是全项目唯一
  一处主动与 C 不同的地方，做 `-D` 压缩时要在 README 的"逐字节相同"说明里点出来。
