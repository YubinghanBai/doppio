# Doppio — 高性能 Rust 缓存库设计文档

> 对标 Java Caffeine & Go Ristretto，追求 Rust 语境下最优的命中率、吞吐量与内存安全。

---

## 目录

1. [背景与动机](#1-背景与动机)
2. [现有 Rust 缓存生态分析](#2-现有-rust-缓存生态分析)
3. [Caffeine 架构深度解析](#3-caffeine-架构深度解析)
4. [Ristretto 架构深度解析](#4-ristretto-架构深度解析)
5. [核心算法：W-TinyLFU](#5-核心算法w-tinylfu)
6. [Doppio 整体架构设计](#6-doppio-整体架构设计)
7. [核心数据结构](#7-核心数据结构)
8. [Rust Trait 抽象体系](#8-rust-trait-抽象体系)
9. [并发模型与内存模型](#9-并发模型与内存模型)
10. [过期策略设计](#10-过期策略设计)
11. [模块结构](#11-模块结构)
12. [演进路线图](#12-演进路线图)
13. [第一步：如何开始](#13-第一步如何开始)

---

## 1. 背景与动机

### 为什么需要 Doppio？

| 维度 | Java Caffeine | Go Ristretto | Rust moka | **Doppio 目标** |
|------|--------------|-------------|-----------|-----------------|
| 命中率算法 | W-TinyLFU (最优) | SampledLFU | W-TinyLFU | W-TinyLFU + 改进 |
| 并发模型 | 条带化读缓冲 + MPSC 写缓冲 | Ring buffer goroutine | Arc + DashMap | 零锁竞争路径 |
| 内存安全 | GC | GC | Arc 引用计数 | Arc + 精细控制 |
| 零成本抽象 | 无 (JVM 开销) | 无 | 部分 | Trait + Monomorphization |
| Cost-aware 淘汰 | 支持 | 支持 (核心) | 支持 | 支持 |
| TTL/TTI | TimerWheel | 独立 goroutine | 支持 | 分层时间轮 |
| async 原生 | 无 | 无 | 支持 | feature-gate 支持 |

### Rust 缓存库的现有痛点

- **moka**：功能最完整，但大量使用 `Arc<Mutex<...>>`，写路径有明显锁争用；异步版本引入额外调度开销。
- **quick_cache**：快，但没有 W-TinyLFU，命中率无法与 Caffeine 媲美。
- **lru / lru-cache**：仅简单 LRU，无并发支持。
- **stretto (Rust)**：Ristretto 的 Rust 移植，维护不活跃，API 不够 Rusty。

**Doppio 的差异化价值**：在保持 Caffeine 级命中率的同时，利用 Rust 的类型系统实现真正的零成本可插拔架构，并通过精细的无锁并发设计超越 moka 的吞吐量。

---

## 2. 现有 Rust 缓存生态分析

### moka (最主要竞品)

```
Architecture:
  Cache<K,V>
    └─ Inner<K,V>
         ├─ DashMap (分片 HashMap，每片一把 RwLock)
         ├─ Policy (W-TinyLFU，由 Segment 模拟)
         ├─ Deques (3条双向链表：window, probation, protected)
         └─ HouseKeeperArc (后台维护任务)
```

**问题**：
- 写操作需要持有 DashMap 分片写锁 + Policy 锁，两次锁获取
- 读操作触发 `on_access` 需要更新 LRU 位置，需要写锁
- `Arc<K>` 和 `Arc<V>` 的克隆开销在高并发下显著

### quick_cache

- 基于 sharded `UnsafeCell<HashMap>`，用 seqlock 保护
- 采用 CLOCK 近似算法，不是 TinyLFU，命中率较低
- 无 TTL 支持（需要用户自行包装）

---

## 3. Caffeine 架构深度解析

### 3.1 整体数据流

```
get(key)
  │
  ├─→ HashMap lookup (sharded, lock-free read path)
  │      │
  │      ├─ HIT: 记录到 ReadBuffer (无锁 ring buffer)
  │      │         │
  │      │         └─ Buffer满/定期 → Maintenance
  │      │                             │
  │      │                             ├─ 处理 ReadBuffer → 更新 FrequencySketch
  │      │                             ├─ 处理 WriteBuffer → 执行 add/remove
  │      │                             └─ 淘汰超容量条目
  │      │
  │      └─ MISS: return None
  │
put(key, value)
  │
  ├─→ HashMap insert
  └─→ WriteBuffer.offer(AddTask) → 可能触发 Maintenance
```

### 3.2 W-TinyLFU 策略层

```
总容量 = 100%
  │
  ├─ Window Cache (1%): 小 LRU，保护新来的热点不被立即淘汰
  │     │
  │     └─ Window 满时，候选者进入 Main Cache 入口
  │
  └─ Main Cache (99%): 由 TinyLFU 守门
        │
        ├─ Protected Segment (80% of Main): 访问过的项晋升至此
        │
        └─ Probation Segment (20% of Main): 初入 Main Cache 的项
              │
              └─ 淘汰候选：选择 Probation 尾部 victim
                    │
                    └─ 与 Window 候选比较频率：
                         freq(candidate) > freq(victim) → 准入，淘汰 victim
                         freq(candidate) ≤ freq(victim) → 拒绝，淘汰 candidate
```

### 3.3 ReadBuffer — 无锁条带化缓冲

```
StripedBuffer<E>
  │
  └─ cells: [BoundedBuffer; NCPU * 4]
        │
        └─ BoundedBuffer: ring array[16], head/tail (AtomicLong)
              │
              ├─ offer(e): CAS tail++, 失败则 return FAILED (有损，可以丢弃)
              └─ drain(consumer): 批量消费所有 pending 项
```

**关键洞察**：读缓冲是 **lossy** 的 — 满时直接丢弃，不阻塞读路径。丢失少量频率更新不影响正确性，只略微影响近似精度。

### 3.4 WriteBuffer — MPSC 无界增长队列

```
MpscGrowableArrayQueue (Agrona)
  ├─ 初始容量: 128
  ├─ 最大容量: 1024 (可配置)
  └─ 操作: offer(task) / drain(consumer)
       │
       └─ Task 类型:
            - AddTask(key, value, weight)
            - UpdateTask(key, old_value, new_value)
            - RemoveTask(key)
            - ClearTask
```

写缓冲是 **lossless** 的 — 背压通过触发同步维护实现，不丢写。

### 3.5 FrequencySketch — 4-bit Count-Min Sketch

```
table: [u64; size]   // size = roundUpToPowerOf2(capacity)
                      // 每个 u64 存储 16 个 4-bit 计数器

对于 key 的 hashCode h:
  seed = [0xabc, 0xdef, 0x123, 0x456]  // 4个独立种子

  对于 i in 0..4:
    spread = h XOR (h >>> 17)  // 混合 hash
    index = (spread * seed[i]) >>> (64 - log2(size))   // 选择哪个 u64
    offset = ((spread >>> (i*4)) & 0xF) * 4  // 选择哪个 4-bit 槽

    // 取最小值作为频率估计
    freq = min(freq, (table[index] >> offset) & 0xF)

    // 增量 (cap at 15)
    if (table[index] >> offset) & 0xF < 15:
      table[index] += 1 << offset

Reset (当 additions >= size * 10):
  对每个 table[i]:
    table[i] = (table[i] >> 1) & 0x7777777777777777L  // 所有计数器右移1位(÷2)
  additions = 0
```

**精妙之处**：
- 4 个 hash → 4 个 4-bit 计数器 → min() 作为估计值（Count-Min 的标准做法）
- 使用 `0x7777...` 掩码右移，避免跨槽进位
- 周期性减半实现"遗忘"——让历史热点的频率自然衰减，适应工作集变化

### 3.6 Doorkeeper — Bloom Filter

```
doorkeeper: BloomFilter<K>
  │
  └─ 功能：对每个新 key，必须先通过 doorkeeper 才能进入频率 sketch

逻辑：
  on_access(key):
    if NOT doorkeeper.contains(key):
      doorkeeper.add(key)
      return  // 不增加 sketch 频率
    // 已见过一次的 key 才进入 sketch
    sketch.increment(key)

  doorkeeper 会在 sketch reset 时同步清空
```

**作用**：防止一次性流量(scan, one-hit-wonder)污染频率估计，过滤 ~50% 的计数需求。

### 3.7 TimerWheel — 分层时间轮

```
TimerWheel:
  buckets[0]: 64 buckets × ~1.07s = ~68.5s
  buckets[1]: 64 buckets × ~1min  = ~64min
  buckets[2]: 64 buckets × ~1hr   = ~64hr
  buckets[3]: 64 buckets × ~1day  = ~64days
  buckets[4]: 4  buckets × ~32days = ~128days

每个 bucket: 双向链表 (由 Node 的 prev/next 指针内联存储)

advance(now):
  计算 delta = now - previousTick
  对每一层: 按 delta 推进，expire 过期的 bucket 中的所有 Node
  将未过期 Node 重新 schedule 到更高层
```

---

## 4. Ristretto 架构深度解析

### 4.1 整体架构

```
Cache<K, V>
  │
  ├─ store: ShardedMap (ristretto 自己的分片 HashMap)
  │     └─ 256 shards, 每 shard 一把 sync.RWMutex
  │
  ├─ policy: defaultPolicy (SampledLFU)
  │     ├─ keyCosts: map[uint64]int64   // key → cost
  │     ├─ lfu: tinyLFU
  │     │    ├─ freq: cmSketch         // Count-Min Sketch
  │     │    └─ door: bloom.Bloom      // Doorkeeper
  │     └─ evict(toFree int64) []victim
  │
  ├─ getBuf: RingStripe×256            // 读请求缓冲
  │     └─ 每个 stripe: ring[64] uint64 (仅存 keyHash)
  │
  ├─ setBuf: chan *Item (大小: 32×bufferItems=32×64=2048)
  │     └─ 写请求通过 channel 异步处理
  │
  └─ processItems goroutine:
        循环读 setBuf:
          ├─ EvictBuf 满了 → batch evict
          └─ 处理 Item: set/del/update cost
```

### 4.2 Ristretto 的关键创新

**1. Cost-based 准入与淘汰**
```
每个 Item 有 Cost (weight)
totalCost 不超过 MaxCost
淘汰时：选 cost 最小但频率也较低的 victim
准入时：候选 cost 必须 ≤ evicted items 的总 cost
```

**2. Key Hash 化存储**
```
存储时 key → hash(key) → uint64
牺牲极小概率的 hash 碰撞，换取统一的 u64 key 处理，避免泛型复杂度
(Doppio 不采用此设计，Rust 泛型更优雅)
```

**3. 异步 Set 路径**
```
set(k, v) → channel.send(item) → return true/false (是否入队成功)
           → processItems goroutine 异步处理
```
注意：Ristretto 的 set 不保证立即可见，这是它 API 的一个权衡。

**4. 指标系统**
```go
type Metrics struct {
    Hits, Misses     uint64
    KeysAdded        uint64
    KeysEvicted      uint64
    CostEvicted      uint64
    SetsDropped      uint64   // set 因 buf 满而丢弃
    SetsRejected     uint64   // 准入策略拒绝
    GetsKept         uint64
    GetsDropped      uint64   // 读 buf 丢弃
}
```

---

## 5. 核心算法：W-TinyLFU

### 5.1 TinyLFU 论文核心

**问题**：LRU 命中率高于 LFU 在某些场景（如扫描），LFU 命中率高于 LRU 在频率偏斜场景。
**解法**：用 LFU 作为**准入过滤器**而非缓存结构本身。

```
准入决策 (AdmissionFilter):
  候选 candidate (被从 window 驱出的 key)
  受害者 victim   (主缓存中优先被淘汰的 key)

  if freq(candidate) > freq(victim):
    admit candidate, evict victim
  else:
    reject candidate (candidate 就消失了)
```

### 5.2 W-TinyLFU 完整流程

```
put(key, value):
1. 插入 Window LRU
2. 如果 Window 满:
   candidate = window.evict_lru()  // Window 中最久未访问
   if main_cache.is_full():
     victim = main.probation.lru()  // Probation 中最久未访问
     if sketch.freq(candidate) > sketch.freq(victim):
       main.admit(candidate)        // candidate 进 Probation
       evict(victim)                // victim 真正被驱逐
     else:
       evict(candidate)             // 直接丢弃 candidate
   else:
     main.admit(candidate)          // 直接进 Probation

get(key):
1. 查找 HashMap
2. HIT:
   sketch.increment(key)            // 更新频率
   if key in probation:
     probation.remove(key)
     protected.add_front(key)       // 晋升到 Protected
     if protected.is_full():
       d = protected.evict_lru()
       probation.add_front(d)       // 降级回 Probation
3. MISS: return None
```

### 5.3 各分区大小的经验值

| 分区 | 占比 | 说明 |
|------|------|------|
| Window | 1% | 保护新进热点，防止新 key 立即被 TinyLFU 拦截 |
| Probation | ~20% of Main (≈19.8%) | 等待"证明自己"的 key |
| Protected | ~80% of Main (≈79.2%) | 被证明频繁访问的 key |

---

## 6. Doppio 整体架构设计

### 6.1 架构图

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Doppio Cache<K, V>                          │
│                                                                     │
│  ┌─────────────┐   ┌──────────────────────────────────────────────┐│
│  │   Public    │   │                   Inner                      ││
│  │   API       │──▶│                                              ││
│  │  Cache<K,V> │   │  ┌─────────────┐   ┌──────────────────────┐ ││
│  │  get/put    │   │  │   Store     │   │      Policy          │ ││
│  │  invalidate │   │  │ (ShardedMap)│   │   (WTinyLFU)         │ ││
│  │  stats      │   │  │             │   │                      │ ││
│  └─────────────┘   │  │ ┌─────────┐ │   │ ┌──────────────────┐ │ ││
│                    │  │ │ Shard 0 │ │   │ │  FrequencySketch │ │ ││
│  ┌─────────────┐   │  │ ├─────────┤ │   │ │  (4-bit CMS)     │ │ ││
│  │   Builder   │   │  │ │ Shard 1 │ │   │ ├──────────────────┤ │ ││
│  │  max_cap    │   │  │ ├─────────┤ │   │ │   Doorkeeper     │ │ ││
│  │  ttl/tti   │   │  │ │  ...    │ │   │ │  (Bloom Filter)  │ │ ││
│  │  weigher   │   │  │ └─────────┘ │   │ ├──────────────────┤ │ ││
│  └─────────────┘   │  └─────────────┘   │ │   Window LRU     │ │ ││
│                    │                    │ ├──────────────────┤ │ ││
│                    │  ┌─────────────┐   │ │ Probation SLRU   │ │ ││
│                    │  │ ReadBuffer  │   │ ├──────────────────┤ │ ││
│                    │  │ (Striped    │──▶│ │ Protected SLRU   │ │ ││
│                    │  │  RingBuf)  │   │ └──────────────────┘ │ ││
│                    │  └─────────────┘   └──────────────────────┘ ││
│                    │                                              ││
│                    │  ┌─────────────┐   ┌──────────────────────┐ ││
│                    │  │ WriteBuffer │   │    Maintenance       │ ││
│                    │  │  (MPSC)     │──▶│    (drain + evict)   │ ││
│                    │  └─────────────┘   └──────────────────────┘ ││
│                    │                                              ││
│                    │  ┌─────────────┐   ┌──────────────────────┐ ││
│                    │  │ TimerWheel  │   │     Metrics          │ ││
│                    │  │ (TTL/TTI)   │   │  (lock-free atomic)  │ ││
│                    │  └─────────────┘   └──────────────────────┘ ││
│                    └──────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────────┘
```

### 6.2 请求生命周期

```
GET 路径 (热路径，最小化延迟):
  get(key)
    ├─ 1. 计算 hash，定位 shard
    ├─ 2. shard.read_lock() → 查找 HashMap
    ├─ 3. HIT:
    │    ├─ 克隆 Arc<V>，释放锁
    │    ├─ read_buffer.offer(key_hash)  [无锁，可丢弃]
    │    └─ 返回 Some(value)
    └─ 4. MISS:
         └─ 返回 None

PUT 路径:
  put(key, value)
    ├─ 1. 计算 hash，定位 shard
    ├─ 2. shard.write_lock() → 插入 HashMap
    ├─ 3. 释放锁
    ├─ 4. write_buffer.push(AddOp { key, weight })
    └─ 5. 若 write_buffer 满 → 触发 schedule_maintenance()

MAINTENANCE (惰性，由 buffer 满或定时触发):
  maintenance()
    ├─ 1. drain(read_buffer) → policy.on_access(keys)  // 更新 sketch
    ├─ 2. drain(write_buffer):
    │    ├─ AddOp  → policy.on_insert(key, weight) → evict(victims)
    │    ├─ RemOp  → policy.on_remove(key)
    │    └─ UpdOp  → policy.on_update(key, old_w, new_w)
    └─ 3. timer_wheel.advance(now)  // 处理过期
```

---

## 7. 核心数据结构

### 7.1 Node — 缓存条目节点

```rust
// 每个缓存条目在 HashMap 中存储为 Arc<Node<K, V>>
// Node 需要同时参与：HashMap查找、LRU链表、TimerWheel
pub struct Node<K, V> {
    // 值 (Arc 包装支持读者不持锁访问)
    pub value: Arc<V>,

    // LRU 双向链表指针 (使用 Option<NonNull<Node<K,V>>>)
    pub prev: Option<NonNull<Node<K, V>>>,
    pub next: Option<NonNull<Node<K, V>>>,

    // 在哪个 deque 中 (window / probation / protected)
    pub queue_type: QueueType,

    // 权重 / Cost
    pub weight: u64,

    // 过期相关 (0 表示不过期)
    pub expire_at: AtomicU64,           // nanos since epoch
    pub last_access: AtomicU64,         // 用于 TTI

    // TimerWheel 链表指针
    pub timer_prev: Option<NonNull<Node<K, V>>>,
    pub timer_next: Option<NonNull<Node<K, V>>>,

    // Key (为淘汰时能找回 HashMap 的 key)
    pub key: K,
}
```

**注意**：Node 包含裸指针，需要 `unsafe`。整个 Node 生命周期由 `Arc` 管理，裸指针只用于内部链表遍历，不跨越 Arc 边界暴露给外部。

### 7.2 ShardedStore — 分片哈希表

```rust
pub struct ShardedStore<K, V> {
    shards: Box<[Shard<K, V>]>,  // 固定数量分片，默认 64 或 128
    shard_mask: usize,           // shards.len() - 1
}

// 每个 Shard 对齐到 CacheLine，避免 false sharing
#[repr(align(64))]
struct Shard<K, V> {
    map: RwLock<HashMap<K, Arc<Node<K, V>>>>,
}

impl<K: Hash + Eq, V> ShardedStore<K, V> {
    fn shard_index(&self, key: &K) -> usize {
        // 使用 key 的 hash 高位选分片
        let h = make_hash(key);
        (h >> (64 - self.shard_mask.count_ones())) as usize & self.shard_mask
    }
}
```

### 7.3 FrequencySketch — 4-bit CMS

```rust
pub struct FrequencySketch {
    table: Vec<u64>,    // 每个 u64 存 16 个 4-bit 计数器
    size: usize,        // table.len()，必须是 2 的幂
    additions: usize,   // 追踪总增量，用于触发 reset
}

impl FrequencySketch {
    // SEEDS: 4 个独立的 hash 种子
    const SEEDS: [u64; 4] = [
        0xABC123DEF456_u64,
        0xFEDCBA987654_u64,
        0x0123456789AB_u64,
        0xFEDCBA012345_u64,
    ];
    // 用于 reset 时保留低位 (每个4-bit右移1位，掩码防止借位)
    const ONE_MASK: u64 = 0x1111_1111_1111_1111_u64;
    const RESET_MASK: u64 = 0x7777_7777_7777_7777_u64; // 右移后保留高3位

    pub fn frequency(&self, h: u64) -> u8 {
        let mut freq = u8::MAX;
        for i in 0..4 {
            let (index, offset) = self.slot(h, i);
            let count = ((self.table[index] >> offset) & 0xF) as u8;
            freq = freq.min(count);
        }
        freq
    }

    pub fn increment(&mut self, h: u64) {
        let mut was_added = false;
        for i in 0..4 {
            let (index, offset) = self.slot(h, i);
            if (self.table[index] >> offset) & 0xF < 15 {
                self.table[index] += 1 << offset;
                was_added = true;
            }
        }
        if was_added {
            self.additions += 1;
            if self.additions >= self.reset_threshold() {
                self.reset();
            }
        }
    }

    fn reset(&mut self) {
        for cell in &mut self.table {
            // 每个 4-bit 计数器右移 1 位 (除以2)，保留低3位
            *cell = (*cell >> 1) & Self::RESET_MASK;
        }
        self.additions /= 2;
    }

    fn slot(&self, h: u64, depth: usize) -> (usize, usize) {
        let spread = h.wrapping_mul(Self::SEEDS[depth]);
        let index = (spread >> 32) as usize & (self.size - 1);
        let offset = (spread as usize & 0xF) * 4; // 0~60, 步长4
        (index, offset)
    }

    fn reset_threshold(&self) -> usize {
        self.size * 10
    }
}
```

### 7.4 Doorkeeper — Bloom Filter

```rust
pub struct Doorkeeper {
    bits: Vec<u64>,
    seed: [u64; 2],   // 两个 hash 函数（双哈希模拟多个）
    m: usize,         // 总 bit 数
    additions: usize, // 跟踪插入数，用于周期清零
}

impl Doorkeeper {
    pub fn contains(&self, h: u64) -> bool {
        let (h1, h2) = self.hashes(h);
        let bits_len = self.m;
        (0..4).all(|i| {
            let bit = (h1.wrapping_add(i.wrapping_mul(h2))) % bits_len as u64;
            self.get_bit(bit as usize)
        })
    }

    pub fn insert(&mut self, h: u64) -> bool {
        let already = self.contains(h);
        if !already {
            let (h1, h2) = self.hashes(h);
            for i in 0..4 {
                let bit = (h1.wrapping_add(i.wrapping_mul(h2))) % self.m as u64;
                self.set_bit(bit as usize);
            }
            self.additions += 1;
        }
        already
    }

    pub fn clear(&mut self) {
        self.bits.fill(0);
        self.additions = 0;
    }
}
```

### 7.5 StripedReadBuffer

```rust
// 每个 CPU 绑定独立 ring buffer，减少竞争
// NCPU 个 buffer，通过 thread_local 或 CPU ID 选取
pub struct StripedReadBuffer {
    buffers: Box<[CachePadded<RingBuffer>]>,
}

#[repr(align(64))]  // CacheLine 对齐
struct CachePadded<T>(T);

struct RingBuffer {
    buf: [AtomicU64; 16],   // 存 key_hash
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl RingBuffer {
    fn offer(&self, hash: u64) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Relaxed);
        if tail - head >= 16 {
            return false;  // buffer 满，丢弃 (lossy is OK)
        }
        self.buf[tail & 15].store(hash, Ordering::Relaxed);
        self.tail.fetch_add(1, Ordering::Release);
        true
    }

    fn drain(&self, consumer: &mut impl FnMut(u64)) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        for i in head..tail {
            consumer(self.buf[i & 15].load(Ordering::Relaxed));
        }
        self.head.store(tail, Ordering::Release);
    }
}
```

### 7.6 WriteMpscBuffer

```rust
pub enum WriteOp<K> {
    Add { key: K, weight: u64 },
    Update { key: K, old_weight: u64, new_weight: u64 },
    Remove { key: K },
}

// 使用 crossbeam-queue 的 ArrayQueue 或 SegQueue
// 或者基于 flume::bounded channel 实现
pub struct WriteBuffer<K> {
    queue: crossbeam_queue::ArrayQueue<WriteOp<K>>,
    drain_needed: AtomicBool,
}
```

### 7.7 TimerWheel — 分层时间轮

```rust
// 6层时间轮，覆盖从秒级到年级的过期时间
const BUCKETS: [usize; 6] = [64, 64, 32, 4, 1, 1];
const SPANS: [u64; 7] = [
    1_073_741_824,          // 2^30 ns ≈ 1.07s (level 0 单桶跨度)
    68_719_476_736,         // 2^36 ns ≈ 68.5s
    4_398_046_511_104,      // 2^42 ns ≈ 72.8min
    281_474_976_710_656,    // 2^48 ns ≈ 77.7hr
    1_125_899_906_842_624,  // 2^50 ns ≈ 12.8days (4-bucket level)
    36_028_797_018_963_968, // 2^55 ns ≈ overflow
    u64::MAX,
];

pub struct TimerWheel<K, V> {
    // 每层的桶数组，每桶是双向链表的头节点
    wheels: [Vec<TimerBucket<K, V>>; 6],
    current_time: u64,  // nanos
}
```

---

## 8. Rust Trait 抽象体系

### 8.1 核心 Trait 设计原则

Doppio 的设计目标是**可插拔但零成本**：

- 通过泛型参数（monomorphization）实现策略可替换
- 不使用 `dyn Trait`（会有虚表开销），而是泛型 + feature flags
- Trait 只定义接口，不持有状态（由实现类型持有状态）

### 8.2 Policy Trait

```rust
/// 缓存淘汰/准入策略的抽象
/// 所有方法由 Maintenance 线程单线程调用，无需内部同步
pub trait Policy<K: Hash + Eq>: Sized {
    /// 记录一次访问（来自 read buffer drain）
    fn on_access(&mut self, key_hash: u64);

    /// 记录一次插入，返回需要淘汰的 key 列表
    fn on_insert(&mut self, key: K, weight: u64) -> SmallVec<[K; 4]>;

    /// 记录一次更新（权重变化）
    fn on_update(&mut self, key: &K, old_weight: u64, new_weight: u64);

    /// 记录一次删除
    fn on_remove(&mut self, key: &K);

    /// 当前策略中的总权重
    fn total_weight(&self) -> u64;

    /// 最大允许权重
    fn max_weight(&self) -> u64;
}

/// W-TinyLFU 策略实现
pub struct WTinyLfuPolicy<K: Hash + Eq> {
    sketch: FrequencySketch,
    doorkeeper: Doorkeeper,
    window: WindowDeque<K>,
    probation: ProbationDeque<K>,
    protected: ProtectedDeque<K>,
    max_weight: u64,
    window_max: u64,    // 1% of max
    protected_max: u64, // 80% of main
    total_weight: u64,
}

/// 简单 LRU 策略（零依赖，适合小缓存或测试）
pub struct LruPolicy<K: Hash + Eq> {
    list: LinkedList<K>,
    max_weight: u64,
    total_weight: u64,
}
```

### 8.3 Store Trait

```rust
/// 键值存储后端抽象
pub trait Store<K, V>: Send + Sync {
    fn get(&self, key: &K) -> Option<Arc<V>>;

    /// 插入，返回旧值（若有）
    fn insert(&self, key: K, value: V, weight: u64) -> Option<Arc<V>>;

    fn remove(&self, key: &K) -> Option<Arc<V>>;

    fn clear(&self);

    fn len(&self) -> usize;

    fn contains(&self, key: &K) -> bool;
}
```

### 8.4 Weigher Trait

```rust
/// 权重计算器 — 用于 cost-aware 淘汰
pub trait Weigher<K, V>: Send + Sync {
    fn weight(&self, key: &K, value: &V) -> u64;
}

/// 单位权重（每个条目 weight = 1，基于 count 的限制）
pub struct UnitWeigher;
impl<K, V> Weigher<K, V> for UnitWeigher {
    fn weight(&self, _: &K, _: &V) -> u64 { 1 }
}

/// 用户自定义权重（通过闭包）
pub struct FnWeigher<F>(F);
impl<K, V, F: Fn(&K, &V) -> u64 + Send + Sync> Weigher<K, V> for FnWeigher<F> {
    fn weight(&self, k: &K, v: &V) -> u64 { (self.0)(k, v) }
}
```

### 8.5 Expiry Trait

```rust
/// 过期策略抽象
pub trait Expiry<K, V>: Send + Sync {
    /// 插入时的 TTL (None = 不过期)
    fn expire_after_write(&self, key: &K, value: &V, now: Instant) -> Option<Duration>;

    /// 访问时重置过期时间（TTI）
    fn expire_after_access(&self, key: &K, value: &V, now: Instant,
                           current_duration: Duration) -> Option<Duration>;

    /// 更新时重置
    fn expire_after_update(&self, key: &K, value: &V, now: Instant,
                           current_duration: Duration) -> Option<Duration>;
}

/// 固定 TTL
pub struct FixedTtl(Duration);

/// 固定 TTI (Time-To-Idle)
pub struct FixedTti(Duration);
```

### 8.6 EvictionListener Trait

```rust
/// 淘汰事件监听器
pub trait EvictionListener<K, V>: Send + Sync {
    fn on_evict(&self, key: &K, value: &V, cause: EvictionCause);
}

pub enum EvictionCause {
    Size,      // 超出容量
    Expired,   // TTL/TTI 到期
    Explicit,  // 用户主动 invalidate
    Replaced,  // 值被更新
}
```

### 8.7 Cache Builder

```rust
pub struct CacheBuilder<K, V> {
    max_capacity: u64,
    initial_capacity: Option<usize>,
    weigher: Option<Box<dyn Weigher<K, V>>>,
    time_to_live: Option<Duration>,
    time_to_idle: Option<Duration>,
    num_shards: Option<usize>,
    // eviction_listener: ...
}

impl<K: Hash + Eq + Clone + Send + Sync + 'static,
     V: Send + Sync + 'static> CacheBuilder<K, V> {

    pub fn new(max_capacity: u64) -> Self { ... }

    pub fn time_to_live(mut self, ttl: Duration) -> Self {
        self.time_to_live = Some(ttl); self
    }

    pub fn time_to_idle(mut self, tti: Duration) -> Self {
        self.time_to_idle = Some(tti); self
    }

    pub fn weigher<W: Weigher<K, V> + 'static>(mut self, w: W) -> Self {
        self.weigher = Some(Box::new(w)); self
    }

    pub fn build(self) -> Cache<K, V> { ... }
}
```

---

## 9. 并发模型与内存模型

### 9.1 三路并发设计

```
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│  Reader Thread  │    │  Writer Thread  │    │ Maintenance     │
│                 │    │                 │    │ (could be       │
│ get(key)        │    │ put(key, val)   │    │  caller thread) │
│  │              │    │  │              │    │                 │
│  ▼              │    │  ▼              │    │ drain read_buf  │
│ shard.read()    │    │ shard.write()   │    │ drain write_buf │
│ [shared lock]   │    │ [exclusive lock │    │ policy update   │
│  │              │    │  per shard]     │    │ evict victims   │
│  ▼              │    │  │              │    │ expire entries  │
│ read_buf.offer()│    │  ▼              │    │                 │
│ [CAS, lossy]    │    │ write_buf.push()│    │ [single thread] │
└─────────────────┘    └─────────────────┘    └─────────────────┘
```

### 9.2 策略选择

**Phase 1-2 (简化)**：
- Maintenance 在调用线程中惰性执行（类似 Caffeine 的 `ScheduledThreadPoolExecutor` 方案简化版）
- 用 `Mutex<PolicyState>` 保护 policy，当 write_buf 或 read_buf 触发时尝试获取锁
- 若获取失败则跳过（另一个线程正在维护）

**Phase 3+ (高性能)**：
- 单独的维护线程（`std::thread::spawn` 或 tokio task）
- 通过 `crossbeam_channel` 或 `flume` 接收维护信号
- Policy 状态由维护线程独占，无需锁

### 9.3 内存 ordering 策略

```rust
// 读 buffer offer: Relaxed store + Release 推进 tail
// 读 buffer drain: Acquire 读 tail + Release 推进 head

// shard 内 HashMap: 用 RwLock<HashMap> (parking_lot)
// 分片数固定(64或128)，避免 false sharing (CacheLine 对齐)

// 统计计数器: fetch_add(1, Relaxed) — 近似值可以接受
```

---

## 10. 过期策略设计

### 10.1 两种过期模式

| 模式 | 触发 | 精度要求 |
|------|------|---------|
| TTL (expire after write) | 写入后固定时间 | 秒级 |
| TTI (expire after access) | 最后访问后固定时间 | 秒级 |
| Variable TTL | 按 Expiry trait 计算 | 用户定义 |

### 10.2 过期检测策略

**惰性检测 (Lazy Expiration)**：
- 每次 `get` 时检查 `expire_at`，若已过期返回 None 并标记删除
- 优点：简单，无额外线程
- 缺点：过期 entry 仍占内存直到被访问或维护清理

**TimerWheel 主动清理**：
- 维护时调用 `timer_wheel.advance(now)`
- 批量处理过期桶，主动逐出过期条目
- 释放内存，防止僵尸条目占位

**Doppio 策略**：两者结合：
1. `get` 时惰性检查，立即返回 None
2. TimerWheel 周期性清理，释放内存

---

## 11. 模块结构

```
doppio/
├── Cargo.toml
├── src/
│   ├── lib.rs                  # 公开 API: Cache, CacheBuilder, Metrics
│   │
│   ├── cache.rs                # Cache<K,V> facade，持有 Arc<Inner<K,V>>
│   ├── builder.rs              # CacheBuilder<K,V>
│   │
│   ├── policy/
│   │   ├── mod.rs              # Policy trait
│   │   ├── tinylfu.rs          # WTinyLfuPolicy: window + main + sketch
│   │   ├── lru.rs              # LruPolicy (简单场景/测试)
│   │   └── sketch/
│   │       ├── mod.rs          # FrequencyEstimator trait
│   │       ├── frequency.rs    # FrequencySketch (4-bit CMS)
│   │       └── doorkeeper.rs   # Doorkeeper (Bloom filter)
│   │
│   ├── store/
│   │   ├── mod.rs              # Store trait
│   │   ├── sharded.rs          # ShardedStore (分片 HashMap)
│   │   └── node.rs             # Node<K,V>: value + LRU ptrs + timer ptrs
│   │
│   ├── buffer/
│   │   ├── mod.rs
│   │   ├── read.rs             # StripedReadBuffer (无锁 ring buffer)
│   │   └── write.rs            # WriteMpscBuffer (MPSC queue)
│   │
│   ├── expiry/
│   │   ├── mod.rs              # Expiry trait
│   │   ├── fixed.rs            # FixedTtl, FixedTti
│   │   └── timer_wheel.rs      # 分层时间轮
│   │
│   ├── metrics/
│   │   ├── mod.rs              # Metrics trait
│   │   └── stats.rs            # StatsCounter (atomic counters)
│   │
│   ├── maintenance.rs          # Maintenance 调度逻辑
│   └── sync/
│       └── mod.rs              # 同步原语封装 (parking_lot RwLock 等)
│
├── benches/
│   ├── throughput.rs           # 吞吐量 benchmark (criterion)
│   └── hit_rate.rs             # 命中率 benchmark (不同 trace 文件)
│
└── tests/
    ├── basic.rs                # 基础功能测试
    ├── concurrent.rs           # 并发正确性测试
    ├── expiry.rs               # TTL/TTI 测试
    └── eviction.rs             # 淘汰策略测试
```

---

## 12. 演进路线图

### Phase 1 — 骨架与基础设施 (Week 1-2)

**目标**：可工作的单线程缓存，确立所有 trait 边界

- [ ] 项目结构初始化（lib, modules）
- [ ] 定义所有核心 Trait（Policy, Store, Weigher, Expiry）
- [ ] 实现 ShardedStore（分片 HashMap + parking_lot RwLock）
- [ ] 实现 Node<K,V>（包含值，暂不含链表指针）
- [ ] 实现 LruPolicy（简单双向链表 + HashMap，单线程）
- [ ] 实现 Cache<K,V> facade（get/put/invalidate）
- [ ] 实现 CacheBuilder
- [ ] 基础单元测试 + 集成测试

**可用 API**:
```rust
let cache = Cache::builder(1000)
    .build::<String, String>();
cache.put("key".to_string(), "value".to_string());
assert_eq!(cache.get("key"), Some(Arc::new("value".to_string())));
```

### Phase 2 — W-TinyLFU 核心 (Week 3-4)

**目标**：实现标志性的高命中率策略

- [ ] FrequencySketch（4-bit CMS，4深度，reset机制）
- [ ] Doorkeeper（Bloom filter，双哈希）
- [ ] Window Deque（固定容量 LRU）
- [ ] Probation/Protected Deque（SLRU 两段）
- [ ] WTinyLfuPolicy（完整准入/淘汰逻辑）
- [ ] StripedReadBuffer（无锁环形缓冲）
- [ ] WriteMpscBuffer（MPSC 队列）
- [ ] Maintenance 逻辑（drain + evict）
- [ ] 命中率测试（对比 LRU baseline）

### Phase 3 — 过期支持 (Week 5-6)

**目标**：生产可用的 TTL/TTI 支持

- [ ] TimerWheel（6层分层时间轮）
- [ ] FixedTtl + FixedTti Expiry 实现
- [ ] Maintenance 集成 timer_wheel.advance()
- [ ] 惰性过期检测（get 时检查）
- [ ] 主动过期清理（maintenance 时）
- [ ] Variable TTL（Expiry trait）
- [ ] TTL/TTI 测试

### Phase 4 — 并发优化与 Async (Week 7-9)

**目标**：高并发性能 + async 支持

- [ ] 独立维护线程（单线程 event loop）
- [ ] Cost-aware eviction（Weigher trait 完整集成）
- [ ] EvictionListener 回调
- [ ] `feature = "async"` → tokio 版本 AsyncCache
- [ ] 压力测试 + 火焰图分析
- [ ] 与 moka 的性能对比 benchmark

### Phase 5 — 打磨与发布 (Week 10-12)

**目标**：crates.io 发布质量

- [ ] 完整文档（rustdoc + examples）
- [ ] Hit rate benchmark（ARC trace、zipf分布、wiki trace）
- [ ] no_std 兼容性探索（mini-doppio）
- [ ] NUMA 感知优化（可选）
- [ ] 发布到 crates.io
- [ ] README + 性能对比图

---

## 13. 第一步：如何开始

### 13.1 立即行动清单

**Step 1**: 将项目转为 library crate

```toml
# Cargo.toml
[package]
name = "doppio"
version = "0.1.0"
edition = "2024"

[dependencies]
parking_lot = "0.12"          # 比 std Mutex/RwLock 快
crossbeam-queue = "0.3"       # MPSC ArrayQueue
ahash = "0.8"                 # 快速非加密 hash
smallvec = "0.11"             # 避免小 Vec 堆分配

[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }
rand = "0.8"

[[bench]]
name = "throughput"
harness = false
```

**Step 2**: 建立 lib.rs 骨架

```rust
// src/lib.rs
pub mod builder;
pub mod cache;
pub mod policy;
pub mod store;
pub mod buffer;
pub mod expiry;
pub mod metrics;

pub use builder::CacheBuilder;
pub use cache::Cache;
pub use metrics::Metrics;
```

**Step 3**: 先写 Trait，再写实现

从最简单的开始：
1. `Store` trait + `ShardedStore` 实现
2. `Policy` trait + `LruPolicy` 实现
3. `Cache<K,V>` 直接串联以上两个
4. 测试通过后再进行 Phase 2

### 13.2 第一个可工作版本的目标代码

```rust
use doppio::CacheBuilder;
use std::sync::Arc;

fn main() {
    let cache = CacheBuilder::new(1_000)
        .build::<String, String>();

    cache.insert("hello".to_string(), "world".to_string());

    let val: Option<Arc<String>> = cache.get("hello");
    println!("{:?}", val); // Some("world")

    println!("stats: {:?}", cache.stats());
}
```

### 13.3 技术选型建议

| 组件 | 选择 | 理由 |
|------|------|------|
| HashMap | `std::collections::HashMap` with ahash | 简单可靠，后续可优化 |
| RwLock | `parking_lot::RwLock` | 无毒化，比 std 快 30%+ |
| MPSC Queue | `crossbeam_queue::ArrayQueue` | 无锁，固定容量，有界 |
| Hash 函数 | `ahash` 或 `foldhash` | 速度最快的非加密 hash |
| AtomicCounter | `std::sync::atomic::AtomicU64` | 原生支持 |
| Linked List | 手写 unsafe doubly-linked list | 内联指针，零额外分配 |

### 13.4 关键设计决策点

在开始之前需要确认的设计决策：

1. **Value 存储方式**：`Arc<V>` vs `V` (clone on read)
   - 推荐：`Arc<V>`，get 操作无需持锁

2. **Key 存储方式**：是否也用 `Arc<K>`
   - 推荐：Phase 1 先 `K: Clone + Hash + Eq`，后续再优化

3. **维护触发方式**：调用线程内联 vs 专用线程
   - 推荐：Phase 1 先做调用线程内联（简单），Phase 4 再引入专用线程

4. **`#[non_exhaustive]` 与向后兼容**：
   - 所有公开枚举加 `#[non_exhaustive]`，预留扩展空间

---

## 附录 A：关键论文引用

- Gil Einziger, Roy Friedman (2015). **TinyLFU: A Highly Efficient Cache Admission Policy**. Euro-Par.
- Gil Einziger, Roy Friedman, Ben Manes (2017). **TinyLFU: A Highly Efficient Cache Admission Policy**. ACM TOS.
- Jaleel et al. (2010). **High Performance Cache Replacement Using Re-Reference Interval Prediction (RRIP)**. ISCA.

## 附录 B：参考实现

- [Caffeine](https://github.com/ben-manes/caffeine) — Java，最高命中率的参考实现
- [Ristretto](https://github.com/dgraph-io/ristretto) — Go，高吞吐率，cost-aware
- [moka](https://github.com/moka-rs/moka) — Rust，功能完整的现有实现
- [quick-cache](https://github.com/arthurprs/quick-cache) — Rust，轻量高性能
