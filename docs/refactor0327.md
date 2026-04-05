一、自建 Agent Runtime

- 自主 ReAct Loop，直接调 LLM API，Orchestrator 常驻内存，零启动开销
- 动态 token 预算：历史按预算裁剪，工具结果超长截断
- Prompt Caching：静态前缀（identity/bootstrap/memory）注入 cache_control 断点，跨轮HIT大幅降低成本
- 提示词统一管理：config/prompts.yaml

二、多模型编排

- 主控 LLM 通过 dispatch_subtasks 按需拆分子任务，支持依赖关系、并发、超时
- coding → GPT、reasoning → Gemini 等，简单任务可配国产模型，更省钱，配合relay.tuyoo.com一个key支持方便配置
- 主任务与子任务通过 dispatch 目录文件交互（main_llm.md / {node_id}.md），read_file 按需读取
- per-dispatch 隔离目录，并发互不干扰

三、并发架构

- Orchestrator 全 &self 无全局锁，耗时的 agent loop 完全无锁并行
- 共享状态用内部 Mutex 或 append 文件，只锁微秒级读写
- 网络/LLM 调用走 tokio async，文件/exec 用 tokio spawn_blocking 线程池
- 群聊 session_key 含 user_id，同群多人独立 session 并行

四、工具系统

- 文件操作（read/write/edit/list/glob/grep/copy/move/mkdir）+ 路径穿越防护
- exec、ask_user、send_file、timer、web_search/web_fetch
- Skill 由 LLM 判断匹配，后续可加 SkillRouter（小模型本地匹配）

五、会话与记忆

- per-session JSONL 持久化，多轮对话
- 记忆合并器：token 预算控制，自动归档
- 案例库：历史问答检索，注入 system prompt

六、多通道接入及部署

- CLI（rustyline REPL）/ 钉钉（WebSocket Stream）/ 混合模式
- Timer 常驻，task_local 隔离请求上下文
- 可执行文件10m以下单二进制，方便服务器集中部署和个人应用两种方式
