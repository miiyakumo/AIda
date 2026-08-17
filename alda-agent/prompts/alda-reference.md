# Alda Agent 精简参考

权威来源是随项目保存的 Alda 官方文档快照 `vendor/alda-docs/2.4.3/`。当前运行时兼容目标为 Alda 2.3.3；遇到不确定语法时先调用 `lookup_alda_docs`，已有乐谱用 `inspect_score` 实际解析，尚未提交的源码用 `inspect_alda_source` 实际解析，不能靠猜测反复提交。

## 时间轴是按声部独立的

不同声部默认从各自的时间零点开始，同时发生。源文件中先写“引子声部”、后写“主题声部”并不会让主题自动排在引子之后。要延后声部，使用等长休止或标记同步：

```alda
(tempo! 90)
midi-harp: o4 c2 e2 g1 %theme
midi-flute: @theme o5 c4 d e f g1
midi-cello: o3 r1 r1 c1
```

重新打开已有声部会从该声部自己的当前位置继续：

```alda
midi-trumpet: o4 c4 d e f
midi-cello: o3 c1
midi-trumpet: g4 a b > c
```

## 多实例与别名

同种乐器有多个实例时必须分别命名。完整声明只出现一次；后续用别名继续，不能再次声明同一别名：

```alda
midi-violin "violin-1": o4 c4 d e f
midi-violin "violin-2": o4 e4 f g a
violin-1: g4 a b > c
violin-2: b4 > c d e
```

## 可靠复用

变量保存事件序列，适合段落发展和紧凑源码。复杂连音优先在声部中用单个长时值表达；临时写法以 `inspect_alda_source`、已保存乐谱以 `inspect_score` 的真实解析结果为准。

```alda
theme = [c8 d e f g a b > c]
answer = [< g8 a b > c d e f g]
midi-flute: o5 [theme answer]*4
midi-cello: o3 [c2 g2 a2 e2 f2 c2 g1]*2
```

## 版本约束

- Alda 2.3.3 没有 `time-signature` 或 `time-signature!`。
- 全局速度写作 `(tempo! 120)`；声部局部速度写作 `(tempo 120)`。
- 音符、休止、和弦、属性、反复、变量、序列、voices、markers 的详细规则以官方快照对应章节为准。
- 修改现有作品前应调用 `inspect_score` 了解 work/current；构造局部材料时用 `inspect_alda_source(scope=fragment)`，检查大小限制内的完整临时候选时用 `scope=candidate`，后者套用项目约束并留下故障恢复检查点，但不算正式提交，也不要求预检通过后立即提交。正式 `submit_result` 后宿主仍会自动校验。需要确认已有乐谱的真实声音时调用 `render_score`。不能把“语法通过”说成“已经播放”。
