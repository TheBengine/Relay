"""Reproduce Relay's fixed-shape Silero artifact; no downloads or live audio.

Development only: onnx==1.21.0, onnxslim==0.1.98, onnxruntime==1.24.4,
numpy==2.4.4. The client embeds the generated ONNX; no Python runtime is shipped.
"""
from pathlib import Path
import hashlib
import json
import numpy as np
import onnx
import onnxruntime as ort
import onnxslim

root = Path(__file__).resolve().parent
source = root / "silero_vad_16k_op15.onnx"
assert hashlib.sha256(source.read_bytes()).hexdigest() == "7ed98ddbad84ccac4cd0aeb3099049280713df825c610a8ed34543318f1b2c49"
model = onnx.load(source)
for value, shape in zip(model.graph.input, [[1, 576], [2, 1, 128], []], strict=True):
    del value.type.tensor_type.shape.dim[:]
    for size in shape:
        value.type.tensor_type.shape.dim.add().dim_value = size
model = onnxslim.slim(model)
onnx.checker.check_model(model)
target = root / "silero_vad_16k_fixed.onnx"
onnx.save(model, target)

# Compare both probability and recurrent state over continuous random audio.
sessions = [ort.InferenceSession(str(path), providers=["CPUExecutionProvider"]) for path in [source, target]]
states = [np.zeros((2, 1, 128), np.float32) for _ in sessions]
context = np.zeros((1, 64), np.float32)
rng = np.random.default_rng(412)
maximum_error = 0.0
for _ in range(500):
    samples = np.concatenate([context, rng.normal(0, 0.1, (1, 512)).astype(np.float32)], axis=1)
    outputs = []
    for i, session in enumerate(sessions):
        outputs.append(session.run(None, {"input": samples, "state": states[i], "sr": np.array(16000, np.int64)}))
        states[i] = outputs[-1][1]
    for original, fixed in zip(*outputs, strict=True):
        maximum_error = max(maximum_error, float(np.max(np.abs(original - fixed))))
        np.testing.assert_allclose(original, fixed, atol=1e-5, rtol=1e-5)
    context = samples[:, -64:]

# Independent original-model values used by the Rust runtime parity test.
state = np.zeros((2, 1, 128), np.float32)
context = np.zeros((1, 64), np.float32)
probabilities = []
for block in range(8):
    samples = (((np.arange(512) + block * 512) % 97 - 48) / 512).astype(np.float32)[None, :]
    probability, state = sessions[0].run(None, {"input": np.concatenate([context, samples], axis=1), "state": state, "sr": np.array(16000, np.int64)})
    probabilities.append(float(probability.item()))
    context = samples[:, -64:]
(root / "reference.json").write_text(json.dumps({"engine": f"onnxruntime {ort.__version__}", "sample_rate": 16000, "signal": "((block * 512 + i) % 97 - 48) / 512", "probabilities": probabilities}, indent=2) + "\n", encoding="utf-8")
provenance = json.loads((root / "provenance.json").read_text())
provenance.update(fixed_model_sha256=hashlib.sha256(target.read_bytes()).hexdigest(), preparation={"onnx": onnx.__version__, "onnxslim": onnxslim.__version__, "onnxruntime": ort.__version__, "numpy": np.__version__, "verified_windows": 500, "max_probability_and_state_error": maximum_error})
(root / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n", encoding="utf-8")
print(json.dumps(provenance["preparation"]))
