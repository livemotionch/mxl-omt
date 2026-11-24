import json, time, urllib.request, sys
sys.path.insert(0, __import__("os").path.dirname(__file__))
from scale import get, put, host_cpu
for i in range(1, 9): put(i, f"MXL-TEST (OMT Out {i})")
time.sleep(30)
a = get("/api/status"); h0 = host_cpu(); t0 = time.time(); cpus = []
while time.time() - t0 < 600:
    time.sleep(5); cpus.append(get("/api/status")["cpu_percent"])
b = get("/api/status"); h1 = host_cpu()
def d(k, side): return sum(y[k] - x[k] for x, y in zip(a[side], b[side]))
print(json.dumps({"seconds": round(time.time() - t0), "grains_written": d("grains_written", "inputs"), "expected": 8 * 50 * round(time.time() - t0),
  "dropped": d("dropped", "inputs"), "repeated": d("repeated", "inputs"), "black": d("black", "inputs"), "resyncs": d("resyncs", "inputs"),
  "audio_resyncs": d("audio_resyncs", "inputs"), "audio_silence": d("audio_silence", "inputs"),
  "out_frames": d("frames_sent", "outputs"), "out_late": d("late", "outputs"), "out_held": d("held", "outputs"), "out_skipped": d("skipped", "outputs"), "out_audio_gaps": d("audio_gaps", "outputs"),
  "proc_cpu_avg": round(sum(cpus) / len(cpus)), "proc_cpu_max": round(max(cpus)),
  "host_cpu_pct": round(100 * (1 - (h1[1] - h0[1]) / (h1[0] - h0[0])), 1),
  "latency_ms": [i["latency_ms"] for i in b["inputs"]], "transit_max_ms": max(i["transit_max_ms"] or 0 for i in b["inputs"])}))
