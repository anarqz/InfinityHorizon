"""Generate the original Infinity notification chime; no sampled or third-party audio."""
from pathlib import Path
import math
import struct
import wave

RATE = 48000
DURATION = 0.8
samples = []
for i in range(round(RATE * DURATION)):
    time = i / RATE
    value = 0.0
    for onset, frequency, gain in [(0.0, 622.254, 0.7), (0.105, 932.328, 0.54), (0.215, 1244.508, 0.5)]:
        t = time - onset
        if t < 0:
            continue
        envelope = (1 - math.exp(-t / 0.006)) * math.exp(-t / 0.15)
        phase = 2 * math.pi * frequency * t
        bell = math.sin(phase + 0.22 * math.exp(-t / 0.06) * math.sin(2.01 * phase))
        value += gain * envelope * (bell + 0.09 * math.sin(2 * phase))
    value *= min(1, max(0, (DURATION - time) / 0.04))
    samples.append(value)
peak = max(abs(value) for value in samples)
pcm = b''.join(struct.pack('<h', round(value / peak * 0.62 * 32767)) for value in samples)
path = Path(__file__).resolve().parent.parent / 'assets' / 'infinity-notification.wav'
path.parent.mkdir(parents=True, exist_ok=True)
with wave.open(str(path), 'wb') as output:
    output.setnchannels(1)
    output.setsampwidth(2)
    output.setframerate(RATE)
    output.writeframes(pcm)
assert len(path.read_bytes()) == 44 + len(pcm)
print(path)
