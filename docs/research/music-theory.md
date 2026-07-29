# 程序员最小乐理知识库

> 目标读者: 完全不懂乐理但有编程背景的人。内容覆盖 LLM agent 生成与评价 Alda 乐谱所需的最小乐理知识。
>
> 每个主题分两部分: (a) Alda 怎么写; (b) 如何从 `alda parse -o data` 输出的 JSON 用代码计算。
>
> JSON 数据结构参考 `client/model/note.go` 中的 `NoteEvent`:
> ```
> { "midi-note": int, "offset": float64, "duration": float64,
>   "audible-duration": float64, "volume": float64, "track-volume": float64,
>   "panning": float64, "part": string, "midi-channel": int }
> ```
> - `offset` 和 `duration` 单位均为毫秒 (ms)
> - `midi-note` 为 0-127 的整数, 代表音高
> - 默认 tempo = 120 BPM, 默认四分音符 = 1 拍 = 500ms (60000/120)

---

## 1. 音高与 MIDI 编号映射

### 1.1 概念

MIDI 用一个 0-127 的整数表示音高。国际标准: **A4 = 69 = 440 Hz**。

十二平均律: 相邻半音频率比为 2^(1/12)。每升高一个八度 (12 个半音), 频率翻倍。

**MIDI 编号 → 频率 (Hz)**
```
f = 440 * 2^((midi_note - 69) / 12)
```

**频率 → MIDI 编号**
```
midi_note = 69 + 12 * log2(f / 440)
```

### 1.2 Alda 写法

Alda 内部计算 MIDI 编号的公式 (见 `client/model/pitch.go`):
```
baseMidiNote = (octave + 1) * 12 + letterInterval
letterInterval: C=0, D=2, E=4, F=5, G=7, A=9, B=11
```
然后加减升降号 (sharp `+` = +1, flat `-` = -1), 再叠加 transposition。

```alda
# 设置八度
o4              # 八度 4, 默认值
o5              # 八度 5
<               # 降低一个八度
>               # 升高一个八度

# 音名 + 升降号
c               # C 自然 (八度 4 时 = MIDI 60)
c+              # C# (升半音)
c-              # Cb (降半音)
c_              # C 还原 (覆盖调号)
c++             # C 重升 = D
c--             # C 重降 = Bb

# 调号影响: 设置后, 不带升降号的音符自动匹配调号
(key-signature "f+ c+ g+")
```

在 JSON 中直接读取 `midi-note` 字段即可, 无需自己计算。

### 1.3 代码计算

```python
import math

def midi_to_freq(midi_note: int, a4_freq: float = 440.0) -> float:
    """MIDI 编号 → 频率 (Hz)"""
    return a4_freq * (2 ** ((midi_note - 69) / 12))

def freq_to_midi(freq: float, a4_freq: float = 440.0) -> int:
    """频率 (Hz) → 最接近的 MIDI 编号"""
    return round(69 + 12 * math.log2(freq / a4_freq))

# 验证: A4 = 69 = 440Hz
assert abs(midi_to_freq(69) - 440.0) < 0.01
assert freq_to_midi(440.0) == 69
```

---

## 2. 半音、全音与八度

### 2.1 概念

- **半音 (semitone/half-step)**: 音高的最小间隔单位, 相邻 MIDI 编号差 1
- **全音 (whole-tone/whole-step)**: 2 个半音, 相邻 MIDI 编号差 2
- **八度 (octave)**: 12 个半音, 频率翻倍; 同名音例如 C4 → C5 差值 12

### 2.2 Alda 写法

```alda
c4 c+4          # C 到 C#, 差 1 个半音
c4 d4           # C 到 D, 差 2 个半音 (1 个全音)
< c             # 降一个八度, MIDI 编号减 12
> c             # 升一个八度, MIDI 编号加 12
o5 c            # 跳到八度 5, 比 o4 c 高 12
```

### 2.3 代码计算

```python
def semitone_diff(midi_a: int, midi_b: int) -> int:
    """两音之间的半音数, 取绝对值"""
    return abs(midi_a - midi_b)

def is_octave(midi_a: int, midi_b: int) -> bool:
    """判断两音是否同名 (相差整数个八度)"""
    return abs(midi_a - midi_b) % 12 == 0

def notes_in_octave_range(notes: list[int], midi_low: int, midi_high: int) -> list[int]:
    """筛选在指定 MIDI 音域内的音符"""
    return [n for n in notes if midi_low <= n <= midi_high]

# MIDI 编号转八度数 (C0 为八度 0 的起点)
def midi_to_octave(midi_note: int) -> int:
    return (midi_note // 12) - 1  # MIDI 0 = C-1

def midi_to_note_name(midi_note: int) -> str:
    """MIDI 编号 → 音名 """
    names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
    octave = (midi_note // 12) - 1
    return f"{names[midi_note % 12]}{octave}"
```

---

## 3. 大小调音阶与调号

### 3.1 概念

**音阶 (scale)** 是选定的 7 个音 (来自 12 个半音), 按固定全音/半音模式排列。
**调号 (key signature)** 指定哪些音默认升/降, 使音阶"落在白键上"。
**调性 (key/tonality)** = 主音 (tonic) + 调式 (mode)。

七种调式, 以 C 为主音的半音偏移模式 (各数字为相对主音的 MIDI 偏移):

| 调式 | Alda 名 | 半音偏移 | 举例 (C tonic) |
|------|---------|----------|----------------|
| 大调 (自然大调) | `ionian` / `major` | 0,2,4,5,7,9,11 | C D E F G A B |
| 自然小调 | `aeolian` / `minor` | 0,2,3,5,7,8,10 | C D Eb F G Ab Bb |
| 多利亚 | `dorian` | 0,2,3,5,7,9,10 | C D Eb F G A Bb |
| 弗里吉亚 | `phrygian` | 0,1,3,5,7,8,10 | C Db Eb F G Ab Bb |
| 利底亚 | `lydian` | 0,2,4,6,7,9,11 | C D E F# G A B |
| 混合利底亚 | `mixolydian` | 0,2,4,5,7,9,10 | C D E F G A Bb |
| 洛克里亚 | `locrian` | 0,1,3,5,6,8,10 | C Db Eb F Gb Ab Bb |

