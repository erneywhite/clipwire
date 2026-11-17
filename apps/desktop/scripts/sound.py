"""Звуки копирования и вставки: двойной клик из двух коротких нот.

Копирование — вторая нота выше, вставка — ниже и чуть тише, чтобы на слух
их было не спутать. Синтезируются, а не берутся готовыми, чтобы не было
вопросов с лицензией.
Запуск: python apps/desktop/scripts/sound.py  →  apps/desktop/src-tauri/sounds/{copy,paste}.wav
"""
import math
import pathlib
import struct
import wave

SR = 44100


def note(f0, f1, dur, attack, decay, harm=(), delay=0.0):
    out = [0.0] * int(SR * delay)
    ph = 0.0
    for i in range(int(SR * dur)):
        t = i / SR
        f = f0 + (f1 - f0) * min(1, t / dur)
        ph += 2 * math.pi * f / SR
        e = t / attack if t < attack else math.exp(-(t - attack) / decay)
        out.append((math.sin(ph) + sum(a * math.sin(k * ph) for k, a in harm)) * e)
    return out


def mix(*tracks):
    n = max(len(t) for t in tracks)
    return [sum(t[i] if i < len(t) else 0 for t in tracks) for i in range(n)]


CLICK = ((2.7, 0.2),)
SOUNDS = {
    "copy": (
        mix(
            note(1150, 1090, 0.045, 0.001, 0.009, CLICK),
            note(1540, 1470, 0.05, 0.001, 0.01, CLICK, delay=0.048),
        ),
        0.5,
    ),
    "paste": (
        mix(
            note(1320, 1260, 0.045, 0.001, 0.009, CLICK),
            note(990, 940, 0.055, 0.001, 0.012, CLICK, delay=0.048),
        ),
        0.42,
    ),
}
for name, (samples, gain) in SOUNDS.items():
    peak = max(abs(s) for s in samples)
    out = pathlib.Path(__file__).resolve().parents[1] / "src-tauri" / "sounds" / f"{name}.wav"
    with wave.open(str(out), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(b"".join(struct.pack("<h", int(s / peak * gain * 32767)) for s in samples))
    print(out)
