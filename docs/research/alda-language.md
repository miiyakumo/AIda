# Alda 语言完整参考手册

> 基于仓库内 Alda 源码快照 (`ref/alda/`) 的文档、示例乐谱和 parser 测试文件整理。校验基准: Alda 2.4.3, commit `33e17e5674fd98da89462f21b2e0e5f2d9f16944`。
> 本文作为 agent 系统提示词的基础素材 —— 语法示例经过 doc/ 和 examples/ 交叉验证。
> 文件引用格式: `文件:行号` 或 `文件:章节`（行号来自原始文件,行号不适用时用章节名）。

---

## 目录

1. [概述](#概述)
2. [核心语法概念 (从浅到深)](#核心语法概念)
   - [2.1 Score/Part 与乐器声明](#21-scorepart-与乐器声明)
   - [2.2 音符/音名/升降号](#22-音符音名升降号)
   - [2.3 八度控制](#23-八度控制)
   - [2.4 时值与连音线](#24-时值与连音线)
   - [2.5 休止符](#25-休止符)
   - [2.6 和弦](#26-和弦)
   - [2.7 声部 Voice](#27-声部-voice)
   - [2.8 Cram 表达式](#28-cram-表达式)
   - [2.9 Repeat 与交替结尾](#29-repeat-与交替结尾)
   - [2.10 变量](#210-变量)
   - [2.11 Attribute 属性系统](#211-attribute-属性系统)
   - [2.12 Marker](#212-marker)
   - [2.13 Lisp 表达式](#213-lisp-表达式)
   - [2.14 其他特性](#214-其他特性)
3. [Examples 分级](#examples-分级)
   - [3.1 入门级](#31-入门级)
   - [3.2 中级](#32-中级)
   - [3.3 复杂级](#33-复杂级)
4. [常见语法错误形态](#常见语法错误形态)

---

## 概述

Alda 是一种音乐编程语言，使用文本语法描述乐谱，通过 MIDI 合成器播放。核心设计理念：

- 每条指令由**乐器声明 + 音符/事件序列**组成
- 使用括号 `( )` 包裹属性变更（Lisp S-表达式风格）
- 每个乐器声部维护独立状态（八度、时值、音量等），声部间自动同步
- 支持变量、重复、标记等模块化构造

---

## 核心语法概念

### 2.1 Score/Part 与乐器声明

**来源:** `doc/scores-and-parts.md`, `doc/instance-and-group-assignment.md`

#### 基本乐器声明

```alda
# 格式: 乐器名: 音符序列
piano: c d e f g
```

乐器名可以是任何 GM MIDI 乐器名（如 `piano`, `violin`, `midi-acoustic-grand-piano`），或别名规则下的自定义名称。

#### 乐谱组织方式

**方式一：逐乐器写完再换**

```alda
piano: c d e f g a b > c
clarinet: r2 e4 f g2
```

**方式二：交替编写**（各乐器按段落交错）

```alda
piano: c d e
clarinet: r2 e4
piano: f g a b > c
clarinet: f g2
```

Alda 自动跟踪每个乐器的当前偏移量（offset），即使交替编写也不会错位。

#### 乐器组 (Groups)

`/` 分隔让多个乐器同时演奏相同音符：

```alda
trumpet/trombone/tuba: c e g
```

各组独立维护自身属性（音量、速度等），默认不进行同步。使用 rests 或 markers 来同步声部。

#### 别名 (Aliases)

使用双引号为乐器实例创建别名，用于区分同类乐器的多个实例：

```alda
oboe "oboe-1": c d e
oboe "oboe-2": g a b

# 通过 . 操作符访问组内成员
violin/viola/cello "strings": g1~1~1
strings.cello: < c1~1~1         # 仅操作组内的 cello
```

**实例分配规则** (来自 `doc/instance-and-group-assignment.md`):

| 调用形式 | 行为 |
|---------|------|
| `foo:` | 若已有同名未命名实例则引用，否则创建新实例 |
| `foo "bar":` | 创建 `foo` 类型、别名 `"bar"` 的新实例 |
| `foo/bar:` | 组调用；两者均为库存乐器时创建/选取 |
| `foo/bar "baz":` | 创建组并赋予别名 `"baz"`，可通过 `baz.foo` 访问 |

**命名规则:** 至少 2 个字符，前两位必须是字母，之后可包含字母、数字、下划线。

---

### 2.2 音符/音名/升降号

**来源:** `doc/notes.md`

Alda 音符由三个组件构成：**八度 (octave)** + **时值 (duration)** + **音高 (letter pitch)**。三者均可部分省略。

#### 音名

使用字母 `a` 到 `g`（小写，大小写在 Alda 中不敏感但惯例用小写）：

```alda
piano: c d e f g a b
```

#### 升降号 (Accidentals)

| 符号 | 含义 | 示例 |
|------|------|------|
| `+` | 升号 (sharp) | `c+` (C#) |
| `-` | 降号 (flat) | `b-` (Bb) |
| `_` | 还原号 (natural) | `f_` (F 还原) |
| `++` | 重升 (double sharp) | `c++` |
| `--` | 重降 (double flat) | `b--` |

当设置了调号 (key-signature) 后，音符会自动应用调号，无需手动标升降号。还原号 `_` 用于覆盖调号。

**示例：B 大调音阶（对比手动升降号和调号两种写法）**

```alda
# 手动写法
piano: b c+ d+ e f+ g+ a+ b

# 使用调号
piano:
  (key-signature "f+ c+ g+ d+ a+")
  b c d e f g a b
```

#### 音符与时值

时值数字紧跟在音名字母后（见下一节的详细时值规则）：

```alda
c     # 隐式时值（沿用前一个音符的时值或默认值）
c4    # 四分音符
c8    # 八分音符
c2.   # 附点二分音符
```

---

### 2.3 八度控制

**来源:** `doc/notes.md`, `doc/attributes.md`, parser test `octaves_test.go`

Alda 的八度概念来自科学音高记法 (scientific pitch notation)：`o4` 表示从中央 C 开始的第四个八度。

#### 设置八度

```alda
o4 c d e    # 八度 4 (默认值)
o5 c d e    # 八度 5
o3 c d e    # 八度 3
```

#### 八度升降

```alda
> c         # 升一个八度后再发音符
< c         # 降一个八度后再发音符
c >         # 发音符后升八度
c <         # 发音符后降八度
>>>         # 连续上升三个八度
<<<         # 连续下降三个八度
><>         # 上->下->上 ("八度鱼")
```

#### 作为属性

```alda
(octave 5) c d e   # 设置当前八度为 5
```

**默认八度:** `o4`

---

### 2.4 时值与连音线

**来源:** `doc/notes.md`, parser test `duration_test.go`

#### 基本时值

| 数字 | 含义 |
|------|------|
| `1` | 全音符 |
| `2` | 二分音符 |
| `4` | 四分音符 (默认) |
| `8` | 八分音符 |
| `16` | 十六分音符 |
| `32` | 三十二分音符 |

#### 附点音符

```alda
c4.    # 附点四分音符 (四分 + 八分)
c2..   # 双重附点二分音符
```

#### 非标准时值

```alda
c6     # 六分音符 (2 的幂以外的时值)
c0.5   # 小数时值 (二分之一的全音符)
```

#### 毫秒/秒时值

```alda
c450ms    # 450 毫秒
c2s       # 2 秒
c1.5s     # 1.5 秒
```

#### 连音线 (Tie)

用 `~` 连接音符，表示持续发声不中断：

```alda
c1~2      # 全音符接二分音符，持续发音
c1~2~4    # 整数时值连音
c500ms~350ms  # 毫秒时值连音
c5s~4~350ms~0.5  # 多种时值混合
```

#### 连奏线 (Slur)

不带后续时值的 `~` 表示连奏（平滑过渡）：

```alda
c4~ d~ e~ f   # 四个音符连奏
```

#### 跨小节延音

```alda
c4.~4~|~4.~8   # 通过小节线 | 的延音
```

#### 默认时值设置

```alda
(set-duration 4)     # 设置默认时值为四分音符
(set-note-length 8)  # 设置默认时值为八分音符
(set-duration-ms 500) # 设置默认时值为 500 毫秒
```

#### 时值继承

音符不写时值时，沿用前一个音符的时值：

```alda
c8 d e f g f e d    # 所有音符都是八分音符
```

---

### 2.5 休止符

**来源:** `doc/rests.md`

```alda
r      # 隐式时值休止符
r4     # 四分休止符
r1     # 全休止符
r2s    # 2 秒休止符
```

休止符本质上只向前推进偏移量，不产生音符事件。

---

### 2.6 和弦

**来源:** `doc/chords.md`, parser test `chords_test.go`

用 `/` 分隔和弦内各音符：

```alda
c/e/g          # C 大三和弦
c/e/g/>c       # 跨八度 C 大三和弦
c1/e2/g4/r8   # 和弦内不同时值 + 休止符
b>/d/f2.       # 和弦内八度变化 + 附点音符
```

和弦中各音符可以有不同时值。和弦结束后，下一个音符从**最短音符**结束处开始。

---

### 2.7 声部 Voice

**来源:** `doc/voices.md`, parser test `voices_test.go`

Voice 用于将一个乐器细分为多个同时演奏的独立部分（如钢琴左手/右手）：

```alda
piano:
  V1: c8 e g e   # 右手旋律
  V2: c2 e2      # 左手伴奏
  V0: c1          # V0: 结束多声部，回到单声部
```

与和弦的关键区别：Voice 组结束后，offset 取**所有 Voice 中最长的**（而非和弦的最短音符原则）。这保证了 V0 之后的内容在所有 Voice 完成之后再开始。

```alda
# 在序列内使用 Voice
piano:
  [V1: e b d V2: a c f]
```

---

### 2.8 Cram 表达式

**来源:** `doc/cram-expressions.md`, parser test `cram_test.go`

Cram 表达式将一组音符"压缩"到指定时值内，用于 n 连音和复合节奏 (polyrhythm)：

```alda
{c d e f g}2    # 5 个音符压缩到一个二分音符的时值内（五连音）
{c d e}          # 省略时值，使用上一次的音符时值
{c d e}4         # 3 个音符压缩到一个四分音符的时值内（三连音）
```

Cram 内部音符可带有独立时值标注以影响相对时间分配，但整个 cram 的总时长不变：

```alda
{c2 d4 e8}1    # 内部不同时值，总时长 = 1 (全音符)
```

默认 cram 内第一个音符为四分音符。

支持嵌套 cram 表达式：

```alda
{c {c c c} c}       # 外层三连音，中间嵌套三连音
{c {c c c c c c c} c}  # 七连音嵌套
{c32 c1 c32}         # 极端时值变化
```

---

### 2.9 Repeat 与交替结尾

**来源:** `doc/repeats.md`, parser test `repeats_test.go`

#### 基本重复

任何事件都可通过 `*N` 重复 N 次：

```alda
riffA = f8 f g+ a

c8 *4              # 单音符重复 4 次
[ c d e ] *4       # 序列重复 4 次
c8*7                # 紧凑写法
c8 * 7              # 带空格写法
riffA*4             # 变量重复
[ c > ] *5          # 含八度变换的序列
[c*2]*2             # 嵌套重复
r1*3                # 休止符乘法（用于延迟进入）
```

#### 交替结尾 (Alternate Endings / Variations)

使用 `' 范围` 语法标记特定重复次数中才生效的片段：

```alda
vibraphone:
  [ a b8 > d < b g b > c | e4 < a > c < g |
    [g > g8 f e c < a4] '1-2     # 第 1-2 次使用
    [b8 > d g2.]        '3       # 第 3 次使用
  ] *3
```

- `'1-2` — 第 1 到 2 次重复时生效
- `'3` — 仅第 3 次重复时生效
- `'1,3` — 第 1 和第 3 次生效
- `'2-4` — 第 2 到 4 次生效
- 变奏片段可以出现在重复块的任何位置（开头、中间、结尾均可）

---

### 2.10 变量

**来源:** `doc/variables.md`, parser test `variables_test.go`

#### 定义与引用

```alda
# 定义（音符序列）
motif = b-8 a g f e g a4

# 引用
piano: motif

# 带重复引用
clarinet: motif *8
```

#### 多行定义

```alda
riffA = f8 f g+ a
riffB = b-8 a g f
riffC = e8 f g a
riffD = > c8 < b a g

rockinRiff = [
  riffA*4
  riffB*2 riffA*2
  riffC riffB riffD
]
```

#### 变量作为属性别名

```alda
quiet  = (vol 25)
loud   = (vol 50)
louder = (vol 75)
notes  = c d e

piano: quiet notes
```

#### 变量嵌套

```alda
riffA = f8 f g+ a > c c d c <
riffB = b-8 b- > c+ d f f g f <
rockinRiff = [
  riffA*4
  riffB*2 riffA*2
]
```

一个变量定义中可引用之前已定义的变量。

#### 命名规则

至少 2 个字符，前两位必须是字母，之后可包含字母、数字、下划线。

**有效名称示例（来自 parser 测试 `variables_test.go`):** `aa`, `aaa`, `HI`, `celloPart2`, `xy42`, `my20cats`, `apple_cider`, `underscores_are_great`, `GELATO`, `flan`, `pudding123`, `cheesecake`, `custard_`, `satb`

#### 变量定义后跟换行

```alda
bar = r
foo = bar
# 连续两个变量定义，不产生事件
```

---

### 2.11 Attribute 属性系统

**来源:** `doc/attributes.md`, `doc/tempo.md`

属性使用 Lisp S-表达式语法设置。默认只影响当前乐器，加 `!` 后缀变为全局：

```alda
(volume 50)     # 仅影响当前乐器
(tempo! 120)    # 影响所有乐器（全局）
```

#### 完整属性列表

| 属性 | 格式 | 范围/默认值 | 说明 |
|------|------|------------|------|
| `tempo` | `(tempo n)` 或 `(tempo beat n)` | 默认 120 BPM | 速度，单位 BPM |
| `volume` / `vol` | `(volume n)` | 0-100，默认 54 (= mf) | 音符力度 |
| `track-volume` / `track-vol` | `(track-volume n)` | 0-100，默认 78.7 | 轨道音量 |
| `panning` | `(panning n)` | 0-100 (0=左, 50=中, 100=右) | 声像定位 |
| `octave` | `(octave n)` | 初始值 4 | 当前八度 |
| `key-signature` / `key-sig` | `(key-signature "...")` 或 `(key-signature '(...))` | — | 调号 |
| `transposition` / `transpose` | `(transpose n)` | 半音数，正=上移 | 移调 |
| `quantization` / `quant` | `(quant n)` | 0-100，默认 90 | 量化程度/奏法 |
| `duration` | `(set-duration n)`, `(set-note-length n)`, `(set-duration-ms n)` | — | 默认音符时值 |
| `midi-channel` | `(midi-channel n)` | 0-15，9 保留给打击乐 | MIDI 通道 |

#### Tempo 高级用法

```alda
(tempo! 180)              # 180 BPM (♩ = 180)
(tempo! 2 100)            # 𝅗𝅥 = 100 (二分音符 = 100 BPM)
(tempo! "4." 100)         # ♩. = 100 (附点四分音符 = 100 BPM)
(metric-modulation! "4." 2)  # 节拍转换: 从 ♩. = 𝅗𝅥
```

#### Key Signature 的多种写法

```alda
# 字符串形式（空格分隔的升降号列表）
(key-signature "f+ c+ g+")       # A 大调 / F# 小调
(key-signature "")                # C 大调（清除调号）

# S-表达式形式
(key-signature '(c major))       # C 大调
(key-signature '(a minor))       # A 小调
(key-signature '(g minor))       # G 小调
(key-signature '(a flat major))  # Ab 大调
(key-signature '(e (flat) b (flat)))  # 使用还原号

# 调式 (Modes) — 来自 examples/modes.alda
(key-sig '(c ionian))      # C 大调
(key-sig '(d dorian))      # D 多利亚
(key-sig '(e phrygian))    # E 弗里吉亚
(key-sig '(f lydian))      # F 利底亚
(key-sig '(g mixolydian))  # G 混合利底亚
(key-sig '(a aeolian))     # A 伊奥利亚
(key-sig '(b locrian))     # B 洛克利亚
```

#### 力度标记到 MIDI 音量映射

| 标记 | 音量值 | 标记 | 音量值 |
|------|--------|------|--------|
| `(pppppp)` | 1 | `(mf)` | 54 |
| `(ppppp)` | 5 | `(f)` | 71 |
| `(pppp)` | 10 | `(ff)` | 84 |
| `(ppp)` | 21 | `(fff)` | 91 |
| `(pp)` | 37 | `(ffff)` | 95 |
| `(p)` | 48 | `(fffff)` | 98 |
| `(mp)` | 50 | `(ffffff)` | 100 |

#### 全局 vs 局部

```alda
(tempo! 60)     # 全局速度 60

harpsichord: c8 d e f g a b > c       # 速度 = 60

banjo: (tempo 180) c8 d e f g a b > c # 仅 banjo 速度 = 180
```

**来自 `examples/overriding-a-global-attribute.alda`**

#### Transpose

```alda
# 移调常用于移调乐器（如 A 调单簧管、F 调圆号）
clarinet:
  (transpose -3) c d e    # A 调移调 (-3 半音)

french-horn:
  (transpose -7) c d e    # F 调移调 (-7 半音)
```

---

### 2.12 Marker

**来源:** `doc/markers.md`, parser test `markers_test.go`

#### 放置与引用

```alda
piano:
  %chorus           # 放置标记
  @chorus           # 跳转到标记位置（从标记后继续）
```

标记必须在引用之前先放置（否则报错）。

#### 实际用法

标记常用于多个乐器之间的同步和对齐。来自 `examples/rachmaninoff_piano_concerto_2_mvmt_2.alda`:

```alda
piano "solo":
  %opening
  %piano_in
  %flute_in
  %clarinet_in
  %ending
  @opening a+12 > e_ a+

flute:
  @flute_in r2 g+2~2

clarinet:
  @clarinet_in (transpose -3) c_2 d_
```

**命名规则:** 与乐器名规则相同。

---

### 2.13 Lisp 表达式

**来源:** `doc/attributes.md`, parser test `lisp_test.go`

所有属性变更使用 Lisp S-表达式语法：

```alda
(fff)                         # 无值属性（力度标记）
(volume 50)                   # 数值属性
(key-signature "f+ c+ g+")    # 字符串属性
(tempo! 200)                  # 全局属性 (! 后缀)
(key-sig '(a major))          # 引用列表参数
(key-signature '(e (flat) b (flat)))  # 嵌套列表
(quant 90)                    # 量化属性
(panning 50)                  # 声像属性
```

---

### 2.14 其他特性

#### 注释

```alda
# 这是一个注释
piano: c d e    # 行内注释
```

#### 小节线

```alda
piano: c d | e f | g a
```

小节线不影响播放，纯粹用于视觉分隔。可与延音线组合实现跨小节延音。

#### 事件序列 (Sequences)

```alda
[]                         # 空序列
[c d c r]                  # 含音符和休止符
[ c d e f c/e/g ]          # 含和弦
[c d [e f] g]              # 嵌套序列
[V1: e b d V2: a c f]     # 序列内含多声部
```

序列本身不改变乐谱播放，主要用于重复和变量存储。

#### 偏移量 (Offset)

- **绝对偏移：** 从乐谱开始的毫秒数
- **相对偏移：** 从某个标记之后的毫秒数

#### 打击乐 (Percussion)

`midi-percussion` 是特殊的 GM 乐器，其每个音高对应不同的打击乐器。来自 `examples/percussion.alda`:

```alda
midi-percussion:
  o2
  c8   # Bass Drum 1
  c+   # Side Stick
  d    # Acoustic Snare
  d+   # Hand Clap
  e    # Electric Snare
  f    # Low Floor Tom
  f+   # Closed Hi-Hat
  g    # High Floor Tom
  g+   # Pedal Hi-Hat
  o3
  c8   # Hi-Mid Tom
  c+   # Crash Cymbal 1
  d    # High Tom
  d+   # Ride Cymbal 1
  # ... 更多 MIDI GM 打击乐映射
```

#### Phase Shifting (相位偏移)

来自 `examples/phase.alda` — 通过给不同乐器设置略微不同的速度产生相位错位效果：

```alda
violin: (tempo 100)
viola: (tempo 105)
cello: (tempo 110)

violin/viola/cello: [e8 f g]*99
```

#### MIDI 通道管理

Alda 自动分配 MIDI 通道（0-15，通道 9 保留给打击乐），超过 15 个乐器也能自动复用。也可以手动控制：

```alda
piano:
  (midi-channel 2)
  c8 d e f g a b > c

guitar:
  (midi-channel 2)    # 手动共享通道 2
  r1                  # 此时 piano 正在使用通道 2
  o3 c1/e/g/>c
```

---

## Examples 分级

所有示例文件位于 `ref/alda/examples/`，共 29 首。

### 3.1 入门级

适合首次接触 Alda 的学习者，每个示例聚焦 1-3 个核心概念。

| 文件名 | 行数 | 核心特性 | 亮点 |
|--------|------|----------|------|
| `hello_world.alda` | 2 | 乐器声明、音符字母、时值 (`8`, `2.`)、时值继承 | 最小可运行示例 |
| `dynamics.alda` | 31 | `(pppppp)` 到 `(ffffff)` 全范围力度标记、八度升降 | 渐进式力度展示 |
| `key_signature.alda` | 12 | 调号（字符串形式、S-表达式形式）、还原号 `_`、和弦 | 三种调号写法对比 |
| `seconds_and_milliseconds.alda` | 5 | 毫秒时值 (`500ms`)、秒时值 (`1s`, `2s`)、和弦 | 非传统时值单位 |
| `modes.alda` | 23 | 7 种教会调式 (`ionian` 到 `locrian`)、多段调号切换 | 调式教学范例 |
| `panning.alda` | 17 | `panning` (0-100)、`key-signature`、Voice、和弦 | 声场定位渐变 |
| `overriding-a-global-attribute.alda` | 5 | `tempo!` (全局)、`tempo` (局部覆盖) | 全局/局部属性对比 |
| `variables-2.alda` | 11 | 变量作为属性别名 (`(vol 25)`)、变量组合 | 力度变量+音符变量 |
| `dot_accessor.alda` | 3 | 乐器组别名、`.` 操作符 | 访问组内单乐器 |

### 3.2 中级

覆盖多个概念的组合使用，适合有基础语法知识后的综合练习。

| 文件名 | 行数 | 核心特性 | 亮点 |
|--------|------|----------|------|
| `across_the_sea.alda` | 9 | 全局 tempo/quant、多乐器、延音线、附点、八度升降 | 双乐器合奏 |
| `awobmolg.alda` | 49 | Voice (V0-V2)、marker (`%voiceIn` + `@voiceIn`)、和弦、跨小节延音 | 标记+声部同步 |
| `alternate-endings.alda` | 17 | 重复块 `[...]*3`、交替结尾 `'1-2`/`'3`、嵌套重复、别名 | 进阶重复 |
| `nicechord-transposed-variable.alda` | 9 | 变量、`transpose` 移调、变量重复 `*2` | 同 riff 多种移调 |
| `variables.alda` | 18 | 变量嵌套组合、乐器组别名、`.` 引用 | 模块化 riff |
| `track-volume.alda` | 31 | `track-vol` (轨道音量)、`vol` (音符力度)、变量、quant | 音量淡入/淡出 |
| `phase.alda` | 6 | 不同乐器不同 tempo、乐器组、重复 `*99` | 相位偏移效果 |
| `midi-channel-management-2.alda` | 22 | `midi-channel`、和弦、共享通道 | MIDI 通道手动控制 |
| `percussion.alda` | 78 | GM 打击乐映射、多八度、注释标注 | 打击乐完整映射 |
| `multi-poly.alda` | 10 | 多乐器组、cram、`set-duration`、`octave` 属性 | 乐器组+cram |
| `nesting.alda` | 17 | 嵌套重复、嵌套 cram、和弦、Voice、`key-sig` | 极端嵌套深度 |

### 3.3 复杂级

完整的真实乐曲片断或多层次综合示例。

| 文件名 | 行数 | 核心特性 | 亮点 |
|--------|------|----------|------|
| `bach_cello_suite_no_1.alda` | 69 | Voice (V1-V3)、marker、多个 cello 实例、quant、跨小节延音 | 巴赫大提琴组曲 |
| `debussy_quartet.alda` | 54 | 多乐器别名、力度渐变、quant、调号、cram、和弦 | 德彪西弦乐四重奏 |
| `jimenez-divertimento.alda` | 49 | 变量+属性别名、transpose (4 种移调)、跨小节延音、`key-sig` | 萨克斯四重奏 |
| `midi-channel-management.alda` | 116 | 128 GM 乐器、变量嵌套、打击乐 Voice (V1-V4)、休止符乘法 `r1*N` | The Lick 遍历所有乐器 |
| `poly.alda` | 59 | 多乐器复节奏、嵌套 cram (3/5/7 连音嵌套)、多节奏层 | 复节奏实验 |
| `all-instruments.alda` | 141 | 128 GM 乐器、cram、全局 tempo、panning、休止符乘法 `r1*N` | 全部 GM 乐器遍历 |
| `gau.alda` | 22 | 多乐器合奏、多八度跳跃、交互式声部 | 游戏音乐 |
| `rachmaninoff_piano_concerto_2_mvmt_2.alda` | 205 | 多别名乐器组、marker 同步、变量 (力度/模式)、Voice、transpose、quant、cram、调号切换 | 拉赫玛尼诺夫钢协 |
| `nicechord-alda-demo.alda` | 249 | Voice (V0-V8)、marker、全力度范围、多乐器组、cram、quant、调号、和弦、ping-pong 声像 | 全特性综合展示 |

---

## 常见语法错误形态

**重要说明：** Alda 的 parser 测试文件（位于 `ref/alda/client/parser/*_test.go`，共 15 个文件）**全部是纯正向测试**，不存在 `expectError`、`shouldFail` 或任何"预期失败"的测试路径。以下错误形态是基于 parser 实现逻辑和语法规则推断出的常见无效输入。在构建自己的 Alda 解析器时，应主动测试以下场景：

### 音符相关

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 非法音名字母 | `h4`, `x8`, `m2` | 只有 `a`-`g` 是有效的音名字母 |
| 非法时值 | `cz`, `c-4`, `c` (后跟无效字符) | 时值必须是数字或 `.` |
| 未闭合的延音线 | `c2~` (后面无内容) | `~` 后需要一个有效音符或结束标记 |
| 延音线后无音符 | `c4 ~ \n piano:` | 延音线不能跨声部/乐器 |

### 调号与升降号

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 升降号格式错误 | `c+++`, `b---` | 最多两个 `+` 或 `-` |
| 非法升降号字符 | `c*`, `b#` | 不是有效的升降号符号 |
| 调号字符串格式错误 | `(key-signature "f+ c+ g")` (没有 `+`) | 调号字符串中升降号需带 `+`/`-` |
| 调号列表格式错误 | `(key-sig '(c major)` (缺少右括号) | S-表达式必须完整 |
| 非法调式名 | `(key-sig '(c superlocrian))` | 仅支持 7 种教会调式 |

### 八度

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 八度值越界 | `o-1`, `o10` (超出 MIDI 范围) | 有效八度取决于实现 |
| 孤立的八度标记 | `piano: > > >` (无实际音符) | 仅八度变化不产生音符事件 |
| 非法八度格式 | `o`, `oabc`, `o 5` | 必须 `o` 紧接数字 |

### 乐器声明

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 缺少冒号 | `piano c d e` | 乐器声明必须以 `:` 结尾 |
| 空乐器名 | `: c d e` | 乐器名不可省略 |
| 已命名和未命名冲突 | `piano "p1": c \n piano: d` | 可能因实例分配规则抛出歧义错误 |
| 非法组定义 | `piano//violin:` | 连续的 `/` 无效 |
| `.` 操作符引用不存在成员 | `strings.harp:` | 必须是组内已存在的乐器 |

### 和弦

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| `/` 开头 | `/c/e` | 和弦不能以分隔符开始 |
| `/` 结尾 | `c/e/` | 和弦不能以分隔符结束 |
| 连续分隔符 | `c//e` | 无效 |
| 和弦内非法字符 | `c/x/e` | `x` 不是有效音名 |
| 只有休止符的和弦 | `r/r/r` | 语法有效但无实际音符 |

### 声部 (Voice)

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 未关闭的 Voice 组 | 多个 `V1:` 但缺少 `V0:` | 可能导致 offset 计算错误 |
| Voice 编号不一致 | `V1: ... V3:` (跳过 V2) | 语法有效但可能逻辑混乱 |
| Voice 和非 Voice 混用 | `V1: c d e c/e/g` | 语法合法但概念混合 |

### Cram 表达式

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 未闭合的花括号 | `{c d e` | 必须闭合 `}` |
| 空 cram | `{}` | 内容不能为空 |
| 只有花括号 | `{}4` | 内容不能为空 |
| 非法时值 | `{c d e}x` | 时值必须是数字 |
| 嵌套花括号未闭合 | `{c {d e}2` | 括号必须匹配 |

### 重复

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 非正整数重复次数 | `c8*0`, `c8*-1` | `*N` 的 N 应为正整数 |
| 非数字重复次数 | `c8*abc` | `*` 后必须是数字 |
| 交替结尾范围越界 | `[... [c]'5 ...]*3` | `'5` 超出 `*3` 的重复次数 |
| 非法交替结尾格式 | `[c]'abc`, `[c]'1-` | 范围格式必须是 `数字-数字` 或 `数字` |

### 变量

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 变量名太短 | `x = c d` (1 个字符) | 至少 2 个字符 |
| 变量名前两位非字母 | `1stMotif = c d`, `_priv = c d` | 前两位必须是字母 |
| 缺少等号 | `motif c d e` | 定义必须包含 `=` |
| 缺少右侧表达式 | `motif =` | `=` 后必须有内容 |
| 引用未定义的变量 | `piano: undefinedVar` | 解析时可能报错 |
| 变量定义中的非法嵌套 | `motif = [c d [e f]` | 括号必须闭合 |

### 属性 (Lisp 表达式)

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 括号未闭合 | `(volume 50` | 必须有匹配的 `)` |
| 未知属性名 | `(flavor 42)`, `(speed 120)` | 不是有效的 Alda 属性 |
| 非法属性值 | `(tempo "fast")`, `(volume -10)` | 值类型或范围不符 |
| `!` 使用位置错误 | `(tempo 120!)`, `(volume! 50)` | `!` 只能紧跟在属性名后（如 `tempo!`） |
| 全局属性 `!` 不适用于所有属性 | `(panning! 50)` | `!` 语义上对所有属性有效但某些属性全局化无意义 |

### 标记 (Marker)

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 引用未放置的标记 | `piano: @foo` (之前没有 `%foo`) | 必须 `%name` 先于 `@name` |
| 标记名格式错误 | `%`, `%1start` | 命名规则与乐器名相同 |
| 标记重复定义 | `%foo ... %foo` | 标记名在同一位置被覆盖 |

### 结构和综合

| 错误类别 | 无效输入示例 | 说明 |
|----------|-------------|------|
| 空文件 | 无任何内容 | 至少需要一个乐器声明和音符 |
| 只有乐器声明无音符 | `piano:` | 有冒号但无事件 |
| 孤立的小节线 | `piano: |` | `|` 前后无有效事件 |
| 音符在乐器声明外 | `c d e` (没有 `instrument:`) | 所有音符必须在某乐器声部内 |
| 注释后无实际代码 | 整个文件只有 `#` 注释行 | 无实际事件 |
| 跨乐器的连音线 | `piano: c4~ ... violin: d4` | 不支持跨乐器连音 |

---

## 附录：Alda 文档结构图

```
doc/index.md                       # 文档导航首页
├── doc/notes.md                   # 音符: 八度、时值、音高
├── doc/rests.md                   # 休止符
├── doc/chords.md                  # 和弦
├── doc/voices.md                  # 声部 (Voice)
├── doc/scores-and-parts.md        # 乐谱/声部组织
├── doc/attributes.md              # 属性系统 (tempo/vol/panning/...)
├── doc/tempo.md                   # 速度详解
├── doc/markers.md                 # 标记
├── doc/offset.md                  # 偏移量
├── doc/repeats.md                 # 重复与交替结尾
├── doc/variables.md               # 变量系统
├── doc/cram-expressions.md        # Cram 表达式
├── doc/sequences.md               # 事件序列
├── doc/comments.md                # 注释
├── doc/instance-and-group-assignment.md  # 实例与组分配规则
├── doc/list-of-instruments.md     # 可用乐器列表 (GM MIDI)
├── doc/writing-music-programmatically.md # 程序化创作
├── doc/alda-repl.md               # REPL 使用
├── doc/alda-client.md             # CLI 客户端
├── doc/alda-2-migration-guide.md  # v1 → v2 迁移指南
├── doc/editor-plugins.md          # 编辑器插件
├── doc/installing-a-good-soundfont.md   # SoundFont 安装
├── doc/implementing-an-alda-library.md  # 库实现
└── doc/doc_zh_cn/                 # 中文翻译
    ├── index_zh_cn.md
    ├── notes_zh_cn.md
    ├── scores-and-parts_zh_cn.md
    ├── attributes_zh_cn.md
    ├── tempo_zh_cn.md
    ├── rests_zh_cn.md
    ├── chords_zh_cn.md
    ├── voices_zh_cn.md
    ├── cram-expressions_zh_cn.md
    ├── variables_zh_cn.md
    ├── markers_zh_cn.md
    ├── repeats_zh_cn.md
    ├── offset_zh_cn.md
    ├── sequences_zh_cn.md
    ├── instance-and-group-assignment_zh_cn.md
    └── comments_zh_cn.md
```

---

## 附录：Parser 测试文件清单

位于 `ref/alda/client/parser/`，共 15 个。**全为正向测试**，无负面测试。`parseTestCase` 结构体（`test_helper.go:15-21`）包含 `label`、`given`、`expectUpdates`（可选）、`expectAST`（可选）、`scoreApplyOptOut`（可选）5 个字段，不支持"预期失败"。解析出错时测试直接 `t.Errorf` 退出（`test_helper.go:34-37`）。

| 测试文件 | 测试函数 | 覆盖内容 |
|----------|---------|---------|
| `barlines_test.go` | `TestBarlines` | 小节线、跨小节延音 |
| `chords_test.go` | `TestChords` | 和弦（含休止符、八度变化、不同时值） |
| `comments_test.go` | `TestComments` | `#` 注释（行首、行尾、无空格） |
| `cram_test.go` | `TestCram` | Cram 表达式（含/不含时值） |
| `duration_test.go` | `TestDurations` | 整数/小数/ms/s 时值、延音线、连音线 |
| `event_sequences_test.go` | `TestEventSequences` | 序列（空序列、嵌套、和弦、声部） |
| `examples_test.go` | `TestExamples` | 遍历所有 `examples/` 文件仅验证可解析 |
| `lisp_test.go` | `TestLisp` | Lisp 属性表达式（各种参数形式） |
| `markers_test.go` | `TestMarkers` | `%` 放置、`@` 引用 |
| `notes_test.go` | `TestNotes` | 音符字母、升降号、重升重降、休止符 |
| `octaves_test.go` | `TestOctaves` | `>`/`<`/`oN` 八度控制 |
| `parts_test.go` | `TestParts` | 乐器声明（单/多/别名/组） |
| `repeats_test.go` | `TestRepeats` | `*N` 重复、交替结尾 `'N`/`'N-M` |
| `variables_test.go` | `TestVariables` | 变量定义/引用/命名规则 |
| `voices_test.go` | `TestVoices` | Voice V0-V2、小节线分隔、序列内重复 |
