Git Diff 总结

  16 个文件改动，+392 / -380 行（净增 12 行，主要是重构而非膨胀）

  按功能分类

  1. dispatch 子任务结果文件通信（核心改进）

  multi_model/tool.rs (+104/-2)
  - dispatch 结果写入 .dispatch/{node_id}.md 临时文件，返回给主控的只有摘要 + 路径
  - 每次 dispatch 前清理 .dispatch/ 目录下旧文件
  - 单任务短路也写临时文件，保持一致

  multi_model/executor.rs (+131/-2)
  - 新增 generate_workspace_context() — 生成 .dispatch/workspace_context.md，包含 workspace 目录结构、_personal 目录树、数据文件目录
  - build_messages() 注入 workspace 路径和 context 文件路径到子 agent system prompt
  - 子 agent 进度回调（reasoning 灰色输出到 stderr）

  2. Phase 和 Focus 动态化

  agent_loop.rs (+155/-113)
  - Focus 从 2 个硬编码字符串改为根据运行时状态动态生成（探索进度、dispatch 次数、write 次数）
  - 新增 AgentPhase::Summarize 阶段使用
  - 去掉 write_todos 相关全部代码（system prompt 注入、并发拦截）

  context_state.rs (+1/-1)
  - Focus 截断上限 180 → 500 chars

  loop_helpers.rs (+1/-1)
  - PRODUCTION_TOOLS 加入 dispatch_subtasks，解决多模型模式主控永远停在探索阶段的 bug

  3. exec_main 误杀修复

  shell.rs (+5/-0)
  - 正则检查前先 strip 掉安全的 stderr 重定向（2>/dev/null、2>&1、&>/dev/null），避免 ls -la dir/ 2>/dev/null 被拒

  4. Reasoning 保护

  openai_compat.rs (+146/-...)
  - 新增 MAX_REASONING_CHARS = 32KB 上限，GLM reasoning 死循环时截断而非无限等待
  - 从 reasoning 中抢救被错误放置的 tool_call（rescue_tool_calls_from_reasoning）

  5. write_todos 移除

  builder.rs (-4)：两处注册删除
  interaction.rs (-64)：用户改动删除了 LoopControlTool（独立改动）
  lib.rs (-2)：导出调整

  6. GUIDELINES.md 提示词优化

  GUIDELINES.md (+23/-3)
  - 依赖规则强化："默认串行，除非能证明并行安全"
  - 子任务 prompt 编写规范："输出文件的完整路径"提到第 1 条
  - 去掉硬编码示例路径

  7. auto-chain 兜底

  tool.rs 中还包含：
  - edges.is_empty() && subtasks.len() >= 2 时自动按顺序串联子任务

  效果对比

  ┌───────────────────────┬──────────────────────────┬──────────────────────────┐
  │         指标          │          改动前          │          改动后          │
  ├───────────────────────┼──────────────────────────┼──────────────────────────┤
  │ 典型耗时              │ 22.5 min                 │ 4.8 min                  │
  ├───────────────────────┼──────────────────────────┼──────────────────────────┤
  │ dispatch 次数         │ 6 次                     │ 1 次                     │
  ├───────────────────────┼──────────────────────────┼──────────────────────────┤
  │ 子 agent 文件写错位置 │ 频繁                     │ 解决                     │
  ├───────────────────────┼──────────────────────────┼──────────────────────────┤
  │ dispatch 结果截断     │ 主控不信任结果，反复验证 │ 文件通信，按需 read_file │
  ├───────────────────────┼──────────────────────────┼──────────────────────────┤
  │ 主控探索阶段卡住      │ 永远不转 produce         │ dispatch 后正确切换      │
  ├───────────────────────┼──────────────────────────┼──────────────────────────┤
  │ exec_main 误杀 ls     │ 2>/dev/null 被拒         │ 修复                     │
  ├───────────────────────┼──────────────────────────┼──────────────────────────┤
  │ GLM reasoning 死循环  │ 无限等待                 │ 32KB 截断                │
  └───────────────────────┴──────────────────────────┴──────────────────────────┘
