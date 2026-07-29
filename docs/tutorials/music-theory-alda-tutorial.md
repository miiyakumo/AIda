# 程序员学乐理 x Alda 零基础教程

> 目标读者：完全不懂乐理但知道什么是变量/函数/JSON/命令行的程序员。
> 读完本文，你不仅能看懂乐理概念，还能写出能编译（生成 MIDI）的乐谱，并用代码分析它的度量。

---

## 目录

1. [前言：为什么程序员应该用代码写音乐](#1-前言为什么程序员应该用代码写音乐)
2. [音高与 MIDI：数组索引，从 0 开始](#2-音高与-midi数组索引从-0-开始)
3. [音程与协和度：两个数的差，mod 12 就完了](#3-音程与协和度两个数的差mod-12-就完了)
4. [音阶与调式：掩码过滤器，从 12 位里选 7 位](#4-音阶与调式掩码过滤器从-12-位里选-7-位)
5. [和弦：位掩码叠加，同时置多个 bit](#5-和弦位掩码叠加同时置多个-bit)
6. [和弦进行：有限状态机，状态转移图](#6-和弦进行有限状态机状态转移图)
7. [节拍与节奏：时间序列，sample rate = BPM](#7-节拍与节奏时间序列sample-rate--bpm)
8. [曲式结构：控制流，block + loop + goto](#8-曲式结构控制流block--loop--goto)
9. [多声部：goroutine，独立执行 + 自动 join](#9-多声部goroutine独立执行--自动-join)
10. [贯穿案例：一首 8 小节 C 大调钢琴小曲的进化史](#10-贯穿案例一首-8-小节-c-大调钢琴小曲的进化史)
11. [程序员-乐理速查表（可打印 B4）](#11-程序员-乐理速查表可打印-b4)

---

## 1. 前言：为什么程序员应该用代码写音乐

你每天都在做的事：写文本文件，交给编译器，编译器吐出机器码，CPU 执行。

Alda 做的事：写文本文件，交给 parser，parser 吐出 MIDI 指令，合成器执行。

```
你的代码          → 编译器      → 二进制      → CPU 执行
alda 乐谱         → alda parse  → MIDI JSON  → 合成器执行
```

中间那层 JSON -- `alda parse -o data` 的输出 -- 是我们审查"编译结果"的入口。就像你看 `gcc -S` 输出的汇编，我们看 score JSON 里的 `midi-note`、`offset`、`duration` 三个字段来理解"这段音乐到底是什么"。

本文每一节都遵循三步落地法：

1. **乐理是什么** -- 用你已经理解的编程概念做类比
2. **Alda 怎么写** -- 语法 + 可运行示例
3. **score JSON 怎么算** -- 从 `alda parse -o data` 的 NoteEvent 数组写度量函数

---

## 2. 音高与 MIDI：数组索引，从 0 开始

### 2.1 乐理是什么

你有一个长度为 128 的数组 `frequency[]`，索引 0-127。

- 索引 69 = A4 = 440Hz（国际标准，钢琴中央 C 上面那个 A）
- 索引每 +1 = 频率乘以 `2^(1/12)` ≈ 1.05946（一个半音）
- 索引每 +12 = 频率翻倍（一个八度）

把钢琴想象成这个数组的**可视化**：88 个键 = MIDI 21 到 108。左手小指按的是低索引，右手小指按的是高索引。

```python
# 你用索引访问频率
freq = 440 * (2 ** ((midi - 69) / 12))

# 反过来从频率找索引
midi = 69 + 12 * log2(freq / 440)
```

**编程类比**：`midi-note` 就是一个 enum 的 ordinal 值 -- 你不需要关心它叫什么名字，重要的是**两个值的差**（见下一节）。

### 2.2 Alda 怎么写

```alda
# 音名 = 字母 a-g, 八度 = o0 到 o10
o4 c     # MIDI 60 = 中央 C
o4 c+    # MIDI 61 = C# (升半音)
o4 d-    # MIDI 61 = Db (降半音, 和 C# 是同一个键!)
o5 c     # MIDI 72 = 高八度 C (60 + 12)
< c      # 从当前八度降一个八度再发音
> c      # 从当前八度升一个八度再发音
```

Alda 内部计算 MIDI 编号的公式（来自 `client/model/pitch.go`）：

```
midi = (octave + 1) * 12 + letterInterval
letterInterval: C=0, D=2, E=4, F=5, G=7, A=9, B=11
```

然后加上 `+`(sharp) = +1, `-`(flat) = -1。所以你写 `o4 c+`，Alda 算出来：(4+1)*12 + 0 + 1 = 61。

### 2.3 从 score JSON 怎么算

执行 `alda parse -o data`，得到一个 events 数组：

```json
{
  "events": [
    {
      "midi-note": 60,
      "offset": 0.0,
      "duration": 500.0,
      "volume": 0.54,
      "part": "part001"
    }
  ]
}
```

`midi-note` 直接就是算好的，你不需要重复实现 Alda 的公式。我们关心的是**对 events 数组做聚合计算**：

```python
def midi_to_note_name(midi: int) -> str:
    """60 → 'C4', 61 → 'C#4' ... 像把枚举值还原成变量名"""
    names = ["C","C#","D","D#","E","F","F#","G","G#","A","A#","B"]
    return f"{names[midi % 12]}{midi // 12 - 1}"

def pitch_range(events: list[dict]) -> dict:
    """音域跨度: 太小=平淡, 太大=可能超越乐器能力"""
    midis = [e["midi-note"] for e in events]
    return {"min": min(midis), "max": max(midis),
            "span": max(midis) - min(midis)}
```

### 动手练习 1

给你一段 Alda：

```alda
piano: o4 c d e f g a b > c
```

预测 `alda parse -o data` 输出的 events 数组中，第 1 个和第 8 个 `midi-note` 各是多少？第 1 个和第 8 个的差是多少？

<details>
<summary>答案</summary>

- 第 1 个 (C4): **60**
- 第 8 个 (C5): **72** (60 + 12, 因为是 > c，即八度 5 的 C)
- 差值: **12** (= 一个八度)

```text
// -- 完整 events (省略 volume/part 等字段) --
// [0]  offset=0,    midi-note=60, duration=500
// [1]  offset=500,  midi-note=62, duration=500
// [2]  offset=1000, midi-note=64, duration=500
// [3]  offset=1500, midi-note=65, duration=500
// [4]  offset=2000, midi-note=67, duration=500
// [5]  offset=2500, midi-note=69, duration=500
// [6]  offset=3000, midi-note=71, duration=500
// [7]  offset=3500, midi-note=72, duration=500
```
</details>

---

## 3. 音程与协和度：两个数的差，mod 12 就完了

### 3.1 乐理是什么

**音程** = 两个音在频率数组中的索引差（绝对值）。

但音乐里我们关心的是"**mod 12** 后的差"——因为差 12（八度）听起来是"同一种颜色，只是亮一点/暗一点"。

```python
# C4(60) 到 E4(64): 差 4 个半音 = 大三度, 协和
# C4(60) 到 G4(67): 差 7 个半音 = 纯五度, 非常协和
# C4(60) 到 C#4(61): 差 1 个半音 = 小二度, 不协和(Jaws 主题曲那种紧张感)

semitones = abs(midi_a - midi_b) % 12
```

**编程类比**：这就像两个指针做差，然后 `& 0b1111`（mod 16）只看低位 -- 八度是高位，音程是低位。你不在乎指向的是 heap 还是 stack，你只在乎两个指针的距离。

协和度分三档（mod 12 差 → 听感）：

| mod 12 差 | 音程名 | 协和度 | 听感类比 |
|-----------|--------|--------|---------|
| 0 | 同度/八度 | 完全协和 | 同一个变量,不同的 scope |
| 7 | 纯五度 | 完全协和 | 父子关系,稳定 |
| 5 | 纯四度 | 完全协和 | 纯五度的镜面 |
| 4 | 大三度 | 不完全协和 | 明亮,开心 |
| 3 | 小三度 | 不完全协和 | 柔和,忧伤 |
| 1,2,6,10,11 | 二度/七度/三全音 | 不协和 | 冲突,需要"解决" |

### 3.2 Alda 怎么写

```alda
# 水平方向: 旋律中相邻音符的音程
c4 d4 e4 f4    # C→D = 2半音, D→E = 2, E→F = 1

# 垂直方向: 和弦中同时发音的音符间的音程
c4/e4/g4       # 和弦斜线: C-E = 4半音(大三度), C-G = 7半音(纯五度)
```

### 3.3 从 score JSON 怎么算

```python
CONSONANCE_MAP = {
    0: "完全协和", 7: "完全协和", 5: "完全协和",
    4: "不完全协和", 3: "不完全协和", 8: "不完全协和", 9: "不完全协和",
    1: "不协和", 2: "不协和", 6: "不协和", 10: "不协和", 11: "不协和",
}

def classify_interval(midi_a: int, midi_b: int) -> str:
    return CONSONANCE_MAP[abs(midi_a - midi_b) % 12]

# 对同时发音的音符组(offset 差 < 5ms) 做两两协和统计
def consonance_ratio(events: list[dict], tol_ms: float = 5.0) -> dict:
    # 按 offset 分组 → 每组内两两比较 → 统计三档比例
    groups = group_by_offset(events, tol_ms)
    counts = {"完全协和": 0, "不完全协和": 0, "不协和": 0}
    for group in groups:
        midis = [e["midi-note"] for e in group]
        for i in range(len(midis)):
            for j in range(i+1, len(midis)):
                counts[classify_interval(midis[i], midis[j])] += 1
    total = sum(counts.values())
    return {k: v/total for k, v in counts.items()} if total else {}
```

### 动手练习 2

以下 Alda 代码：

```alda
piano: c/e/g
```

`alda parse -o data` 输出 3 个 NoteEvent，midi-note 分别为 60, 64, 67，offset 均为 0.0。

请问这三对音程 (60-64, 64-67, 60-67) 各自的 mod 12 差和协和度分别是什么？

<details>
<summary>答案</summary>

| 音对 | mod 12 差 | 音程名 | 协和度 |
|------|----------|--------|--------|
| C4(60) - E4(64) | 4 | 大三度 | 不完全协和 |
| E4(64) - G4(67) | 3 | 小三度 | 不完全协和 |
| C4(60) - G4(67) | 7 | 纯五度 | 完全协和 |

大三度+小三度叠成纯五度 (4+3=7)，这就是大三和弦的构造原理。
</details>

---

## 4. 音阶与调式：掩码过滤器，从 12 位里选 7 位

### 4.1 乐理是什么

12 个半音构成一个八度的全部"原材料"。**音阶**是从中选出 7 个作为"合法音"。

**编程类比**：音阶就是一个 12 位的 bitmask -- 置 1 的位表示"可用"，0 表示"偶尔用（离调音）"。

```python
# C 大调音阶: C D E F G A B
# 位 置:   0 1 2 3 4 5 6 7 8 9 10 11
# 音名:    C C# D D# E F F# G G# A A# B
SCALE_MAJOR = 0b101011010101  # 位 0,2,4,5,7,9,11 置 1

# 判断一个 midi_note 是否"在调内":
def in_scale(midi: int, tonic_midi: int, mask: int) -> bool:
    return (mask >> ((midi - tonic_midi) % 12)) & 1 == 1
```

所以**调性(key)** = 主音(tonic) + 音阶掩码(mode mask)。C 大调 = tonic=60, mask=MAJOR。G 大调 = tonic=67, mask=MAJOR。

**调号(key signature)** = 声明哪些位被永久翻转为升/降。这本质上是预处理器的 `#define` -- 你写 `f`，预处理器自动替换成 `f+`（如果调号里有 `f+`）。

七种调式就是七种不同的 mask（半音偏移模式）：

| 调式 | 置 1 的位 | 听感 |
|------|----------|------|
| Major (Ionian) | 0,2,4,5,7,9,11 | 明亮, 大调 |
| Minor (Aeolian) | 0,2,3,5,7,8,10 | 忧伤, 小调 |
| Dorian | 0,2,3,5,7,9,10 | 爵士小调 |
| Phrygian | 0,1,3,5,7,8,10 | 西班牙风 |
| Lydian | 0,2,4,6,7,9,11 | 梦幻 |
| Mixolydian | 0,2,4,5,7,9,10 | 布鲁斯/摇滚 |
| Locrian | 0,1,3,5,6,8,10 | 黑暗, 不稳定 |

### 4.2 Alda 怎么写

```alda
# 方式 1: 字符串 -- 像 #define F_SHARP, C_SHARP, ...
(key-signature "f+ c+ g+ d+")      # E 大调 / C# 小调

# 方式 2: 结构体 -- 像 Mode(tonic=C, mode=major)
(key-sig '(c major))               # C 大调, 无升降号
(key-sig '(a minor))               # A 小调 (= C 大调的关系小调)
(key-sig '(d dorian))              # D 多利亚
(key-sig '(g mixolydian))          # G 混合利底亚
```

设置调号后，不带升降号的音符自动应用调号规则。`_` (下划线) 是还原号，覆盖调号。

```alda
(key-sig "f+ c+ g+")
c               # 自动变成 C# (因为调号里有 c+)
c_              # C 还原，覆盖调号
```

### 4.3 从 score JSON 怎么算

```python
SCALE_MASKS = {
    "major":      {0,2,4,5,7,9,11},
    "minor":      {0,2,3,5,7,8,10},
    "dorian":     {0,2,3,5,7,9,10},
    "phrygian":   {0,1,3,5,7,8,10},
    "lydian":     {0,2,4,6,7,9,11},
    "mixolydian": {0,2,4,5,7,9,10},
    "locrian":    {0,1,3,5,6,8,10},
}

def tonality_score(events: list[dict], tonic_midi: int, mode: str) -> float:
    """调内音比例: 1.0 代表所有音符都在调内。类似代码风格检查中 '符合命名规范的比例'"""
    valid = SCALE_MASKS[mode]
    in_count = sum(1 for e in events
                   if (e["midi-note"] - tonic_midi) % 12 in valid)
    return in_count / len(events) if events else 1.0

def pitch_entropy(events: list[dict]) -> float:
    """音高多样性 (Shannon entropy), 归一化到 0~1。太高=杂乱, 太低=单调"""
    from math import log2
    from collections import Counter
    counter = Counter(e["midi-note"] % 12 for e in events)
    total = len(events)
    entropy = sum(-c/total * log2(c/total) for c in counter.values())
    return entropy / log2(12)  # 最大熵 = log2(12) ≈ 3.585
```

### 动手练习 3

下面这段 Alda 的效果是什么？预测 `alda parse -o data` 输出的 events 中 `midi-note` 值。

```alda
piano:
  (key-sig '(c major))
  o4 c d e f g a b > c
```

<details>
<summary>答案</summary>

C 大调没有升降号，所以和直接写音名完全一样。events 的 midi-note = 60, 62, 64, 65, 67, 69, 71, 72。

但如果改成：

```alda
piano:
  (key-sig "f+ c+ g+")   # A 大调
  o4 c d e f g a b > c
```

则 parser 会自动将 `f`→F#, `c`→C#, `g`→G#。events 的 midi-note = 61, 62, 64, 66, 68, 69, 71, 73。这就是 A 大调音阶。
</details>

---

## 5. 和弦：位掩码叠加，同时置多个 bit

### 5.1 乐理是什么

**和弦** = 三个或更多音符**同时**发音。在 MIDI 里就是多个 NoteEvent 共享同一个 `offset`。

三和弦 = 根音(root) + 三音(third) + 五音(fifth)。从根音往上数半音：

```python
# 就像三个 bit 在 offset 上的组合
TRIADS = {
    "大三和弦 (major)":  [0, 4, 7],   # 明亮, 开心
    "小三和弦 (minor)":  [0, 3, 7],   # 柔和, 忧伤
    "减三和弦 (dim)":    [0, 3, 6],   # 紧张, 收缩
    "增三和弦 (aug)":    [0, 4, 8],   # 漂浮, 梦幻
}
```

**编程类比**：和弦就像对一个位图同时置多个 bit。大三和弦 C-E-G：在 12 位环上把 0, 4, 7 三个位置 1。换根音就是 `rol` (循环左移) 整个掩码。

```python
def make_chord(root: int, offsets: list[int]) -> list[int]:
    """给定根音和偏移模式, 返回所有音的 MIDI 编号"""
    return [root + o for o in offsets]

# C 大三 = [60, 64, 67]
# G 大三 = [67, 71, 74]  (同样的偏移模式, 根音变了)
```

七和弦 = 三和弦 + 第七音。这就变成 4 个 bit：

```python
SEVENTH_CHORDS = {
    "大七 (maj7)": [0, 4, 7, 11],    # 温柔, jazz
    "属七 (dom7)": [0, 4, 7, 10],    # 有"推力", blues
    "小七 (m7)":   [0, 3, 7, 10],    # 忧郁
    "半减七 (m7b5)": [0, 3, 6, 10], # 爵士 tension
    "减七 (dim7)": [0, 3, 6, 9],     # 惊悚片配乐
}
```

### 5.2 Alda 怎么写

```alda
# 和弦: 用 / 分隔同时发音的音符
c/e/g          # C 大三和弦 (C E G)
c-/e-/g-       # C 小三和弦 (C Eb G)
c/e/g/b        # C 大七和弦 (Cmaj7)
c/e/g/b-       # C 属七和弦 (C7)

# 跨八度和弦: 可以在内部用八度控制
c/g/>c/e       # C大三和弦, 跨两个八度, 更"开放"的排列

# 和弦声部结束后, 从最短音符的结束处继续
c1/e2/g4       # c 为一拍, e 为二拍, g 为四分音符 → 下一音符从 g 结束处开始
```

### 5.3 从 score JSON 怎么算

```python
from collections import defaultdict

CHORD_PATTERNS = {
    ("major", 3): (0, 4, 7),
    ("minor", 3): (0, 3, 7),
    ("dim", 3):   (0, 3, 6),
    ("aug", 3):   (0, 4, 8),
    ("maj7", 4):  (0, 4, 7, 11),
    ("dom7", 4):  (0, 4, 7, 10),
    ("m7", 4):    (0, 3, 7, 10),
    ("m7b5", 4):  (0, 3, 6, 10),
    ("dim7", 4):  (0, 3, 6, 9),
}

def group_by_offset(events: list[dict], tol_ms: float = 5.0) -> list[list[dict]]:
    """把 offset 相近 (同时发音) 的 note 分组 -- 就像 GROUP BY offset"""
    groups = []
    current = []
    cur_offset = None
    for e in sorted(events, key=lambda x: x["offset"]):
        if cur_offset is None or abs(e["offset"] - cur_offset) < tol_ms:
            current.append(e)
            cur_offset = cur_offset or e["offset"]
        else:
            if len(current) >= 2:
                groups.append(current)
            current = [e]
            cur_offset = e["offset"]
    if len(current) >= 2:
        groups.append(current)
    return groups

def identify_chord(notes: list[dict]) -> list[tuple[str, int]]:
    """匹配和弦类型 -- 像正则匹配: 把 midi 集合转成偏移模式, 查表"""
    midi_set = sorted(set(n["midi-note"] for n in notes))
    results = []
    for root in midi_set:
        deltas = tuple(sorted((n - root) % 12 for n in midi_set))
        for (name, size), pattern in CHORD_PATTERNS.items():
            pattern_deltas = tuple(sorted(p % 12 for p in pattern))
            if size <= len(midi_set) and all(p in deltas for p in pattern_deltas):
                results.append((name, root))
    return results
```

### 动手练习 4

以下 score JSON 片段（三个 NoteEvent，offset 均相同），请识别它是什么和弦：

```json
[
  {"midi-note": 62, "offset": 1000.0, "duration": 500.0},
  {"midi-note": 65, "offset": 1000.0, "duration": 500.0},
  {"midi-note": 69, "offset": 1000.0, "duration": 500.0}
]
```

<details>
<summary>答案</summary>

- 根音候选: D (62)
- 偏移: 62-62=0, 65-62=3, 69-62=7
- 模式 [0,3,7] = **小三和弦 (minor)**
- 所以这是 **D 小三和弦 (Dm)** = D-F-A

如果你从 F(65) 作为根音: 65-65=0, 69-65=4, (62+12)-65=9 → [0,4,9] 不在表中。从 A(69) 作为根音: 同理也不匹配。所以只有 Dm 一种解释。
</details>

---

## 6. 和弦进行：有限状态机，状态转移图

### 6.1 乐理是什么

如果和弦是单个 bitmask，那**和弦进行**就是 bitmask 序列 -- 一帧一帧的状态转移。

**编程类比**：和弦进行就是一个**有限状态机 (FSM)**，状态 = 当前和弦，转移 = 和弦变换。你的耳朵就是状态机模拟器 -- 它听到 V 和弦时，期望下一个是 I（"解决"）。

以 C 大调为例，tonic=60，音阶七个音级上构建的三和弦：

```python
# degrees:    1    2    3    4    5    6    7
# 罗马数字:   I    ii   iii  IV   V    vi   vii°
# root 偏移:  0    2    4    5    7    9    11
# 和弦类型:  Maj  min  min  Maj  Maj  min  dim
```

三种最常见的**状态转移路径**：

| 进行 | root 序列 (相对 tonic) | 听感 | 用途 |
|------|------------------------|------|------|
| I-IV-V-I | 0, 5, 7, 0 | 最经典的"回家" | 儿歌、民谣、摇滚 |
| ii-V-I | 2, 7, 0 | 爵士核心 | 几乎所有爵士标准曲 |
| I-V-vi-IV | 0, 7, 9, 5 | 流行金曲公式 | Let It Be, 无数热单 |

和弦功能（把状态分组）：

- **Tonic (主功能)**: I, vi, iii --"在家", 稳定
- **Subdominant (下属功能)**: IV, ii --"出门了但不太远"
- **Dominant (属功能)**: V, vii° --"在门口掏钥匙, 急迫想进门"

经典路径: **Tonic → Subdominant → Dominant → Tonic**

```python
# FSM 视角的"理想状态转移":
# T ──→ S ──→ D ──→ T
# 开   续   转   合 (起承转合)
```

### 6.2 Alda 怎么写

```alda
piano:
  (key-sig '(c major))
  o4
  # I:  C-E-G
  c4/e/g
  # IV: F-A-C
  < f4/a/>c
  # V:  G-B-D
  < g4/b/>d
  # I:  C-E-G (回家!)
  c4/e/g

  # 流行万能和弦: I - V - vi - IV
  c4/e/g         # I:   C
  < g4/b/>d      # V:   G
  a4/>c/e        # vi:  Am
  < f4/a/>c      # IV:  F
```

### 6.3 从 score JSON 怎么算

```python
# 大调各音级的和弦 root 偏移
MAJOR_DEGREES = {
    0: "I",   2: "ii",  4: "iii",
    5: "IV",  7: "V",   9: "vi",  11: "vii°",
}

def detect_progression(chord_groups: list[list[dict]], tonic: int) -> list[str]:
    """把和弦序列转成罗马数字序列 -- 像把 opcode 序列翻译成伪码"""
    result = []
    for group in chord_groups:
        midis = sorted(set(n["midi-note"] for n in group))
        if len(midis) < 3:
            continue
        # 找根音: 最低的那个 midi, 检查它 mod 12 的偏移
        for root in midis:
            offset_mod = (root - tonic) % 12
            if offset_mod in MAJOR_DEGREES:
                result.append(MAJOR_DEGREES[offset_mod])
                break
    return result

# 检查是否匹配经典进行模板
PROGRESSIONS = {
    "I-IV-V-I":    [0, 5, 7, 0],
    "ii-V-I":      [2, 7, 0],
    "I-V-vi-IV":   [0, 7, 9, 5],
}
```

### 动手练习 5

以下是 parse 输出，3 组和弦的 root 分别是 60, 67, 60 (midi)。tonic = 60, mode = major。请写出罗马数字序列。

<details>
<summary>答案</summary>

- root 60: (60-60) % 12 = 0 → **I**
- root 67: (67-60) % 12 = 7 → **V**
- root 60: (60-60) % 12 = 0 → **I**

序列 = **I-V-I**，一个"半终止 + 解决"的短进行（你可能在无数流行歌的结尾听到过 G → C 这种收束）。
</details>

---

## 7. 节拍与节奏：时间序列，sample rate = BPM

### 7.1 乐理是什么

**节奏** = 时间线上的事件序列。在 score JSON 里，每个 NoteEvent 有 `offset` (ms) 和 `duration` (ms)，这就构成了时间序列数据。

```python
# BPM = 采样率的倒数: 一拍多少毫秒
beat_ms = 60000 / bpm  # 默认 BPM=120 → 一拍 500ms

# duration 单位 ms, 转成"拍数"方便理解
beats = duration_ms / beat_ms
```

时值体系（把所有时值看成拍数的倍数/分数）：

| Alda 数字 | 名称 | 拍数 (4/4拍中) | duration (ms, BPM=120) |
|----------|------|---------------|----------------------|
| `1` | 全音符 | 4 拍 | 2000ms |
| `2` | 二分音符 | 2 拍 | 1000ms |
| `4` | 四分音符 | 1 拍 | 500ms |
| `8` | 八分音符 | 0.5 拍 | 250ms |
| `16` | 十六分音符 | 0.25 拍 | 125ms |
| `2.` | 附点二分 | 3 拍 | 1500ms |

**编程类比**：如果把音乐看成时间序列数据库，BPM 就是采样率的倒数，`offset` 是时间戳，`duration` 是值的持续时间。强拍(beat) = 采样点。

- **4/4 拍** = 每小节 4 个采样点，每个采样点间隔 = `beat_ms`
- **附点** = 原时值 * 1.5（就像 `duration *= 1.5`）
- **连音线(~)**: 把两个 duration 合并（`c4~4` = duration 500+500 = 1000ms, 一个 NoteEvent）
- **连奏线(~)**: 平滑过渡（两个 NoteEvent, 但 `slurred?` 标记为 true）
- **三连音**: 把一个时值等分为三份（`{c d e}4` = 3 个音瓜分 500ms = 每个约 166.7ms）

### 7.2 Alda 怎么写

```alda
(tempo! 120)       # 全局 120 BPM → beat_ms = 500

# 基本时值
c1                 # 全音符, 2000ms
c2                 # 二分音符, 1000ms
c4                 # 四分音符, 500ms (默认)
c8 d e f g         # 五个八分音符, 时值继承

# 附点
c2.                # 附点二分 = 3 拍 = 1500ms
c4..               # 双附点 = 1.75 拍

# 连音线 (合并时值)
c4~4               # 两个四分音符连接 → 一个 NoteEvent, duration=1000ms

# 连奏线 (平滑过渡)
c4~ d~ e~ f        # 四个 NoteEvent, slurred? = true

# Cram 表达式: 压缩到指定时值
{c d e f g}2       # 5 个音符压缩到二分音符 (1000ms) 内 → 每个 200ms
{c d e}4           # 三连音: 3 个音在 500ms 内 → 每个 166.7ms

# 任意时值 (毫秒/秒)
c500ms             # 500 毫秒
d2s                # 2 秒
e1.5s              # 1.5 秒
```

### 7.3 从 score JSON 怎么算

```python
def note_density(events: list[dict]) -> float:
    """音符密度 (NPS = notes per second), 像计算吞吐量"""
    if not events:
        return 0.0
    sorted_e = sorted(events, key=lambda x: x["offset"])
    total_span_ms = (sorted_e[-1]["offset"] + sorted_e[-1]["duration"]
                     - sorted_e[0]["offset"])
    return len(events) / (total_span_ms / 1000.0) if total_span_ms > 0 else 0.0

def syncopation_approx(events: list[dict], bpm: float = 120.0) -> float:
    """切分音近似度量: 非强拍 onset 的比例。
    强拍 = offset 对齐到 beat 网格。切分音 = 故意不对齐, 制造"摇摆感"。
    就像采样点落在量化网格之外的比例。"""
    beat_ms = 60000.0 / bpm
    tol = beat_ms * 0.05
    weak = 0
    for e in events:
        offset_beats = e["offset"] / beat_ms
        if abs(offset_beats - round(offset_beats)) > tol:
            weak += 1
    return weak / len(events) if events else 0.0

def duration_entropy(events: list[dict]) -> float:
    """时值多样性 -- 检查是不是只有一种 duration (如全是四分音符)。
    像代码中检查是否只有一种语句类型。"""
    from collections import Counter
    from math import log2
    counter = Counter(round(e["duration"]) for e in events)
    total = len(events)
    entropy = sum(-c/total * log2(c/total) for c in counter.values())
    return entropy
```

### 动手练习 6

下面这段 Alda 有几个 NoteEvent？每个 duration 是多少？总时长是多少 ms（BPM=120）？

```alda
piano: c4~4 d8 e8 f8 g8 c2
```

然后写一段代码计算"二分音符占所有音符时长的比例"。

<details>
<summary>答案</summary>

BPM=120 → beat_ms=500ms。

| 音符 | duration (ms) | 说明 |
|------|-------------|------|
| c4~4 | 1000 | 两个四分音符连在一起 |
| d8 | 250 | 八分音符 |
| e8 | 250 | 八分音符 |
| f8 | 250 | 八分音符 |
| g8 | 250 | 八分音符 |
| c2 | 1000 | 二分音符 |

6 个 NoteEvent，总时长 = 1000+250+250+250+250+1000 = 3000ms。

二分音符 (duration=1000ms) 占比 = (1000+1000) / 3000 ≈ 66.7%。

```python
def half_note_ratio(events: list[dict], bpm: float = 120.0) -> float:
    half_ms = 2 * 60000.0 / bpm  # 二分音符 = 2拍
    total = sum(e["duration"] for e in events)
    half = sum(e["duration"] for e in events
               if abs(e["duration"] - half_ms) < 10)
    return half / total if total > 0 else 0.0
```
</details>

---

## 8. 曲式结构：控制流，block + loop + goto

### 8.1 乐理是什么

写代码你不会把所有逻辑写在一个 5000 行的 main() 里--你会拆成函数、用循环、有时用 goto（好吧，你不会用 goto，但 Alda 会）。

**编程类比**：

- **乐句 (phrase)** = 一个代码块 `{ ... }`，有自己的入口和出口
- **乐段 (section)** = 一个函数，有名字（intro, verse, chorus...）
- **重复 (repeat)** = `for i in range(N): ...`
- **标记跳转 (marker)** = `goto label`（但只向前跳，所以比 C 的 goto 安全）
- **交替结尾** = `if iteration in [1,2]: ... elif iteration == 3: ...`

经典曲式：

| 曲式 | 伪代码 | 实例 |
|------|--------|------|
| AABA | `A(); A(); B(); A()` | 32 小节爵士标准曲 |
| Verse-Chorus | `while True: verse(); chorus()` | 几乎所有流行歌 |
| 12-Bar Blues | `A*4; B*2; A*2; C; B; A*2` | 蓝调标准 |

### 8.2 Alda 怎么写

```alda
piano:
  (tempo! 100)

  %intro                          # 放标记: 像 label:
  c4 e g > c2. <                  # 4 小节引子

  %verse                          # 主歌
  c8 d e f g4. a8 g2
  c8 d e c d4 e2

  %chorus                         # 副歌
  c4/e/g > c2 <
  a4/>c/e > c2 <
  f4/a/>c a2
  g4/b/>d d2

  @verse                          # 像 goto verse; 但只是回到标记位置继续
  @chorus                         # 再一遍副歌

  %outro
  r1                              # 全休止
  c1                              # 结束
```

**重复与交替结尾**（最像控制流的特性）：

```alda
piano:
  [ c4 d e f          # 主旋律 (A)
    [g8 f e d c4] '1  # 第 1 次用这个结尾
    [g8 a b > c2.] '2 # 第 2 次用这个结尾
  ] *2                 # 整个块重复 2 次
```

翻译成伪代码：

```python
for i in [1, 2]:
    play(c4, d, e, f)       # 主旋律
    if i == 1:
        play(g8, f, e, d, c4)   # 结尾 A
    elif i == 2:
        play(g8, a, b, >c2.)    # 结尾 B
```

### 8.3 从 score JSON 怎么算

```python
def segment_by_rest(events: list[dict], rest_threshold_ms: float = 500.0) -> list[list[dict]]:
    """通过长休止 (>500ms gap) 自动分段 -- 像用空行分隔代码段落"""
    sorted_e = sorted(events, key=lambda x: x["offset"])
    segments = []
    current = [sorted_e[0]] if sorted_e else []

    for i in range(1, len(sorted_e)):
        prev_end = sorted_e[i-1]["offset"] + sorted_e[i-1]["duration"]
        gap = sorted_e[i]["offset"] - prev_end
        if gap > rest_threshold_ms and len(current) >= 4:
            segments.append(current)
            current = []
        current.append(sorted_e[i])

    if current:
        segments.append(current)
    return segments

def repetition_score(events: list[dict], window: int = 4) -> float:
    """重复度: 检测连续音程序列是否有重复模式。
    像检测代码中 copy-paste 的比例 -- 太高 = 机械化, 太低 = 无主题"""
    intervals = []
    for i in range(1, len(events)):
        intervals.append(events[i]["midi-note"] - events[i-1]["midi-note"])

    pattern_count = 0
    total = len(intervals) - window + 1
    for i in range(total - window):
        if intervals[i:i+window] == intervals[i+window:i+2*window]:
            pattern_count += 1
    return pattern_count / total if total > 0 else 0.0
```

### 动手练习 7

下面这段 Alda 有两个嵌套重复块。内层的 `'1` 和 `'2` 会怎样展开？

```alda
piano:
  [ c4 d e f
    [g4 a b > c] '1
    [r1]         '2
  ] *2
```

<details>
<summary>答案</summary>

展开结果：

- 第 1 遍: `c4 d e f g4 a b > c`
- 第 2 遍: `c4 d e f r1`

注意 `r1` 是全休止符(4拍) -- 第 2 遍主旋律后面直接安静。

这就像一个 for 循环里嵌套 if: `for i in [1,2]: play_main(); play_ending(i)`。
</details>

---

## 9. 多声部：goroutine，独立执行 + 自动 join

### 9.1 乐理是什么

**编程类比**：多个乐器声部 = 多个 goroutine（或线程）。

```
go func() { playMelody() }()   // 小提琴: 旋律
go func() { playHarmony() }()   // 中提琴: 和声填充
go func() { playBass() }()      // 大提琴: 低音线
// Alda 自动 WaitGroup.Wait()
// 所有声部结束后, 输出完整的 score JSON
```

每个声部维护自己的**独立上下文**（八度、时值、音量、offset），就像每个线程有自己的栈。声部之间通过调整时值自动同步 offset。

设计多声部时，要考虑三条准则：

1. **音域不冲**：高声部全部 midi > 低声部全部 midi（避免 "声部交叉"）
2. **节奏互补**：一个 busy 时另一个 sparse（像流水线不同 stage 的 busy/idle 比）
3. **乐器音域约束**：每个乐器的 midi 范围是有限的（就像 uint8 不能存 >255 的值）

常见乐器 MIDI 音域速记：

| 乐器 | MIDI 范围 | 类比 |
|------|----------|------|
| Piano | 21-108 | `int8_t` 全量程 |
| Violin | 55-100 | 高位优先 |
| Cello | 36-84 | 中低位 |
| Flute | 60-96 | 高位, 无符号数 |
| Tuba | 29-55 | 低位, 接近 0 |

### 9.2 Alda 怎么写

```alda
# 多乐器 = 多个 goroutine 分别启动
violin "v1":
  o4 e4 f g a b > c d e2        # 旋律线

violin "v2":
  o4 c4 d e f g a b > c2        # 副旋律

viola:
  o3 a2 > c4 c d2 c             # 中音填充

cello:
  o3 f2 c4 f < b-2 > f          # 低音线

# 乐器组: 多个乐器执行相同代码
violin/viola/cello "strings":
  c1~1~1

# Voice: 同一乐器内多个独立声部 (像单进程多线程)
piano:
  V1: c4 e g > c2.              # 右手: 高音旋律
  V2: c2 e2                     # 左手: 低音伴奏
  V0: c1                        # V0: 切回单声部, join 所有 Voice
```

Voice 的关键语义（与和弦不同）：

- 和弦 `/` 结束后 offset 取**最短**音符（尽早开始下一个）
- Voice 组结束后 offset 取**最长** Voice（等所有人完成，类似 WaitGroup.Wait()）
- `V0:` 是 "结束多声部, 恢复到单声部" 的信号

### 9.3 从 score JSON 怎么算

```python
def voice_crossing_check(events_high: list[dict], events_low: list[dict], tol_ms: float = 100.0) -> list[dict]:
    """检测声部交叉: 高声部音符的 midi 是否 < 同时刻低声部音符的 midi
    像检查两个并发数据结构是否有重叠访问范围"""
    crossings = []
    for h in events_high:
        h_end = h["offset"] + h["duration"]
        for l in events_low:
            l_end = l["offset"] + l["duration"]
            overlap = min(h_end, l_end) - max(h["offset"], l["offset"])
            if overlap > 0 and h["midi-note"] < l["midi-note"]:
                crossings.append({
                    "offset": max(h["offset"], l["offset"]),
                    "high_midi": h["midi-note"],
                    "low_midi": l["midi-note"],
                })
    return crossings

INSTRUMENT_RANGES = {
    "piano": (21, 108), "violin": (55, 100), "cello": (36, 84),
    "flute": (60, 96), "trumpet": (55, 82), "tuba": (29, 55),
    "guitar": (40, 84), "bass-guitar": (28, 55),
}

def range_check(events: list[dict], instrument: str) -> dict:
    """验证音符是否在乐器可演奏范围内 -- 像检查值是否超出类型上限"""
    lo, hi = INSTRUMENT_RANGES[instrument]
    bad = [e["midi-note"] for e in events if not (lo <= e["midi-note"] <= hi)]
    return {
        "total": len(events), "bad_count": len(bad),
        "compliance": 1.0 - len(bad)/len(events) if events else 1.0,
        "out_of_range": sorted(set(bad)),
    }
```

### 动手练习 8

找出以下多声部代码的结构问题（提示：检查声部交叉和时值）：

```alda
violin:
  o4 c4 d e f g2

cello:
  o4 c4 d e f g2
```

`alda parse -o data` 后，violin 和 cello 的 midi 相同。这在传统写作中有什么问题？

<details>
<summary>答案</summary>

大提琴和演奏的 midi 范围与小提琴完全相同 (`o4` 是中央区，cello 的实际舒适区在 `o3` 即 MIDI 36-55 左右)。问题：

1. **Cello 音域太高**：`o4 c` = MIDI 60，cell 舒适上限才 84，虽然技术上可演奏但不是最佳音色
2. **完全同度**：两个声部演奏完全相同的内容 = 浪费一个声部，没有声部独立性

修正：

```alda
violin:
  o4 c4 d e f g2    # 旋律在舒适区

cello:
  o3 c2 f2 c1       # 低音在舒适区, 节奏稀疏 (长时值), 与旋律互补
```
</details>

---

## 10. 贯穿案例：一首 8 小节 C 大调钢琴小曲的进化史

现在我们把所有概念串起来，从零开始写一首完整的钢琴曲。每加入一个新概念，我们就改进它。

### v0.1: 只有旋律 (单音)

```alda
piano:
  (tempo! 100)
  (key-sig '(c major))
  o4
  c4 d e f | g a b > c |           # 小节 1-2: C 大调音阶上行
  < b a g f | e d c2 |              # 小节 3-4: 下行
  c4 e g e | c e g > c |           # 小节 5-6: 分解和弦式旋律
  < b g e c | d2 c2 |               # 小节 7-8: 下行结束
```

### v0.2: + 和弦伴奏 (Voice)

```alda
piano:
  (tempo! 100)
  (key-sig '(c major))

  V1:                             # 右手: 旋律 (高音)
    o4
    c4 d e f | g a b > c |
    < b a g f | e d c2 |
    c4 e g e | c e g > c |
    < b g e c | d2 c2 |

  V2:                             # 左手: 和弦伴奏 (低音)
    o3
    c2 e2 | g2 > c2 < |           # I (C) 和弦分解
    g2 < b2 | f2 > c2 < |         # V → IV 进行
    c2 e2 | c2 e2 |               # I
    g2 < b2 | f2 c2 |             # V → IV → I

  V0: c1                          # 结束
```

### v0.3: + 和声设计 (I-V-vi-IV + 和弦进行)

用上节 6 的"流行万能和弦"来设计左手：

```alda
piano:
  (tempo! 100)
  (key-sig '(c major))

  V1:                             # 右手: 旋律
    o4
    c8 d e c d e f d |            # 小节 1-2: I 上的旋律
    e8 f g e f g a f |            # 小节 3-4: V 上的旋律
    g8 a g f e d c4 |             # 小节 5-6: vi → IV 上的旋律
    d8 e d c d2 |                 # 小节 7-8: 收束

  V2:                             # 左手: 流行进行 I-V-vi-IV
    o3
    c4/e/g c4/e/g                 # 小节 1: I (C)
    g4/b/>d g4/b/d                 # 小节 2: V (G) [注意 d 在 o4]
    a4/>c/e a4/c/e                 # 小节 3: vi (Am)
    f4/a/>c f4/a/c                 # 小节 4: IV (F)
    c4/e/g c4/e/g                 # 小节 5: I
    g4/b/d g4/b/d                 # 小节 6: V
    a4/c/e a4/c/e                 # 小节 7: vi
    f4/a/c c4/e/g                 # 小节 8: IV → I (终止式)

  V0: c1
```

### v0.4: + 非和弦音 + 节奏变化 (passing tones + 切分)

在强拍和弦骨架之间插入**经过音**（不在和弦里的音），让旋律更有流动感：

```alda
piano:
  (tempo! 100)
  (key-sig '(c major))

  V1:                             # 右手: 旋律 (含经过音)
    o4
    # 小节 1-2: I, 旋律围绕 C-E-G
    c8 d e4~ e8 d | c4. e8~ e4 |
    # 小节 3-4: V, 旋律围绕 G-B-D
    g8 a b4~ b8 a | g4. b8~ b4 |
    # 小节 5-6: vi→IV, 引入附点节奏
    a4. b8 > c4 < | a8 g f e d4 |
    # 小节 7-8: 收束
    e8 f e d c4 | d2 c2 |

  V2:                             # 左手: 和弦进行
    o3
    # I (C): 小节 1-2
    c2 e4/g | c2 e4/g |
    # V (G): 小节 3-4
    < g2 b4/>d | g2 b4/d |
    # vi (Am): 小节 5
    a2 > c4/e | <
    # IV (F): 小节 6
    f2 a4/>c |
    # 终止式: 小节 7-8
    g2 b4/d | f4/a/>c < c4/e/g |

  V0: c1
```

### v0.5: + 力度 (dynamics) + 曲式标记

最后加上力度变化和结构标记：

```alda
# 钢琴小曲: C 大调, 4/4 拍, 8 小节
# 曲式: A (小节 1-4) + B (小节 5-8)

piano:
  (tempo! 100)
  (quant! 85)                     # 量化 85% (留一点"人味")

  (key-sig '(c major))

  (vol 48)                        # p = 柔和起始

  %section-a                      # A 段: I → V, 安静
  V1:
    o4
    (vol 54)                      # mf
    c8 d e4~ e8 d | c4. e8~ e4 |
    g8 a b4~ b8 a | g2 |

  V2:
    o3
    c2 e4/g | c2 e4/g |
    < g2 b4/>d | g4 < b4/>d g2 |

  %section-b                      # B 段: vi → IV → I, 渐强
  V1:
    o4
    (vol 64)                      # f
    a4. b8 > c8~ c4 < |           # 高潮: 跳高八度, 带附点
    a8 g f e d4 |
    e8 f e d c4 | d2 c2 |

  V2:
    o3
    a2 > c4/e | <
    f2 a4/>c |
    g2 b4/d | f4/a/>c < c4/e/g |

  V0: (vol 54) c1                 # 安静结束
```

现在我们验证这个作品：`alda parse -o data` 后，我们期望：

- `metric_tonality(events, 60, "major")` ≈ 0.85+ （绝大部分音符在 C 大调内）
- `metric_consonance_ratio(events)` 的不协和 < 20% （大部分和弦是协和的）
- `chord_vs_melody_ratio` ≈ 0.4-0.6 （一半旋律一半和弦，中等织体）
- `pitch_range` ≈ 24-36 半音 （适合钢琴）
- `phrase_count` ≈ 2 （A/B 两段）

---

## 11. 程序员-乐理速查表（可打印 B4）

打印建议: B4 纸 (257mm x 364mm), 横向, 字号 8-9pt。

```
┌─────────────────────────────────────────────────────────────────────────────────────────────────────┐
│                           程 序 员 × 乐 理 · Alda 速 查 表                                          │
│                         适用于 alda parse -o data JSON 的度量计算                                    │
├─────────────────────────────────────────────────────────────────────────────────────────────────────┤
│                                                                                                     │
│  ┌─ 音高 / MIDI ───────────────────────────────────────────────────────────────────────┐            │
│  │ midi_note = 0..127 的整数     |  A4=69=440Hz  |  freq = 440 * 2^((midi-69)/12)     │            │
│  │ Alda: o4 c  (八度4=midi 60)   |  o4 c+ (61)  |  < > 八度升降  |  sharp(+), flat(-) │            │
│  │ JSON: events[].midi-note       |  度量: pitch_range(), pitch_entropy()              │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 音程 / 协和度 (mod 12) ───────────────────────────────────────────────────────────┐            │
│  │  0:八度  1:小二  2:大二  3:小三  4:大三  5:纯四  6:三全音  7:纯五  8:小六           │            │
│  │  9:大六  10:小七  11:大七                                                          │            │
│  │  完全协和(0,5,7)  不完全协和(3,4,8,9)  不协和(1,2,6,10,11)                         │            │
│  │  Alda: 和弦 c/e/g 中 C-E=4半音  |  JSON: classify_interval(midi_a, midi_b)          │            │
│  │  度量: consonance_ratio(events) → {perfect, imperfect, dissonant}                    │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 音阶 / 调式 (12位bitmask) ───────────────────────────────────────────────────────┐            │
│  │  major:     0,2,4,5,7,9,11  (101011010101)    minor:     0,2,3,5,7,8,10           │            │
│  │  dorian:    0,2,3,5,7,9,10     phrygian:  0,1,3,5,7,8,10                          │            │
│  │  lydian:    0,2,4,6,7,9,11     mixolydian: 0,2,4,5,7,9,10                         │            │
│  │  locrian:   0,1,3,5,6,8,10                                                         │            │
│  │  Alda: (key-sig '(c major))   |  度量: tonality_score(events, tonic, mode)          │            │
│  │  in_scale(midi) ≡ (midi - tonic) % 12 ∈ mask                                      │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 和弦 (同时发音的midi组) ───────────────────────────────────────────────────────────┐            │
│  │  三和弦: major[0,4,7]  minor[0,3,7]  dim[0,3,6]  aug[0,4,8]                        │            │
│  │  七和弦: maj7[0,4,7,11]  dom7[0,4,7,10]  m7[0,3,7,10]  m7b5[0,3,6,10]  dim7[0,3,6,9]│            │
│  │  Alda: c/e/g (C大三)  c/e/g/b- (C属七)  c-/e-/g- (C小三)                            │            │
│  │  JSON: group_by_offset(events, tol=5ms) → identify_chord()                          │            │
│  │  度量: chord_type_distribution(), chord_vs_melody_ratio()                            │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 和弦进行 (FSM 状态转移) ──────────────────────────────────────────────────────────┐            │
│  │  大调各音级: I(0,M) ii(2,m) iii(4,m) IV(5,M) V(7,M) vi(9,m) vii°(11,d)            │            │
│  │  经典进行:  I-IV-V-I [0,5,7,0]    ii-V-I [2,7,0]    I-V-vi-IV [0,7,9,5]           │            │
│  │  功能组:  Tonic(I,vi,iii)  Subdominant(IV,ii)  Dominant(V,vii°)                     │            │
│  │  Alda: 用斜线/和弦 + 标记%verse, 或手动排和弦序列                                    │            │
│  │  JSON: detect_progression(chord_groups, tonic) → ["I","V","vi","IV"]                │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 节奏 / 时值 (BPM=120 → 1beat=500ms) ─────────────────────────────────────────────┐            │
│  │  1=全(4拍,2000ms)  2=二分(2拍,1000ms)  4=四分(1拍,500ms)                            │            │
│  │  8=八分(250ms)  16=十六分(125ms)  2.=附点二分(3拍,1500ms)                           │            │
│  │  Alda: c4~4 (连音=合并duration)  c4~ d (连奏=slurred)  {c d e}4 (三连音)            │            │
│  │  Alda: 任意时值 c500ms / c2s / c1.5s                                               │            │
│  │  JSON 字段: offset(ms) duration(ms) audible-duration(ms)                            │            │
│  │  度量: note_density(nps) syncopation_approx() duration_entropy() rest_ratio()        │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 曲式 / 控制流 ───────────────────────────────────────────────────────────────────┐            │
│  │  %name 放标记  @name 跳到标记  [ ... ] *N 重复  [x]'1 [y]'2-3 交替结尾              │            │
│  │  经典: AABA, Verse-Chorus, 12-Bar Blues                                             │            │
│  │  JSON: segment_by_rest(events, gap>500ms) → 乐句分界                                 │            │
│  │  度量: phrase_count()  repetition_score()                                            │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 多声部 / goroutine ──────────────────────────────────────────────────────────────┐            │
│  │  piano: V1: 右手 V2: 左手 V0: join    violin/viola/cello: 乐器组                   │            │
│  │  声部交叉: 高声部midi < 低声部midi → 避免                                             │            │
│  │  JSON: part 字段区分声部  |  度量: voice_crossing_check(), range_check()             │            │
│  │  钢琴(21-108) 小提琴(55-100) 大提琴(36-84) 长笛(60-96) 小号(55-82)                  │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ NoteEvent JSON 字段速查 ──────────────────────────────────────────────────────────┐            │
│  │  {"midi-note":60, "offset":0.0, "duration":500.0, "audible-duration":450.0,        │            │
│  │   "volume":0.54, "track-volume":0.787, "panning":0.5, "part":"part001"}            │            │
│  │  - offset, duration 单位 ms  |  volume, panning 范围 [0,1]                          │            │
│  │  - audible-duration = duration * quantization (默认 0.9)                             │            │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  ┌─ 常用度量函数速查 (所有输入来自 events[]) ─────────────────────────────────────────┐            │
│  │  tonality_score()    调内音比例         0.7-1.0    pitch_range()     音域跨度     12-48 │          │
│  │  pitch_entropy()     音高多样性         0.5-0.9    note_density()    音符密度nps  1-10  │          │
│  │  consonance_ratio()  协和度统计         取决于风格  chord_type_dist() 和弦类型分布   —    │          │
│  │  syncopation()       切分音度           0.2-0.5    rest_ratio()      休止比例   0.05-0.3│          │
│  │  repetition_score()  重复度             0.1-0.5    voice_crossings()  声部交叉      0    │          │
│  │  phrase_count()      乐句数             > 1        duration_entropy() 时值多样性    —    │          │
│  └─────────────────────────────────────────────────────────────────────────────────────┘            │
│                                                                                                     │
│  核心心法: 所有乐理概念本质上是"对 midi-note, offset, duration 三个字段做集合/算术/统计运算"     │
│  编程类比: 音高=数组索引 | 音阶=bitmask | 和弦=同时置位 | 调性=命名空间 | 节奏=时间序列 | 声部=goroutine │
└─────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 附录 A: Alda CLI 快速参考

```bash
# 安装
brew install alda          # macOS
scoop install alda         # Windows

# 核心命令
alda play file.alda        # 播放乐谱
alda parse -o data file.alda   # 输出 score JSON (含 events 数组)
alda parse -o events file.alda # 输出 ScoreUpdate 事件 JSON
alda parse -o ast file.alda    # 输出 AST JSON
alda export -o midi file.alda  # 导出 MIDI 文件

# 解析错误格式: <filename>:<line>:<column> <message>
# 例如: test.alda:3:5 Unexpected 'x' at the top level
```

## 附录 B: 度量函数完整签名一览

| 函数 | 输入 | 输出 | 含义 |
|------|------|------|------|
| `in_scale_ratio(events, tonic, mode)` | midi-note | 0-1 | 调内音比例 |
| `pitch_range(events)` | midi-note | {min, max, span} | 音域 |
| `pitch_entropy(events)` | midi-note | 0-1 | 音高多样性 |
| `classify_interval(midi_a, midi_b)` | (mod 12) | Consonance | 协和度 |
| `consonance_ratio(events)` | midi-note, offset | {三档比例} | 协和统计 |
| `group_by_offset(events)` | offset | 和弦组列表 | 和弦分割 |
| `identify_chord(note_group)` | midi-note | [(type, root)] | 和弦识别 |
| `detect_progression(chord_groups, tonic)` | midi-note, offset | [罗马数字] | 进行检测 |
| `note_density(events)` | offset | nps | 音符密度 |
| `syncopation_approx(events, bpm)` | offset | 0-1 | 切分度 |
| `duration_entropy(events)` | duration | float | 时值多样性 |
| `rest_ratio(events)` | offset, duration | 0-1 | 休止比例 |
| `segment_by_rest(events)` | offset, duration | [段落] | 乐句分界 |
| `repetition_score(events)` | midi-note | 0-1 | 重复度 |
| `voice_crossing_check(high, low)` | midi-note, offset | [交叉事件] | 声部交叉 |
| `range_check(events, instrument)` | midi-note | 合规度 | 音域检查 |
| `metric_parallel_octaves_fifths(events)` | midi-note, offset | int | 平行八/五度 |
| `generate_score_summary(events, tonic, mode)` | 全部字段 | dict | 聚合报告 |

---

*本文严格基于 Alda 源码 (client/model/\*.go + client/parser/\*.go) 的数据结构编写。*
*所有 JSON 字段名、度量公式均可在 `docs/research/music-theory.md` 和 `docs/research/alda-pipeline.md` 中找到原始定义。*
