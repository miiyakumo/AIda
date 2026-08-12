# 《机械硬盘之诗》真实端到端产物

> 生成日期：2026-08-12
>
> 模型：运行时显式配置的 `deepseek-v4-flash`

这是首期单作品真实验收项目的快照，可复制到
`~/.alda-agent/projects/mechanical-drive-poem/` 后通过 REPL 重新加载。

## 产物

- `project.json`：素材、需求、模型会话、检查结果和版本元数据；不含 API key；
- `versions/0001.alda`：真实模型生成的版本 1，9 声部、891 个事件、约 191 秒；
- `versions/0002.alda`：按反馈修改并校准后的版本 2，9 声部、759 个事件、180 秒；
- `current.alda`：当前选中的版本 2；
- `exports/version-0002.alda`：版本 2 的 Alda 导出；
- `exports/version-0002.mid`：版本 2 的 MIDI 导出。

反馈为：“让中段更冰冷、更机械，结尾的加速和明亮感更强”。版本 2 使用模型给出的修改候选，
在内容不变的前提下只按真实 Alda 时长结果缩放 tempo，随后通过 `/reload` 校验并采用。

## 已验证流程

1. 输入素材与约 3 分钟完整纯器乐要求；
2. 模型解读并调用 `submit_alda`；
3. 真实 Alda 诊断与自动修正；
4. 保存版本 1；
5. 执行 `/play` 与 `/stop`；
6. 输入自然语言修改反馈并保存版本 2；
7. 查看历史、恢复版本 1、重新选择版本 2；
8. 导出 Alda 与 MIDI；
9. 退出并重新进入项目，确认当前版本和历史恢复；
10. 使用真实 API key 精确值扫描项目，未发现密钥。

远程控制环境无法向用户传递音频，因此未声称完成人工听感评价；播放命令、完整播放时长窗口和
底层 Alda 播放/停止链路已运行。

## 复验

```bash
mkdir -p ~/.alda-agent/projects/mechanical-drive-poem
cp -a artifacts/mechanical-drive-poem/. ~/.alda-agent/projects/mechanical-drive-poem/
cd alda-agent
cargo run -- repl --name mechanical-drive-poem
```

进入 REPL 后可运行 `/history`、`/restore 1`、`/restore 2`、`/play` 和 `/export`。