### 3.2 Alda 写法

```alda
# 方式 1: 手动指定升降号
(key-signature "f+ c+ g+ d+")   # E 大调 / C# 小调

# 方式 2: 用调式指定 (Alda 支持的调式见上表)
(key-sig '(c major))            # C 大调, 无升降号
(key-sig '(d minor))            # D 小调, 一个降号 (Bb)
(key-sig '(g mixolydian))       # G Mixolydian

# 三行等价写法 (A 自然小调)
(key-sig '(a minor))
(key-sig '(a aeolian))
(key-signature "")
# (A minor = C major 的关系小调, 无升降号)
```

### 3.3 代码计算: 判断音符是否在调内

找出所有合法的 MIDI 偏移 (即"调内音") — 无论哪个八度, `midi_note % 12` 落在这 7 个值里就算。

```python
# 七种调式的 MIDI 偏移 (相对于主音, 差值为 mod 12 的 pitch class)
SCALE_PATTERNS = {
    "major":     [0, 2, 4, 5, 7, 9, 11],  # ionian
    "minor":     [0, 2, 3, 5, 7, 8, 10],  # aeolian
    "dorian":    [0, 2, 3, 5, 7, 9, 10],
    "phrygian":  [0, 1, 3, 5, 7, 8, 10],
    "lydian":    [0, 2, 4, 6, 7, 9, 11],
    "mixolydian":[0, 2, 4, 5, 7, 9, 10],
    "locrian":   [0, 1, 3, 5, 6, 8, 10],
}

def scale_pitch_classes(tonic_midi: int, mode: str = "major") -> set[int]:
    """返回调内所有 pitch class (midi_note % 12 的合法值)"""
    offsets = SCALE_PATTERNS[mode]
    return {(tonic_midi + o) % 12 for o in offsets}

def is_in_scale(midi_note: int, tonic_midi: int, mode: str = "major") -> bool:
    """判断单个音是否在调内"""
    return (midi_note - tonic_midi) % 12 in scale_pitch_classes(tonic_midi, mode)

def in_scale_ratio(notes: list[int], tonic_midi: int, mode: str = "major") -> float:
    """返回调内音的比例 (0.0 ~ 1.0)"""
    if not notes:
        return 1.0
    pc_set = scale_pitch_classes(tonic_midi, mode)
    in_count = sum(1 for n in notes if n % 12 in pc_set)
    return in_count / len(notes)
```

---

## 4. 音程及协和/不协和分类

### 4.1 概念

**音程 (interval)**: 两个音之间的半音距离。

协和度分为三级:
- **完全协和 (perfect consonance)**: 同度、八度、纯五度、纯四度
- **不完全协和 (imperfect consonance)**: 大三度、小三度、大六度、小六度
- **不协和 (dissonance)**: 小二度、大二度、三全音、小七度、大七度

> 注意: 这里的分类是传统和声学的简化。纯四度在某些语境下被视为不协和 (如二声部对位), 但对于和弦检测的通用目的, 此分类足够使用。

### 4.2 Alda 写法

```alda
# 单音旋律 (水平方向, 音程体现在相邻音符的 midi-note 差)
c4 d4 e4 f4 g4

# 和弦 (垂直方向, 音程体现在同时发声的各音之间)
c4/e4/g4       # C 大三和弦, 根音到三音=4半音, 根音到五音=7半音
```

### 4.3 代码计算: 从 score JSON 判断音程与协和度

```python
from enum import Enum

class Consonance(Enum):
    PERFECT_CONSONANCE = 2    # 完全协和
    IMPERFECT_CONSONANCE = 1  # 不完全协和
    DISSONANCE = 0            # 不协和

# 半音数 (mod 12, 忽略八度差) → 协和度分类
INTERVAL_CONSONANCE = {
    0:  Consonance.PERFECT_CONSONANCE,    # 同度
    1:  Consonance.DISSONANCE,            # 小二度
    2:  Consonance.DISSONANCE,            # 大二度
    3:  Consonance.IMPERFECT_CONSONANCE,  # 小三度
    4:  Consonance.IMPERFECT_CONSONANCE,  # 大三度
    5:  Consonance.PERFECT_CONSONANCE,    # 纯四度
    6:  Consonance.DISSONANCE,            # 三全音 (增四度/减五度)
    7:  Consonance.PERFECT_CONSONANCE,    # 纯五度
    8:  Consonance.IMPERFECT_CONSONANCE,  # 小六度
    9:  Consonance.IMPERFECT_CONSONANCE,  # 大六度
    10: Consonance.DISSONANCE,            # 小七度
    11: Consonance.DISSONANCE,            # 大七度
}

def interval_name(semitones_mod12: int) -> str:
    """半音差 (mod 12) → 音程名称"""
    names = ["同度", "小二度", "大二度", "小三度", "大三度",
             "纯四度", "三全音", "纯五度", "小六度", "大六度",
             "小七度", "大七度"]
    return names[semitones_mod12 % 12]

def classify_interval(midi_a: int, midi_b: int) -> Consonance:
    """两音协和度, mod 12 版的简化分类"""
    return INTERVAL_CONSONANCE[abs(midi_a - midi_b) % 12]

def get_interval_semitones(midi_a: int, midi_b: int) -> int:
    """两音之间的绝对半音数 (不取模)"""
    return abs(midi_a - midi_b)
```

---

## 5. 三和弦与七和弦

### 5.1 概念

