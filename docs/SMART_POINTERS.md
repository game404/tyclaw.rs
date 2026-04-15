# Rust 智能指针完全指南

本文档覆盖 Rust 标准库、tokio 异步运行时、以及常用第三方库中的所有智能指针和同步原语，
结合本项目 (tyclaw.rs) 的真实代码示例讲解。

---

## 目录

1. [所有权类](#1-所有权类)
   - [Box\<T\>](#11-boxt) — 堆分配
   - [Arc\<T\>](#12-arct) — 原子引用计数
   - [Rc\<T\>](#13-rct) — 单线程引用计数
   - [Cow\<T\>](#14-cowt) — 写时克隆
   - [Weak\<T\>](#15-weakt) — 弱引用
2. [内部可变性类](#2-内部可变性类)
   - [Cell\<T\>](#21-cellt) — 值语义内部可变
   - [RefCell\<T\>](#22-refcellt) — 运行时借用检查
   - [std::sync::Mutex\<T\>](#23-stdsyncmutext) — 标准互斥锁
   - [std::sync::RwLock\<T\>](#24-stdsyncrwlockt) — 读写锁
3. [一次性初始化类](#3-一次性初始化类)
   - [OnceLock\<T\>](#31-oncelockt) — 线程安全一次初始化
   - [OnceCell\<T\>](#32-oncecellt) — 单线程一次初始化
   - [LazyLock\<T\>](#33-lazylockt) — 惰性初始化
4. [异步 Pin 类](#4-异步-pin-类)
   - [Pin\<Box\<dyn Future\>\>](#41-pinboxdyn-future) — 固定内存位置
5. [第三方库](#5-第三方库)
   - [parking_lot::Mutex\<T\>](#51-parking_lotmutext) — 高性能互斥锁
   - [parking_lot::RwLock\<T\>](#52-parking_lotrwlockt) — 高性能读写锁
   - [tokio::sync::Mutex\<T\>](#53-tokiosyncmutext) — 异步互斥锁
   - [tokio::sync::RwLock\<T\>](#54-tokiosyncrwlockt) — 异步读写锁
   - [tokio::sync::Semaphore](#55-tokiosyncsemaphore) — 异步信号量
   - [ArcSwap\<T\>](#56-arcswapt) — 无锁原子指针交换
6. [选型速查表](#6-选型速查表)
7. [本项目使用情况](#7-本项目使用情况)

---

## 1. 所有权类

### 1.1 Box\<T\>

**用途**：将值分配到堆上，获得固定大小的指针。

**何时使用**：
- 类型大小在编译期未知（`dyn Trait`）
- 需要转移大型数据的所有权而不拷贝
- 递归数据结构（链表、树）

```rust
// 1) 特征对象 —— 最常见用法
// 本项目：crates/tyclaw-tools/src/registry.rs
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,  // 存储不同类型的工具实现
}

// 注册时将具体类型装箱为 trait object
tools.register(Box::new(ReadFileTool::new(None)));
tools.register(Box::new(ExecTool::new(sandbox_config)));
// ReadFileTool 和 ExecTool 是不同类型，Box<dyn Tool> 抹平了差异

// 2) 运行时多态 —— 存储未知具体类型
// 本项目：crates/tyclaw-orchestration/src/orchestrator.rs
pub struct Orchestrator {
    runtime: Box<dyn AgentRuntime>,  // 可以是 AgentLoop 或 MockRuntime
}
```

**注意**：
- `Box<T>` 是零开销抽象（相当于 C 的 `malloc + free`）
- 所有权唯一，不能 Clone（除非 T: Clone）
- 如需多所有者，用 `Arc<T>` 而非 `Box<T>`


### 1.2 Arc\<T\>

**用途**：原子引用计数（Atomic Reference Counting），多线程共享所有权。

**何时使用**：
- 多个线程/task 需要读取同一份数据
- 与 `Mutex`/`RwLock` 配合实现共享可变状态
- `dyn Trait` 需要被多个所有者持有

```rust
// 1) 跨 task 共享不可变数据
// 本项目：crates/tyclaw-orchestration/src/subtasks/executor.rs
let providers: HashMap<String, Arc<dyn LLMProvider>> = HashMap::new();
// 多个子任务共享同一个 LLM Provider，不需要复制

// 2) Arc::clone —— 只增加引用计数，不拷贝数据
// 本项目：crates/tyclaw-orchestration/src/subtasks/mod.rs
providers.insert(model.to_string(), Arc::clone(&default_provider));
// 这里 clone 的开销只是原子计数器 +1（约几纳秒），不是深拷贝

// 3) Arc + Mutex = 多线程共享可变状态
// 本项目：crates/tyclaw-orchestration/src/builder.rs
active_tasks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
// Arc 负责多线程共享，Mutex 负责互斥修改

// 4) blanket impl —— 让 Arc<T> 自动获得 T 的 trait
// 本项目：crates/tyclaw-tool-abi/src/lib.rs
impl<T: ToolDefinitionProvider> ToolDefinitionProvider for Arc<T> {
    fn tool_definitions(&self) -> Vec<Value> {
        (**self).tool_definitions()  // 解引用到内部 T 并调用
    }
}
// 这样 Arc<ToolRegistry> 可以直接当 ToolDefinitionProvider 使用
```

**`Arc::clone` vs `.clone()`**：
```rust
let a: Arc<Vec<u8>> = Arc::new(vec![1, 2, 3]);

let b = Arc::clone(&a);   // 推荐：明确表示只是增加引用计数
let c = a.clone();         // 不推荐：看起来像在拷贝 Vec
// b 和 c 效果完全相同，但 Arc::clone 更清晰
```


### 1.3 Rc\<T\>

**用途**：单线程引用计数（Reference Counting）。

**何时使用**：
- 单线程中多个所有者共享数据
- 不需要跨线程（否则用 `Arc`）

```rust
use std::rc::Rc;

let shared = Rc::new(vec![1, 2, 3]);
let a = Rc::clone(&shared);  // 引用计数 +1
let b = Rc::clone(&shared);  // 引用计数 +1
// shared, a, b 指向同一份 Vec

drop(a);  // 引用计数 -1
drop(b);  // 引用计数 -1
drop(shared);  // 引用计数归零，Vec 被释放
```

**本项目未使用** —— 因为是多线程 async 项目，全部用 `Arc` 替代。
`Rc` 不实现 `Send`，无法跨 `.await` 或 `tokio::spawn`。


### 1.4 Cow\<T\>

**用途**：Clone-on-Write，按需决定是借用还是拥有。

**何时使用**：
- 大多数情况下只需要读取（借用），偶尔需要修改（此时才克隆）
- 函数既接受 `&str` 也接受 `String`

```rust
use std::borrow::Cow;

fn process(input: Cow<str>) {
    // 如果不需要修改，零开销借用
    println!("{}", input);
}

// 传入借用 —— 零拷贝
process(Cow::Borrowed("hello"));

// 传入拥有 —— 直接使用，不额外拷贝
process(Cow::Owned(format!("hello {}", name)));

// 按需克隆的典型场景
fn ensure_uppercase(input: &str) -> Cow<str> {
    if input.chars().all(|c| c.is_uppercase()) {
        Cow::Borrowed(input)   // 已经是大写，直接借用
    } else {
        Cow::Owned(input.to_uppercase())  // 需要修改，才分配新 String
    }
}
```

**本项目未使用** —— 字符串处理场景没有频繁的"可能需要修改"模式。


### 1.5 Weak\<T\>

**用途**：`Arc` 的弱引用版本，不增加强引用计数，不阻止析构。

**何时使用**：
- 打破 `Arc` 循环引用（父子互相引用会导致内存泄漏）
- 缓存场景：持有弱引用，数据还在就用，被释放了也不阻止

```rust
use std::sync::{Arc, Weak};

struct Node {
    parent: Weak<Node>,     // 弱引用，不阻止 parent 被释放
    children: Vec<Arc<Node>>, // 强引用，保证 children 存活
}

let parent = Arc::new(Node { parent: Weak::new(), children: vec![] });
let child = Arc::new(Node {
    parent: Arc::downgrade(&parent),  // Arc → Weak
    children: vec![],
});

// 使用弱引用：可能已经失效
if let Some(p) = child.parent.upgrade() {  // Weak → Option<Arc>
    println!("parent still alive");
} else {
    println!("parent already dropped");
}
```

**本项目未使用** —— 没有循环引用场景，Arc 的生命周期由 Orchestrator 统一管理。

---

## 2. 内部可变性类

### 2.1 Cell\<T\>

**用途**：对 `Copy` 类型提供内部可变性，无运行时开销。

**何时使用**：
- 需要在 `&self` 方法中修改某个 `Copy` 字段（如计数器、标志位）
- 单线程，不需要 `Mutex`

```rust
use std::cell::Cell;

struct Counter {
    count: Cell<u32>,  // 可以在 &self 下修改
}

impl Counter {
    fn increment(&self) {  // 注意：&self 而非 &mut self
        let old = self.count.get();
        self.count.set(old + 1);
    }
}
```

**本项目未使用** —— 多线程环境下 `Cell` 不安全（`!Sync`），用 `AtomicU32` 或 `Mutex` 替代。


### 2.2 RefCell\<T\>

**用途**：运行时借用检查，允许在 `&self` 下获得 `&mut T`。

**何时使用**：
- 单线程中需要内部可变性，且 `T` 不是 `Copy`（不能用 `Cell`）
- 编译期无法证明借用安全，但你确信运行时不会冲突

```rust
use std::cell::RefCell;

let data = RefCell::new(vec![1, 2, 3]);

// 运行时借用检查
let r = data.borrow();       // 获取 &Vec —— 可以同时有多个
let mut w = data.borrow_mut(); // 获取 &mut Vec —— 不能和其他借用共存
// 如果同时存在 borrow 和 borrow_mut，运行时 panic!
```

**本项目未使用** —— 多线程环境下 `RefCell` 不安全（`!Sync`），用 `Mutex` / `RwLock` 替代。


### 2.3 std::sync::Mutex\<T\>

**用途**：标准库互斥锁，保证同一时刻只有一个线程访问数据。

**何时使用**：
- 多线程共享可变状态
- 临界区很短，不跨 `.await`

```rust
// 本项目：crates/tyclaw-provider/src/openai_compat.rs
pub struct OpenAICompatProvider {
    cache_state: std::sync::Mutex<HashMap<String, CacheScopeState>>,
}

// 使用：lock() 返回 MutexGuard，离开作用域自动解锁
fn update_cache(&self, scope: &str, state: CacheScopeState) {
    let mut guard = self.cache_state.lock().unwrap();
    guard.insert(scope.to_string(), state);
    // guard 在这里 drop，自动解锁
}

// 本项目：crates/tyclaw-agent/src/runtime.rs
pub type InjectionQueue = Arc<StdMutex<Vec<HashMap<String, Value>>>>;
// Arc 用于跨 task 共享，Mutex 用于互斥修改 Vec
```

**std::sync::Mutex 的缺点**：
- **Poisoning**：如果持锁线程 panic，锁会被"毒化"，后续 `lock()` 返回 `Err`
- **不能跨 `.await`**：持锁期间 await 会阻塞整个 OS 线程
- **比 parking_lot 慢**：标准库实现偏保守


### 2.4 std::sync::RwLock\<T\>

**用途**：读写锁，允许多个读者或一个写者。

**何时使用**：
- 读远多于写的场景
- 同 Mutex，不能跨 `.await`

```rust
use std::sync::RwLock;

let config = RwLock::new(Config::default());

// 多个线程可以同时读
let r1 = config.read().unwrap();
let r2 = config.read().unwrap();  // OK，共享读锁

// 写时独占
let mut w = config.write().unwrap();  // 等待所有读锁释放
w.timeout = 30;
```

**本项目直接使用较少**，DingTalk gateway 用了 `Arc<RwLock<Vec<Backend>>>` 管理后端列表。

---

## 3. 一次性初始化类

### 3.1 OnceLock\<T\>

**用途**：线程安全的一次性初始化全局变量。Rust 1.70 稳定。

**何时使用**：
- 全局配置、全局单例
- 替代 `lazy_static!`（更现代的方式）

```rust
// 本项目：crates/tyclaw-provider/src/provider.rs
use std::sync::OnceLock;

static LLM_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

/// 启动时调用一次
pub fn init_concurrency(max_concurrent: usize) {
    let _ = LLM_SEMAPHORE.set(Semaphore::new(max_concurrent));
}

/// 运行时获取（保证已初始化）
pub async fn acquire_llm_permit() -> SemaphorePermit<'static> {
    LLM_SEMAPHORE
        .get_or_init(|| Semaphore::new(8))  // 兜底默认值
        .acquire()
        .await
        .unwrap()
}

// 本项目：crates/tyclaw-prompt/src/prompt_store.rs
static STORE: OnceLock<PromptStore> = OnceLock::new();

pub fn init(workspace: &Path) {
    let store = PromptStore::load(workspace);
    STORE.set(store).expect("prompt store already initialized");
}

pub fn get(key: &str) -> String {
    STORE.get().expect("prompt store not initialized").get(key)
}
```


### 3.2 OnceCell\<T\>

**用途**：`OnceLock` 的单线程版本（`!Sync`）。

```rust
use std::cell::OnceCell;

let cell = OnceCell::new();
assert!(cell.get().is_none());

cell.set("hello").unwrap();
assert_eq!(cell.get(), Some(&"hello"));

cell.set("world").unwrap_err();  // 已经初始化，再 set 会失败
```

**本项目未使用** —— 多线程场景统一用 `OnceLock`。


### 3.3 LazyLock\<T\>

**用途**：惰性求值的全局变量。Rust 1.80 稳定。相当于 `OnceLock` + 自动初始化。

```rust
use std::sync::LazyLock;

// 首次访问时自动执行闭包初始化
static REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}").unwrap()
});

fn parse_date(input: &str) -> bool {
    REGEX.is_match(input)  // 第一次调用时编译正则，后续直接复用
}
```

**与 `OnceLock` 的区别**：
- `OnceLock`：需要显式调用 `set()` 或 `get_or_init()`
- `LazyLock`：在声明时给出初始化闭包，首次访问自动执行

---

## 4. 异步 Pin 类

### 4.1 Pin\<Box\<dyn Future\>\>

**用途**：固定值在内存中的位置，防止被 move。async/await 的基础设施。

**为什么需要**：async 块编译后生成自引用结构体（内部指针指向自己的字段）。
如果 move 了这个结构体，内部指针就变成悬垂指针。`Pin` 阻止 move。

```rust
// 本项目：crates/tyclaw-agent/src/runtime.rs
// OnProgress 回调类型定义
pub type OnProgress = Box<
    dyn Fn(&str) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send + Sync,
>;
// 拆解：
// Box<dyn Fn(...)>         —— 装箱的闭包（因为闭包大小不定）
//   → Pin<Box<dyn Future>> —— 返回一个被 Pin 住的 Future（因为 Future 可能自引用）
//       + Send             —— 可以跨线程发送

// 本项目：crates/tyclaw-orchestration/src/subtasks/executor.rs
// 创建 OnProgress 回调
let sub_progress: OnProgress = Box::new(move |msg: &str| {
    let node_id = node_id_for_cb.clone();
    let msg = msg.to_string();
    Box::pin(async move {            // Box::pin = Box::new + Pin::new
        // 这个 async 块会被编译为自引用结构体
        // Box::pin 确保它不会被 move
        eprintln!("[{}] {}", node_id, msg);
    })
});

// 调用
sub_progress("hello").await;  // 返回 Pin<Box<dyn Future>>，可以 await
```

**简化理解**：
```rust
// 如果你需要"返回一个 async 闭包"，目前 Rust 的标准写法就是：
fn make_callback() -> Box<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send> {
    Box::new(|| Box::pin(async { /* ... */ }))
}
// 两层 Box 是必要的：外层装闭包，内层装+pin 住 Future
```

---

## 5. 第三方库

### 5.1 parking_lot::Mutex\<T\>

**crate**: [parking_lot](https://crates.io/crates/parking_lot)

**与 std::sync::Mutex 的区别**：

| 特性 | std::sync::Mutex | parking_lot::Mutex |
|------|------------------|-------------------|
| Poisoning | 有（panic 后锁中毒） | **无**（更实用） |
| API | `lock()` 返回 `Result` | `lock()` 直接返回 Guard |
| 性能 | 一般 | **更快**（自适应自旋） |
| 大小 | 40 bytes (Linux) | **8 bytes** |
| 公平性 | 不保证 | 可选公平模式 |

```rust
// 本项目：crates/tyclaw-orchestration/src/orchestrator.rs
use parking_lot::Mutex;

pub struct Orchestrator {
    // pending_ask_user 不用 Arc 包装 —— Orchestrator 本身已被 Arc 包装
    pending_ask_user: Mutex<HashMap<String, (String, Vec<Message>)>>,
    // active_tasks 需要被 reaper task 独立持有，所以用 Arc
    active_tasks: Arc<Mutex<HashMap<String, ActiveTask>>>,
}

// 使用 —— 比 std 更简洁，不需要 .unwrap()
fn get_task(&self, key: &str) -> Option<ActiveTask> {
    let tasks = self.active_tasks.lock();  // 直接返回 Guard，无 Result
    tasks.get(key).cloned()
    // Guard drop，自动解锁
}

// 本项目：crates/tyclaw-control/src/rate_limiter.rs
pub struct RateLimiter {
    per_user: Mutex<HashMap<String, SlidingWindow>>,
    global: Mutex<SlidingWindow>,
}
```

**选用建议**：同步代码中优先用 `parking_lot::Mutex` 替代 `std::sync::Mutex`。


### 5.2 parking_lot::RwLock\<T\>

**与 std 同理**，更快、无 poisoning、更小。

```rust
// 本项目：crates/dingtalk-gateway/src/downstream.rs
use parking_lot::RwLock;

pub struct DownstreamManager {
    backends: Arc<RwLock<Vec<Backend>>>,  // 后端列表，读多写少
}

// 读（共享锁，多个读者并发）
fn select_backend(&self) -> Option<Backend> {
    let backends = self.backends.read();  // 共享读锁
    backends.first().cloned()
}

// 写（独占锁）
fn add_backend(&self, backend: Backend) {
    let mut backends = self.backends.write();  // 独占写锁
    backends.push(backend);
}
```


### 5.3 tokio::sync::Mutex\<T\>

**用途**：可以跨 `.await` 持锁的异步互斥锁。

**与 parking_lot/std Mutex 的关键区别**：
- `lock().await` 是异步的，不阻塞 OS 线程
- MutexGuard 实现了 `Send`，可以跨 `.await` 持有
- **代价**：比同步 Mutex 慢，每次 lock 都有 async 开销

```rust
// 本项目：crates/tyclaw-channel/src/dingtalk/stream.rs
use tokio::sync::Mutex;

// WebSocket 写端需要跨 await 使用
let write = Arc::new(Mutex::new(ws_write_half));

// 发送消息 —— 持锁期间有 .await
async fn send_message(write: &Arc<Mutex<WsWrite>>, msg: &str) {
    let mut w = write.lock().await;     // 异步等待锁
    w.send(Message::text(msg)).await;   // 持锁期间 await 发送
    // 如果用 std::sync::Mutex，这里会阻塞整个 OS 线程！
}

// 本项目：crates/tyclaw-channel/src/dingtalk/credential.rs
pub struct TokenManager {
    state: Arc<tokio::sync::Mutex<TokenState>>,
}

impl TokenManager {
    pub async fn get_token(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        if state.is_expired() {
            // 持锁期间发 HTTP 请求刷新 token（有 .await）
            state.token = self.refresh_token().await?;
        }
        Ok(state.token.clone())
    }
}
```

**选用规则**：
```
临界区内有 .await？
  ├─ 是 → tokio::sync::Mutex
  └─ 否 → parking_lot::Mutex（更快）
```


### 5.4 tokio::sync::RwLock\<T\>

**用途**：异步读写锁，读锁和写锁都可以跨 `.await`。

```rust
use tokio::sync::RwLock;

let config = Arc::new(RwLock::new(Config::default()));

// 异步读
async fn get_timeout(config: &RwLock<Config>) -> u64 {
    let r = config.read().await;
    r.timeout
}

// 异步写
async fn update_config(config: &RwLock<Config>, new: Config) {
    let mut w = config.write().await;
    *w = new;
}
```

**本项目未直接使用** —— 读写锁场景用 `parking_lot::RwLock`（不跨 await）或 `ArcSwap`（无锁）。


### 5.5 tokio::sync::Semaphore

**用途**：控制并发数量。不是智能指针，但常和 `Arc` 配合使用。

```rust
// 本项目：crates/tyclaw-provider/src/provider.rs
use tokio::sync::Semaphore;

// 全局 LLM 并发限制器
static LLM_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

pub fn init_concurrency(max: usize) {
    let _ = LLM_SEMAPHORE.set(Semaphore::new(max));
}

// 每次 LLM 调用前获取 permit
pub async fn chat(&self, messages: Vec<Message>) -> Response {
    let _permit = LLM_SEMAPHORE
        .get_or_init(|| Semaphore::new(8))
        .acquire().await.unwrap();
    // _permit 存活期间占用一个槽位
    // drop 时自动释放
    self.do_chat(messages).await
}

// 本项目：crates/tyclaw-orchestration/src/subtasks/scheduler.rs
// 子任务并发数限制
let semaphore = Arc::new(Semaphore::new(max_parallel));

for task in ready_tasks {
    let sem = Arc::clone(&semaphore);
    tokio::spawn(async move {
        let _permit = sem.acquire().await.unwrap();
        execute_task(task).await;
        // _permit drop，释放槽位，下一个任务可以开始
    });
}
```


### 5.6 ArcSwap\<T\>

**crate**: [arc-swap](https://crates.io/crates/arc-swap)

**用途**：原子地替换 `Arc<T>`，读端完全无锁。专为"读极多写极少"场景设计。

**原理**：底层是 `AtomicPtr`，读取时直接加载指针（一条原子指令），写入时原子交换指针并正确管理旧 Arc 的引用计数。

```rust
// 本项目：crates/dingtalk-gateway/src/downstream.rs
use arc_swap::ArcSwap;

pub struct DownstreamManager {
    routing: ArcSwap<RoutingConfig>,  // 路由配置，每个请求都读，极少更新
}

impl DownstreamManager {
    pub fn new(config: RoutingConfig) -> Self {
        Self {
            routing: ArcSwap::from_pointee(config),  // 初始化
        }
    }

    // 读 —— 无锁，每个请求调用，极高频
    pub fn get_route(&self, path: &str) -> Route {
        let config = self.routing.load();  // 原子加载，无锁
        config.match_route(path)
        // load() 返回 Guard，类似 Arc，自动管理生命周期
    }

    // 写 —— 原子替换，配置热更新时调用，极低频
    pub fn update_config(&self, new_config: RoutingConfig) {
        self.routing.store(Arc::new(new_config));
        // 旧配置的 Arc 引用计数 -1，正在使用旧配置的读者不受影响
    }
}
```

**与 RwLock 对比**：

| 操作 | RwLock | ArcSwap |
|------|--------|---------|
| 读 | 获取共享锁（原子操作 × 2） | 原子加载（原子操作 × 1） |
| 写 | 获取独占锁（等待所有读者） | 原子交换（不等待读者） |
| 读者阻塞写者 | 是 | **否** |
| 写者阻塞读者 | 是 | **否** |

---

## 6. 选型速查表

```
需要堆分配？
  └─ 单一所有者 → Box<T>
  └─ 多所有者
       ├─ 单线程 → Rc<T>
       └─ 多线程 → Arc<T>

需要内部可变性？
  ├─ 单线程
  │    ├─ T: Copy → Cell<T>
  │    └─ T: !Copy → RefCell<T>
  └─ 多线程
       ├─ 临界区内有 .await？
       │    ├─ 是 → tokio::sync::Mutex<T>
       │    └─ 否 → parking_lot::Mutex<T>
       └─ 读多写少？
            ├─ 读极多写极少 → ArcSwap<T>
            ├─ 读多写少（同步）→ parking_lot::RwLock<T>
            └─ 读多写少（异步）→ tokio::sync::RwLock<T>

需要全局单例？
  ├─ 声明时知道初始化逻辑 → LazyLock<T>
  └─ 运行时才初始化 → OnceLock<T>

需要返回 async 闭包？
  └─ Box<dyn Fn() -> Pin<Box<dyn Future>>>
```

---

## 7. 本项目使用情况

| 智能指针 | 使用量 | 典型场景 |
|---------|--------|---------|
| `Arc<dyn Trait>` | 30+ | LLMProvider / Tool / Sandbox 跨 task 共享 |
| `Arc<parking_lot::Mutex<T>>` | ~8 | Orchestrator 状态、活跃任务、限流器 |
| `Arc<tokio::sync::Mutex<T>>` | ~6 | WebSocket 写端、Token 缓存 |
| `Box<dyn Tool>` | ~24 | 工具注册表存储异构工具实现 |
| `Pin<Box<dyn Future>>` | ~8 | OnProgress 异步回调返回值 |
| `OnceLock<T>` | 3 | 全局 Semaphore、PromptStore |
| `ArcSwap<T>` | 1 | Gateway 路由配置热更新 |
| `std::sync::Mutex<T>` | ~4 | Provider cache、InjectionQueue |
| `Rc / RefCell / Cell / Cow` | 0 | 多线程 async 项目不适用 |
