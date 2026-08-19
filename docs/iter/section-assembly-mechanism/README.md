# 段落组装与时间线机制验证

> 状态：首个纵切原型和真实 A/B 已完成；2026-08-19 已接入主程序可选 `composition-ab` 模式

## 要验证的问题

本轮只验证长篇作曲角色拆分的确定性底座：主 Agent 能否声明段落拍数与可定位入口，Worker 能否交付各声部
Alda 片段，再由 Harness 在不使用时间回跳的情况下组装，并以真实 Alda 解析证明每个声部的段界和错位入口。

它自身不验证角色拆分是否减少模型调用，也不验证作品是否好听或任何具体作曲技法是否成立。后续角色 A/B
已经复用了这套底座并形成配对完整音频；音乐质量仍需人工完整试听。

## 已实现的最小契约

源码位于 `alda-agent/src/composition.rs`：

- `CompositionSpec` 固定 tempo、声部顺序和段落顺序；
- `SectionContract` 使用有理数拍值声明段落长度；
- `AnchorContract` 声明任意可定位入口的声部与段内拍位；
- `SectionArtifact` / `PartArtifact` 让 Worker 按段落、声部交付 Alda 事件；
- `FragmentItem::Anchor` 允许 Worker 指出音乐入口，但 Marker 名称和放置语法仍由 Harness 控制；
- `assemble_sections` 按规格顺序生成命名空间化 Alda 变量和每件乐器的连续轨道；
- `verify_timeline` 将声明拍位换算为预期毫秒，与真实 `alda parse` Marker offset 逐一核对。

每个段落必须交付全部声部。静默声部也必须用休止符填满段落，因而段落结束不是“缺少事件时推测出来的”，
而是该声部游标真实走到的位置。

## 双源码策略

组装器从同一份规格和产物生成两份源码：

```text
CompositionSpec + SectionArtifact[]
               │
       ┌───────┴────────┐
       ↓                ↓
probe_source        alda_source
每声部段首/段尾      仅正式 %section_*
及音乐入口探针       Marker
       ↓                ↓
真实 alda parse      最终候选链路
       ↓
声明拍位逐点核对
```

临时探针不能直接保留在最终作品中，因为当前 `form_plan` 要求实际 Marker 集合与 `%section_*` 精确一致。
正式源码和探针源码的事件与调用顺序相同，只相差 Harness 插入的 Marker。

组装后的各乐器轨道只顺序调用片段变量，不生成 `@marker`，从结构上消除 Marker 回跳。Worker 原始 Alda 事件
若包含 `%` 或 `@` 会在组装前被拒绝，不能自行取得全局时间线控制权。

## 真实验证结果

测试使用系统安装的 Alda 2.3.3，而不是模拟解析器。样例包含两个 8 拍段落、长笛与双簧管两个声部：长笛
从第 0 拍进入，双簧管从第 1 拍进入，tempo 为 120 BPM。

运行：

```text
cargo test --manifest-path alda-agent/Cargo.toml composition::tests -- --nocapture
```

结果：

- 5 个机制测试全部通过；
- 延迟入口实际位于 500 ms，与声明第 1 拍一致；
- 两个声部在每个 8 拍段落的起止探针均为零偏差；
- 最终源码只留下 `section_exposition`、`section_development` 两个正式 Marker；
- 将双簧管第一段故意缩短 1 拍后，实际段尾为 3500 ms，预期为 4000 ms，验证失败；
- Worker 片段尝试写入 `@section_development` 时，在组装前失败。

格式和静态质量同时通过：

```text
cargo fmt --manifest-path alda-agent/Cargo.toml -- --check
cargo clippy --manifest-path alda-agent/Cargo.toml --all-targets -- -D warnings
```

## 新发现：游标边界不等于最后发声边界

探针证明两个声部的最终游标都精确结束在 8000 ms，但现有 `ScoreInfo.duration_ms` 为 7900 ms。原因是 Alda
默认 quantization 为 90%，最后一个半音符从 7000 ms 起只发声 900 ms；当前总时长取最后一个可听事件的
结束位置，而非声部游标。

这两个概念都有效，但不能混用：

- 段落拼接和可定位入口应以声部游标探针为准；
- 音频尾部、静音和用户感知时长应以可听事件或最终 WAV 为准。

后续接入时不能再用当前 `duration_ms` 单独证明各声部已经走到声明段界。

## 当前证明边界

已经证明：

- Alda 变量足以承载命名空间化的单声部段落片段；
- Harness 可以把多段、多声部片段确定性展开为合法 Alda；
- 有理数拍位加真实 Marker 探针可以精确验证段界、错位进入和固定音乐入口；
- 不依赖 `@marker` 也能完成跨声部同步；
- 片段拍数不足能够在进入最终候选前被机械拒绝。

尚未证明：

- Agent 能稳定产出符合这些结构的 JSON 规格和片段；
- 两个 Worker 的调用、修错与汇总成本低于当前单 Agent；
- Anchor 对应的实际音乐材料或功能符合 Composer 声明；
- 五分钟作品的听感优于当前基线。

## 后续结果

原型先接入独立 `composition-ab` 实验入口完成验证，随后进入主程序可选模式；工作流固定一个 Composer、
两个段落家族 Worker 和只读 Reviewer，且
没有修改 Project 的单一事实主体。首轮同题真实 A/B 中单 Agent 成功，角色臂的 Composer 在协议恢复后仍只
声明出 37.14 秒时间线，未进入 Worker。详见
[Composer–Worker–Reviewer 真实 A/B](../composer-worker-reviewer-ab/README.md)。这说明精确拍数分配应由
Harness 从音乐比例和小节网格确定性派生，不能继续依赖 Composer 心算。

后续设计复核还确认：变量、序列、反复、parts、voices 和 Marker 已由 Alda 原生提供。该原型证明的是宿主
能够安全连接多个模型交付并核对边界，不应继续扩展成第二套片段语言、时间线运行时或对位分析系统。最终边界
见[原生 Alda 上的最小角色工作流设计](../composer-worker-reviewer-ab/design.md)。