**三和弦 (triad)**: 三个音按三度叠置。由根音+三音+五音组成。midi 偏移表达式 (`root` 为根音的 midi 编号):

| 和弦类型 | semi 偏移 | MIDI 公式 | 示例 (C 为根) |
|----------|-----------|-----------|----------------|
| 大三和弦 | 0,4,7 | `[root, root+4, root+7]` | C E G |
| 小三和弦 | 0,3,7 | `[root, root+3, root+7]` | C Eb G |
| 减三和弦 | 0,3,6 | `[root, root+3, root+6]` | C Eb Gb |
| 增三和弦 | 0,4,8 | `[root, root+4, root+8]` | C E G# |

**七和弦 (seventh chord)**: 三和弦 + 七音。MIDI 偏移公式:

| 和弦类型 | semi 偏移 | MIDI 公式 |
|----------|-----------|-----------|
| 大七和弦 (maj7) | 0,4,7,11 | `[root, root+4, root+7, root+11]` |
| 属七和弦 (dom7) | 0,4,7,10 | `[root, root+4, root+7, root+10]` |
| 小七和弦 (m7) | 0,3,7,10 | `[root, root+3, root+7, root+10]` |
| 半减七和弦 (m7b5) | 0,3,6,10 | `[root, root+3, root+6, root+10]` |
| 减七和弦 (dim7) | 0,3,6,9 | `[root, root+3, root+6, root+9]` |

### 5.2 Alda 写法

```alda
# 三和弦 (用斜线分隔, 所有音同时开始)
c/e/g           # C 大三和弦
c-/e-/g-        # C 小三和弦
c/e-/g-         # C 减三和弦
c/e/g+          # C 增三和弦

# 七和弦
c/e/g/b         # C 大七和弦 (Cmaj7)
c/e/g/b-        # C 属七和弦 (C7)
c-/e-/g-/b-     # C 小七和弦 (Cm7)

# 跨八度和弦
c/g/>c/e        # C 大三和弦, 跨两个八度

# 和弦内容不同时长 (最短音决定下一事件的时间)
c1~1/>c/<e4 f g f e1
```

### 5.3 代码计算: 从 score JSON 检测和弦

和弦在 JSON 中表现为多个 NoteEvent 具有相同 (或非常接近) 的 `offset`。

```python
from collections import defaultdict

CHORD_PATTERNS = {
    # 三和弦: 偏移模式 (相对于根音的 midi 增量)
    ("major", 3): (0, 4, 7),       # 大三和弦
    ("minor", 3): (0, 3, 7),       # 小三和弦
    ("dim", 3):   (0, 3, 6),       # 减三和弦
    ("aug", 3):   (0, 4, 8),       # 增三和弦
    # 七和弦
    ("maj7", 4):  (0, 4, 7, 11),   # 大七
    ("dom7", 4):  (0, 4, 7, 10),   # 属七
    ("m7", 4):    (0, 3, 7, 10),   # 小七
    ("m7b5", 4):  (0, 3, 6, 10),   # 半减七
    ("dim7", 4):  (0, 3, 6, 9),    # 减七
}

def group_chords(events: list[dict], offset_tol_ms: float = 5.0) -> list[list[dict]]:
    """把 offset 相近的 note 分组, 每组视为一个和弦"""
    sorted_events = sorted(events, key=lambda e: e["offset"])
    groups = []
    current_group = []
    current_offset = None

    for e in sorted_events:
        if current_offset is None or abs(e["offset"] - current_offset) < offset_tol_ms:
            current_group.append(e)
            if current_offset is None:
                current_offset = e["offset"]
        else:
            if len(current_group) >= 2:
                groups.append(current_group)
            current_group = [e]
            current_offset = e["offset"]

    if len(current_group) >= 2:
        groups.append(current_group)
    return groups

def identify_chord(notes: list[dict]) -> list[tuple[str, int]]:
    """
    给定一组同时发音的 note, 识别和弦类型。
    返回 [(和弦类型名称, 根音 midi_note), ...]
    一个音组可能匹配多种和弦 (如 C6 和 Am7 包含相同音)。
    """
    if len(notes) < 3:
        return []

    midi_set = sorted(set(n["midi-note"] for n in notes))
    results = []

    for root in midi_set:
        deltas = tuple(sorted([(n - root) % 12 for n in midi_set]))
        for (name, size), pattern in CHORD_PATTERNS.items():
            pattern_deltas = tuple(sorted(p % 12 for p in pattern))
            if size <= len(midi_set) and all(p in deltas for p in pattern_deltas):
                results.append((name, root))

    return results

def detect_chord_non_chord(events: list[dict], offset_tol_ms: float = 5.0) -> dict:
    """从 score events 分类: 和弦音 vs 非和弦音"""
    chords = group_chords(events, offset_tol_ms)
    chord_notes = set()
    for group in chords:
        for note in group:
            chord_notes.add(id(note))  # 或用 (offset, midi-note) 唯一标识

    chord_count = sum(1 for g in chords for _ in g)
    melodic_count = len(events) - chord_count
    return {
        "chord_simultaneous_groups": len(chords),
        "chord_note_count": chord_count,
        "melodic_note_count": melodic_count,
    }
```

---

## 6. 常见和弦进行与和弦功能

### 6.1 概念

**和弦进行 (chord progression)**: 和弦序列, 是和声运动的骨架。

以下全以**大调**为例。设主音 tonic 的 MIDI 编号为 `T`, 则音阶各音级上的三和弦 root MIDI 编号为:

| 级数 | 罗马数字 | 和弦性质 | root 偏移 | 和弦 midi (半音差) |
|------|----------|----------|-----------|---------------------|
| 1 | I | 大三和弦 | T+0 | `[T, T+4, T+7]` |
| 2 | ii | 小三和弦 | T+2 | `[T+2, T+5, T+9]` |
| 3 | iii | 小三和弦 | T+4 | `[T+4, T+7, T+11]` |
| 4 | IV | 大三和弦 | T+5 | `[T+5, T+9, T+12]` |
| 5 | V | 大三和弦 | T+7 | `[T+7, T+11, T+14]` |
| 6 | vi | 小三和弦 | T+9 | `[T+9, T+12, T+16]` |
| 7 | vii° | 减三和弦 | T+11 | `[T+11, T+14, T+17]` |

