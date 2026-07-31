---
verdict: approved
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
---

# A3 独立设计审查 R2

## 结论

没有具体阻断。A3 §9 已闭合 R1 的三项重大问题，并与原始 MVP/Artifact 设计及当前
单 actor 代码边界相容；B 的磁盘 durability、replay、orphan 清理和正式
Revision/Artifact 引用仍按计划留待后续，本轮不据此否决。

## 阻断项

无。

## 闭合证据

- blob 与 provenance 已拆分：`BlobRecord` 只按 hash 去重；每个不同成功 Turn 创建以
  occurrence ID 唯一标识的不可变 manifest；Project reachability 独立登记。计划同时
  冻结了同 Project 和跨 Project 的相同 bytes 用例，消除了以 hash 查询单一来源的
  歧义。
- HTTP 下载已定义为现有 bounded App Service channel 上的内部
  `ResolveArtifactDownload` 查询，由同一 `AppServiceRunner` 返回不可变快照；
  router 不持有 Store，也不把二进制塞入公开 wire reply。这可由当前
  `AppServiceRunner::state` 单一所有者结构直接扩展。
- fixture 在不可见局部完成 hash/size/MIME 复验；失败发生在任何事实或 Store 修改
  前；成功则在 actor 内无 `await` 的同步 transition 中提交 blob、reachability、
  occurrence、Approval/Turn 事实和稳定 reply。故障注入验收逐项覆盖所有可观察状态，
  足以证明查询只能观察 transition 前或后的完整状态。
- Project reachability 在 ETag 判断前验证；不存在与跨 Project 统一为 `404`，认证
  在查找前完成。ETag 命中也不能跳过 hash/size corruption 复验。
- 所有成功、`304` 和错误响应均冻结为 `private, no-store`，并按 Origin、
  Authorization 和 Project header 设置 `Vary`；`304` 无 body。
- 下载路由只接受严格 hash 段，wire 使用 typed hash；客户端文件名、路径、URL、
  `..` 和 filesystem target 均不进入接口，响应文件名仅由服务端 hash 派生。验收同时
  要求源码检索和负向测试证明该边界。