**三种最经典和弦进行** (以 root MIDI 序列表达):

| 进行 | root 序列 (相对 T) | 说明 |
|------|---------------------|------|
| I-IV-V-I | `T, T+5, T+7, T` | 最基础,建立调性 |
| ii-V-I | `T+2, T+7, T` | 爵士核心进行 |
| I-V-vi-IV | `T, T+7, T+9, T+5` | 流行乐经典 (如 Let It Be) |

**和弦功能** (简化):

| 功能 | 包含和弦 | 听觉作用 |
|------|----------|----------|
| 主功能 (Tonic) | I, vi, iii | 稳定,"回家"感 |
| 下属功能 (Subdominant) | IV, ii | 离开主,但不紧张 |
| 属功能 (Dominant) | V, vii° | 强烈倾向回到主 |

典型功能序: Tonic → Subdominant → Dominant → Tonic

### 6.2 Alda 写法

```alda
# I-IV-V-I 在 C 大调 (T=60, 八度 4 的 C)
(key-sig '(c major))
o4
# I: C
c4/e/g
# IV: F
< f4/a/>c
# V: G
< g4/b/>d
# I: C
c4/e/g

# 或用调号简写 (D 大调):
(key-sig "f+ c+")
o4
d4/f+/a        # I: D
g4/b/>d        # IV: G
a4/>c+/e       # V: A
d4/f+/a        # I: D
```

### 6.3 代码: 从 score JSON 检测和弦进行

```python
# 大调各音级的和弦 root (root 偏移 + 和弦性质)
MAJOR_KEY_DEGREES = {
    1:  {"root_offset": 0,  "type": "major", "name": "I"},
    2:  {"root_offset": 2,  "type": "minor", "name": "ii"},
    3:  {"root_offset": 4,  "type": "minor", "name": "iii"},
    4:  {"root_offset": 5,  "type": "major", "name": "IV"},
    5:  {"root_offset": 7,  "type": "major", "name": "V"},
    6:  {"root_offset": 9,  "type": "minor", "name": "vi"},
    7:  {"root_offset": 11, "type": "dim",   "name": "vii°"},
}

def detect_progression(
    chord_groups: list[list[dict]],
    tonic_midi: int,
    progression_pattern: list[int]  # e.g. [1,4,5,1] for I-IV-V-I
) -> list[tuple[int, str]]:
    """
    检测和弦进行。
    返回 [(和弦起始 offset, 匹配的级数名), ...]
    """
    result = []
    for group in chord_groups:
        if len(group) < 3:
            continue
        midis = sorted(set(n["midi-note"] for n in group))
        for root in midis:
            root_offset_mod = (root - tonic_midi) % 12
            for degree, info in MAJOR_KEY_DEGREES.items():
                if root_offset_mod == info["root_offset"]:
                    result.append((group[0]["offset"], info["name"]))
                    break
    return result

# 已知经典进行的 root 偏移序列:
PROGRESSIONS = {
    "I-IV-V-I": [0, 5, 7, 0],
    "ii-V-I":   [2, 7, 0],
    "I-V-vi-IV":[0, 7, 9, 5],
}
```

---

## 7. 节拍、拍号与时值体系

### 7.1 概念

- **BPM (Beats Per Minute)**: 每分钟拍数, Alda 默认 120
- **拍长**: `beat_ms = 60000 / BPM`
- **拍号 (time signature)**: 如 4/4 表示每小节 4 拍, 以四分音符为一拍
- **时值**: 以拍为单位。四分音符 = 1 拍, 二分音符 = 2 拍, 全音符 = 4 拍, 八分音符 = 0.5 拍。

Alda 默认以四分音符为一拍, 时值用数字表示: `4` = 四分音符 = 1 拍, `2` = 二分音符 = 2 拍, `1` = 全音符 = 4 拍。

### 7.2 Alda 写法

```alda
# 默认 tempo = 120 BPM; 可使用属性修改:
(tempo! 100)       # 全局 100 BPM
(tempo 80)         # 当前声部 80 BPM

# 时值 (数字越大时值越短, 4 是四分音符)
c1                 # 全音符 (4 拍, 默认 tempo 时 2000ms)
c2                 # 二分音符 (2 拍, 1000ms)
c4                 # 四分音符 (1 拍, 500ms)
c8                 # 八分音符 (0.5 拍, 250ms)
c16                # 十六分音符

# 附点: 增加原时值的一半
c2.                # 附点二分 = 3 拍
c4..               # 双附点四分 = 1.75 拍

# 连音线: 合并时值
c4~4               # 两个四分音符连在一起 = 2 拍

# 非标准时值 (Alda 特色)
c6                 # 六分音符 = 1/6 小节
c2.4               # 任意小数时值

# 毫秒/秒 时值 (与标准时值可混用)
c350ms             # 350 毫秒
d2s                # 2 秒
e2s~200ms          # 2 秒 + 200 毫秒

# Cram 表达式: 将多个音压缩到指定时值内
{c d e f g}2       # 5 个音压缩到二分音符内 (五连音效果)
```

### 7.3 代码: 时值网格对齐检查

```python
def beat_duration_ms(bpm: float = 120.0) -> float:
    """一拍多少毫秒"""
    return 60000.0 / bpm

def grid_unit_ms(beat_ms: float, grid_division: int = 4) -> float:
    """
    网格单位 (如十六分音符网格 = beat_ms / 4)。
    grid_division: 每拍等分数 (4 = 十六分音符精度)
    """
    return beat_ms / grid_division

def check_grid_alignment(
    events: list[dict],
    bpm: float = 120.0,
    grid_division: int = 4,
    tolerance_ms: float = 2.0
) -> dict:
    """
    检查 note offset 和 duration 是否对齐到节拍网格。
    返回不对齐的数量和比例。
    """
    grid = grid_unit_ms(beat_duration_ms(bpm), grid_division)
    misaligned_offsets = 0
    misaligned_durations = 0

    for e in events:
        if abs(e["offset"] % grid) > tolerance_ms and \
           abs(e["offset"] % grid - grid) > tolerance_ms:
            misaligned_offsets += 1
        if abs(e["duration"] % grid) > tolerance_ms and \
           abs(e["duration"] % grid - grid) > tolerance_ms:
            misaligned_durations += 1

    total = len(events)
    return {
        "grid_unit_ms": grid,
        "misaligned_offset_ratio": misaligned_offsets / total if total else 0,
        "misaligned_duration_ratio": misaligned_durations / total if total else 0,
    }

def duration_to_beats(duration_ms: float, bpm: float = 120.0) -> float:
    """duration (ms) → 拍数"""
    return duration_ms / beat_duration_ms(bpm)
```

---

## 8. 乐句与简单曲式

### 8.1 概念

- **乐句 (phrase)**: 类似语言的"句子", 通常 2-8 小节, 由休止或长音分隔
- **AABA**: 经典 32 小节曲式 (各段 8 小节)
- **Verse-Chorus**: 主歌-副歌交替, 现代流行乐核心结构

### 8.2 Alda 写法

利用 `%name` / `@name` 标记分段:

```alda
piano:
  (tempo! 100)

  %intro
  c4 e4 g4 c2      # 4 小节引子

  %verse
  (key-sig '(c major))
  c8 d e f g4. a8 g2

  %chorus
  c4/e/g > c2 <
  a4/>c/e > c2 <

  %verse
  @verse            # 复用 verse 段的音乐

  %chorus
  @chorus

  %outro
  @intro            # 引子 = 尾奏
  c1
```

### 8.3 代码: 按 offset 分段检测

```python
def segment_by_rest(
    events: list[dict],
    rest_threshold_ms: float = 500.0,
    min_phrase_notes: int = 4
) -> list[list[dict]]:
    """
    通过检测长休止 (> rest_threshold_ms) 来分段。
    注意: 休止在 JSON 中不产生 NoteEvent, 只能通过相邻 note 的 offset gap 推断。
    """
    sorted_events = sorted(events, key=lambda e: e["offset"])
    segments = []
    current_segment = []

    for i, e in enumerate(sorted_events):
        if i == 0:
            current_segment.append(e)
            continue

        prev_end = sorted_events[i-1]["offset"] + sorted_events[i-1]["duration"]
        gap = e["offset"] - prev_end

        if gap > rest_threshold_ms and len(current_segment) >= min_phrase_notes:
            segments.append(current_segment)
            current_segment = []

        current_segment.append(e)

    if len(current_segment) >= min_phrase_notes:
        segments.append(current_segment)

    return segments

def detect_form(
    segments: list[list[dict]],
    pattern: list[str]  # e.g. ["A", "A", "B", "A"]
) -> bool:
    """简单模板匹配: 检查是否有 n 个段, 用于曲式识别 (需要更复杂的旋律相似度匹配才能精确)"""
    return len(segments) >= len(pattern)

def analyze_phrase_lengths(events: list[dict]) -> dict:
    """分析乐句长度分布"""
    segments = segment_by_rest(events)
    lengths = [len(s) for s in segments]
    durations_ms = [
        (s[-1]["offset"] + s[-1]["duration"] - s[0]["offset"])
        for s in segments if s
    ]
    return {
        "phrase_count": len(segments),
        "avg_phrase_notes": sum(lengths) / len(lengths) if lengths else 0,
        "avg_phrase_duration_ms": sum(durations_ms) / len(durations_ms) if durations_ms else 0,
    }
```

---

## 9. 多声部写作基础

### 9.1 概念

多声部写作将音乐组织为若干条同时进行的旋律线:
- **旋律 (Melody)**: 最突出的声部, 通常在高音区
- **和声伴奏 (Harmonic accompaniment)**: 填充和声, 在中间音域
- **低音 (Bass)**: 和声根基, 在最低音域, 通常与和弦根音相关

**声部交叉 (Voice crossing)**: 高声部的音低于低声部 — 通常在传统写法中应避免。

**常见乐器 MIDI 音域** (实际可发音范围):

| 乐器 | MIDI 范围 | 最低音 | 最高音 |
|------|----------|--------|--------|
| 钢琴 Piano | 21-108 | A0 | C8 |
| 小提琴 Violin | 55-100 | G3 | E7 |
| 中提琴 Viola | 48-93 | C3 | A6 |
| 大提琴 Cello | 36-84 | C2 | C6 |
| 低音提琴 Double Bass | 28-55 | E1 | G3 |
| 长笛 Flute | 60-96 | C4 | C7 |
| 双簧管 Oboe | 58-91 | Bb3 | A6 |
| 单簧管 Clarinet (Bb) | 50-94 | D3 | C7 |
| 大管 Bassoon | 34-78 | Bb1 | E5 |
| 小号 Trumpet | 55-82 | G3 | C6 |
| 长号 Trombone | 34-72 | B1 | E5 |
| 圆号 French Horn | 34-77 | B1 | F5 |
| 大号 Tuba | 29-55 | B0 | G3 |
| 吉他 Guitar | 40-84 | E2 | E6 |
| 贝斯吉他 Bass Guitar | 28-55 | E1 | G3 |
| 人声 Soprano | 60-81 | C4 | A5 |
| 人声 Alto | 53-74 | F3 | D5 |
| 人声 Tenor | 48-72 | C3 | C5 |
| 人声 Bass | 40-64 | E2 | E4 |

### 9.2 Alda 写法

```alda
# 声部 (Voice): 同一乐器可写多个独立声部
piano:
  (tempo! 90)
  V1: c4 e g > c2.      # 高音旋律
  V2: e g > c < g2.     # 中层和声
  V0: c2 < g2 c1         # V0 切回单声部, 等所有声部结束后开始

# 多乐器 (每个乐器是独立声部)
violin "violin-one":
  o4 e4 f g a b > c d e2

violin "violin-two":
  o4 c4 d e f g a b > c2

viola:
  o3 a2 > c4 c d2 c

cello:
  o3 f2 c4 f < b-2 > f

# 乐器组
violin/viola: c d e f g     # 两者同时演奏相同内容

# 命名组
violin-one/violin-two "violins":
  e f g a b > c d e2
```

以下是错误写法；命名组不能通过点号再选择组外乐器，需使用原别名或单独声明乐器：

```text
violins.cello: < c1
```

### 9.3 代码: 声部交叉检测与音域检查

```python
# 乐器音域表 (MIDI 编号闭区间)
INSTRUMENT_RANGES = {
    "piano":           (21, 108),
    "violin":          (55, 100),
    "viola":           (48, 93),
    "cello":           (36, 84),
    "double-bass":     (28, 55),
    "flute":           (60, 96),
    "oboe":            (58, 91),
    "clarinet":        (50, 94),
    "bassoon":         (34, 78),
    "trumpet":         (55, 82),
    "trombone":        (34, 72),
    "french-horn":     (34, 77),
    "tuba":            (29, 55),
    "guitar":          (40, 84),
    "bass-guitar":     (28, 55),
}

def part_events_by_part(events: list[dict]) -> dict[str, list[dict]]:
    """按 part 分组 events"""
    groups = defaultdict(list)
    for e in events:
        groups[e["part"]].append(e)
    return dict(groups)

def voice_crossing_check(
    events_a: list[dict],
    events_b: list[dict],
    time_tol_ms: float = 100.0
) -> list[dict]:
    """
    检测两个声部是否交叉。
    在重叠时间段内, 如果 A 的最低音 < B 的最高音 (设 A 为高声部),
    则存在交叉。
    返回交叉事件列表。
    """
    crossings = []
    for a in events_a:
        a_end = a["offset"] + a["duration"]
        for b in events_b:
            b_end = b["offset"] + b["duration"]
            overlap = min(a_end, b_end) - max(a["offset"], b["offset"])
            if overlap > 0:
                # A 应该高于 B (midi-note 更大表示音更高), 否则交叉
                if a["midi-note"] < b["midi-note"]:
                    crossings.append({
                        "offset": max(a["offset"], b["offset"]),
                        "voice_a_note": a["midi-note"],
                        "voice_b_note": b["midi-note"],
                    })
    return crossings

def range_check(events: list[dict], instrument: str) -> dict:
    """检查音符是否在乐器可演奏音域内"""
    if instrument not in INSTRUMENT_RANGES:
        return {"error": f"Unknown instrument: {instrument}"}

    lo, hi = INSTRUMENT_RANGES[instrument]
    in_range = 0
    out_of_range = 0
    out_notes = []

    for e in events:
        midi = e["midi-note"]
        if lo <= midi <= hi:
            in_range += 1
        else:
            out_of_range += 1
            out_notes.append(midi)

    total = len(events)
    return {
        "instrument": instrument,
        "range": f"MIDI {lo}-{hi}",
        "in_range_ratio": in_range / total if total else 1.0,
        "out_of_range_count": out_of_range,
        "out_of_range_notes": sorted(set(out_notes)),
    }

def part_pitch_range(events: list[dict]) -> dict:
    """计算某声部的音域统计"""
    if not events:
        return {"min_midi": None, "max_midi": None, "span_semitones": 0}
    midis = [e["midi-note"] for e in events]
    return {
        "min_midi": min(midis),
        "max_midi": max(midis),
        "span_semitones": max(midis) - min(midis),
    }
```

---

## 10. 从 score JSON 到 LLM 可读摘要的度量函数清单

以下所有参数来自 `alda parse -o data` JSON 中每个 note 的 `midi-note`, `offset`, `duration` 字段。

### 10.1 调性与音高度量

```python
def metric_tonality(events: list[dict], tonic_midi: int, mode: str = "major") -> float:
    """调内音比例 (0~1)。越高越符合调性。"""
    midis = [e["midi-note"] for e in events]
    return in_scale_ratio(midis, tonic_midi, mode)

def metric_pitch_range(events: list[dict]) -> dict:
    """音域跨度: min/max/span (半音数)。太小=平淡, 太大=可能超出乐器能力。"""
    midis = [e["midi-note"] for e in events]
    if not midis:
        return {"min": 0, "max": 0, "span": 0}
    return {"min": min(midis), "max": max(midis), "span": max(midis) - min(midis)}

def metric_pitch_entropy(events: list[dict]) -> float:
    """音高多样性 (Shannon entropy over pitch classes)。越高越多样。"""
    from collections import Counter
    import math
    counter = Counter(e["midi-note"] % 12 for e in events)
    total = len(events)
    entropy = sum(
        -(c / total) * math.log2(c / total)
        for c in counter.values()
    )
    # 归一化到 0~1 (最大 entropy = log2(12) 约 3.585)
    return entropy / math.log2(12) if total > 0 else 0.0

def metric_unique_pitches(events: list[dict]) -> float:
    """唯一音高数 / 总音符数。太低 = 重复过多。"""
    unique = len(set(e["midi-note"] for e in events))
    return unique / len(events) if events else 0.0
```

### 10.2 节奏与时值度量

```python
def metric_note_density(events: list[dict]) -> float:
    """音符密度: 音符数 / 总时长 (秒)。太低=稀疏, 太高=拥挤。"""
    if not events:
        return 0.0
    sorted_events = sorted(events, key=lambda e: e["offset"])
    total_span = (
        sorted_events[-1]["offset"] + sorted_events[-1]["duration"]
        - sorted_events[0]["offset"]
    )
    return len(events) / (total_span / 1000.0) if total_span > 0 else 0.0

def metric_duration_diversity(events: list[dict]) -> float:
    """时值多样性 (duration 的 entropy)。"""
    from collections import Counter
    import math
    # 对 duration 取 log 分桶, 因为时值是指数分布的 (二分/四分/八分...)
    buckets = [round(math.log2(max(e["duration"], 1)), 1) for e in events]
    counter = Counter(buckets)
    total = len(buckets)
    entropy = sum(
        -(c / total) * math.log2(c / total)
        for c in counter.values()
    )
    return entropy

def metric_syncopation_approx(events: list[dict], bpm: float = 120.0) -> float:
    """
    切分音近似度量: 检查音符 onset 是否落在强拍上。
    4/4 假设: offset % beat_ms == 0 是强拍, 其他是弱拍/切分。
    返回弱拍 onset 的比例。
    """
    beat_ms = 60000.0 / bpm
    tol = beat_ms * 0.05  # 5% 容忍
    weak_onsets = 0
    for e in events:
        offset_beats = e["offset"] / beat_ms
        if abs(offset_beats - round(offset_beats)) > tol:
            weak_onsets += 1
    return weak_onsets / len(events) if events else 0.0

def metric_rest_ratio(events: list[dict]) -> float:
    """休止比例: 总休止时长 / 总时长。通过 gap 估算。"""
    sorted_events = sorted(events, key=lambda e: e["offset"])
    if len(sorted_events) < 2:
        return 0.0

    total_span = (sorted_events[-1]["offset"] + sorted_events[-1]["duration"]
                  - sorted_events[0]["offset"])
    total_gap = 0.0
    for i in range(1, len(sorted_events)):
        prev_end = sorted_events[i-1]["offset"] + sorted_events[i-1]["duration"]
        gap = sorted_events[i]["offset"] - prev_end
        if gap > 0:
            total_gap += gap

    return total_gap / total_span if total_span > 0 else 0.0
```

### 10.3 和声/协和度量

```python
def metric_consonance_ratio(events: list[dict], offset_tol_ms: float = 5.0) -> dict:
    """
    协和度分析。
    同时发声的音对 (同一和弦组内) → 音程分类统计。
    """
    chords = group_chords(events, offset_tol_ms)
    perfect = imperfect = dissonant = 0

    for group in chords:
        midis = [n["midi-note"] for n in group]
        for i in range(len(midis)):
            for j in range(i+1, len(midis)):
                c = classify_interval(midis[i], midis[j])
                if c == Consonance.PERFECT_CONSONANCE:
                    perfect += 1
                elif c == Consonance.IMPERFECT_CONSONANCE:
                    imperfect += 1
                else:
                    dissonant += 1

    total = perfect + imperfect + dissonant
    return {
        "perfect_consonance_ratio": perfect / total if total else 0,
        "imperfect_consonance_ratio": imperfect / total if total else 0,
        "dissonance_ratio": dissonant / total if total else 0,
    }

def metric_chord_type_distribution(events: list[dict], offset_tol_ms: float = 5.0) -> dict:
    """和弦类型分布统计"""
    from collections import Counter
    chords = group_chords(events, offset_tol_ms)
    type_counter = Counter()
    for group in chords:
        results = identify_chord(group)
        for name, _ in results:
            type_counter[name] += 1
    return dict(type_counter)

def metric_chord_vs_melody_ratio(events: list[dict], offset_tol_ms: float = 5.0) -> float:
    """和弦音占比: 同时发音的音数 / 总音符数"""
    info = detect_chord_non_chord(events, offset_tol_ms)
    total = info["chord_note_count"] + info["melodic_note_count"]
    return info["chord_note_count"] / total if total > 0 else 0.0

def metric_parallel_octaves_fifths(events: list[dict], offset_tol_ms: float = 5.0) -> int:
    """
    检测连续平行八度/五度 (传统对位法中的禁忌)。
    两个声部间, 连续两个和弦都是纯八度或纯五度。
    简化版: 检查相邻和弦组中是否有两音对两音的纯八度/纯五度重复。
    (这是一个信号, 不是绝对的"错误")
    """
    chords = group_chords(events, offset_tol_ms)
    parallel_count = 0
    for i in range(len(chords) - 1):
        midis_a = sorted(set(n["midi-note"] for n in chords[i]))
        midis_b = sorted(set(n["midi-note"] for n in chords[i+1]))
        for a1 in midis_a:
            for a2 in midis_a:
                if a1 >= a2:
                    continue
                for b1 in midis_b:
                    for b2 in midis_b:
                        if b1 >= b2:
                            continue
                        int_a = abs(a1 - a2) % 12
                        int_b = abs(b1 - b2) % 12
                        if int_a == int_b and int_a in (0, 7, 5):
                            parallel_count += 1
    return parallel_count
```

### 10.4 声部/编制度量

```python
def metric_voice_independence(events: list[dict], offset_tol_ms: float = 5.0) -> int:
    """声部交叉计数 (全 score 级别)"""
    parts = part_events_by_part(events)
    part_list = list(parts.values())
    total_crossings = 0
    for i in range(len(part_list)):
        for j in range(i+1, len(part_list)):
            total_crossings += len(voice_crossing_check(part_list[i], part_list[j]))
    return total_crossings

def metric_instrument_range_compliance(events: list[dict], instrument: str) -> dict:
    """乐器音域合规度"""
    return range_check(events, instrument)

def metric_part_count(events: list[dict]) -> int:
    """声部数 (不同 part 的数量)"""
    return len(set(e["part"] for e in events))
```

### 10.5 结构与动态度量

```python
def metric_phrase_count(events: list[dict]) -> int:
    """估算乐句数"""
    return len(segment_by_rest(events))

def metric_dynamic_range(events: list[dict]) -> dict:
    """音量动态范围"""
    volumes = [e.get("volume", 0) for e in events]
    if not volumes:
        return {"min": 0, "max": 0, "range": 0}
    return {"min_volume": min(volumes), "max_volume": max(volumes),
            "range": max(volumes) - min(volumes)}

def metric_repetition_score(events: list[dict], window: int = 4) -> float:
    """
    重复度: 连续音程序列的重复模式。
    简化版: 统计相邻 window 个音的音程序列完全相同的比例。
    0=无重复, 1=完全相同 (越高越有 motif 感, 但太高可能机械化)
    """
    if len(events) < window * 2:
        return 0.0

    intervals = []
    for i in range(1, len(events)):
        intervals.append(events[i]["midi-note"] - events[i-1]["midi-note"])

    pattern_count = 0
    total_windows = len(intervals) - window + 1
    for i in range(total_windows - window):
        if intervals[i:i+window] == intervals[i+window:i+2*window]:
            pattern_count += 1

    return pattern_count / total_windows if total_windows > 0 else 0.0
```

### 10.6 聚合摘要函数 (一次性生成 LLM 可读报告)

```python
def generate_score_summary(events: list[dict], tonic_midi: int, mode: str = "major") -> dict:
    """
    生成 score 的完整可计算度量摘要。
    所有参数均来自 note 的 midi-note / offset / duration。
    """
    return {
        # 音高
        "tonality_in_scale_ratio": metric_tonality(events, tonic_midi, mode),
        "pitch_range": metric_pitch_range(events),
        "pitch_entropy": metric_pitch_entropy(events),
        "unique_pitch_ratio": metric_unique_pitches(events),
        # 节奏
        "note_density_nps": metric_note_density(events),  # notes per second
        "duration_entropy": metric_duration_diversity(events),
        "syncopation_approx": metric_syncopation_approx(events),
        "rest_ratio": metric_rest_ratio(events),
        # 和声
        "consonance": metric_consonance_ratio(events),
        "chord_type_distribution": metric_chord_type_distribution(events),
        "chord_vs_melody_ratio": metric_chord_vs_melody_ratio(events),
        "parallel_octaves_fifths": metric_parallel_octaves_fifths(events),
        # 声部
        "voice_crossings": metric_voice_independence(events),
        "part_count": metric_part_count(events),
        # 结构
        "phrase_count": metric_phrase_count(events),
        "dynamic_range": metric_dynamic_range(events),
        "repetition_score": metric_repetition_score(events),
        # 基础统计
        "total_notes": len(events),
        "total_duration_seconds": (
            (max(e["offset"] + e["duration"] for e in events) -
             min(e["offset"] for e in events)) / 1000.0
        ) if events else 0.0,
    }
```

### 10.7 度量函数速查表

| 度量 | 函数 | 输入字段 | 含义 | 期望范围 |
|------|------|----------|------|----------|
| 调内音比例 | `metric_tonality` | midi-note | 符合调性的程度 | 0.7-1.0 |
| 音域跨度 | `metric_pitch_range` | midi-note | 音乐的高低范围 | 12-48 半音 |
| 音高熵 | `metric_pitch_entropy` | midi-note | 音高使用的多样性 | 0.5-0.9 |
| 唯一音高比 | `metric_unique_pitches` | midi-note | 是否过度重复 | 0.3-0.9 |
| 音符密度 | `metric_note_density` | offset | 音符密集程度 | 1-10 nps |
| 时值多样性 | `metric_duration_diversity` | duration | 节奏丰富度 | 越高越好 |
| 切分音度 | `metric_syncopation_approx` | offset | 强拍以外 onset 比例 | 0.2-0.5 |
| 休止比例 | `metric_rest_ratio` | offset, duration | 呼吸空间 | 0.05-0.3 |
| 协和度 | `metric_consonance_ratio` | midi-note, offset | 和声悦耳程度 | 取决于风格 |
| 和弦类型分布 | `metric_chord_type_distribution` | midi-note, offset | 和声复杂度 | — |
| 和弦音占比 | `metric_chord_vs_melody_ratio` | midi-note, offset | 织体厚度 | 0-1 |
| 平行八/五度 | `metric_parallel_octaves_fifths` | midi-note, offset | 对位禁忌信号 | 越小越好 |
| 声部交叉 | `metric_voice_independence` | midi-note, offset, part | 声部独立性 | 0 |
| 音域合规 | `metric_instrument_range_compliance` | midi-note, part | 是否可演奏 | 1.0 |
| 乐句数 | `metric_phrase_count` | offset, duration | 结构层次 | > 1 |
| 动态范围 | `metric_dynamic_range` | volume | 力度变化 | > 30 |
| 重复度 | `metric_repetition_score` | midi-note | motif 使用程度 | 0.1-0.5 |

---

## 附录: Alda `parse -o data` JSON 示例

```json
{
  "events": [
    {
      "type": "note",
      "part": "part001",
      "midi-channel": 0,
      "midi-note": 60,
      "offset": 0.0,
      "duration": 500.0,
      "audible-duration": 450.0,
      "volume": 54.0,
      "track-volume": 0.7874015748031497,
      "panning": 0.5
    }
  ]
}
```

- `midi-note`: 音高, 0-127 整数 (60 = C4 = middle C)
- `offset`: 音符起始时间, 单位 ms
- `duration`: 音符总时长, 单位 ms (已含附点、连音等处理)
- `audible-duration`: 实际发声时长 = duration * quantization (默认 0.9), 用于连奏/断奏效果
- `volume`: 力度 0-100 (对应 MIDI velocity)
- `panning`: 声像 0-1 (0=左, 0.5=中, 1=右)
