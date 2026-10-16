#!/usr/bin/env python3
"""Exercise real telemetry through HTTP with a child-only synthetic NVML library.

Python stdlib and a C compiler are test/build requirements only. No GPU, driver
changes, elevated privileges, or production fixture configuration are needed.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request


NVML = r"""
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
typedef unsigned int U;
typedef void *H;
struct Memory { uint64_t total, free, used; };
struct Utilization { U gpu, memory; };
static int mode(void) {
    int value = 0;
    FILE *file = fopen(getenv("OYASYS_TEST_NVML_MODE"), "r");
    if (file) { if (fscanf(file, "%d", &value) != 1) value = 0; fclose(file); }
    return value;
}
U nvmlInit_v2(void) { return 0; }
U nvmlShutdown(void) { return 0; }
U nvmlDeviceGetCount_v2(U *count) { *count = mode() == 3 ? 0 : 1; return 0; }
U nvmlDeviceGetHandleByIndex_v2(U index, H *handle) { *handle = (H)(uintptr_t)1; return 0; }
U nvmlDeviceGetUUID(H handle, char *buffer, U size) { snprintf(buffer, size, "GPU-SYNTHETIC-HTTP-TEST"); return 0; }
U nvmlDeviceGetName(H handle, char *buffer, U size) {
    snprintf(buffer, size, "%s", mode() == 2 ? "Renamed synthetic P100" : "Synthetic P100"); return 0;
}
U nvmlDeviceGetTemperature(H handle, U type, U *value) { *value = 0; return mode() == 1 ? 15 : 0; }
U nvmlDeviceGetUtilizationRates(H handle, struct Utilization *value) { return 3; }
U nvmlDeviceGetMemoryInfo(H handle, struct Memory *value) { return 4; }
U nvmlDeviceGetPowerUsage(H handle, U *value) { return 2; }
U nvmlDeviceGetClockInfo(H handle, U type, U *value) { *value = 1000; return 0; }
"""


def run(binary):
    compiler = shutil.which("cc")
    if not compiler:
        raise SystemExit("SKIP: a C compiler is needed for the synthetic NVML test library")
    with tempfile.TemporaryDirectory(prefix="oyasys-telemetry-http-") as directory:
        temporary = Path(directory)
        source = temporary / "nvml.c"
        source.write_text(NVML)
        subprocess.run([compiler, "-shared", "-fPIC", "-O2", str(source), "-o",
                        str(temporary / "libnvidia-ml.so.1")], check=True)
        mode = temporary / "mode"
        mode.write_text("0")
        environment = os.environ | {"LD_LIBRARY_PATH": str(temporary),
                                    "OYASYS_TEST_NVML_MODE": str(mode)}
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        origin = f"http://127.0.0.1:{port}"
        child = subprocess.Popen([str(binary), "serve", "--bind", f"127.0.0.1:{port}",
                                  "--origin", origin, "--data-dir", str(temporary / "data")],
                                 env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

        def request(path, method="GET", payload=None):
            headers = {"Connection": "close"}
            if method != "GET":
                headers |= {"Origin": origin, "X-OYAsys-CSRF": request("/state")["csrf_token"]}
            if payload is not None:
                headers["Content-Type"] = "application/json"
            body = None if payload is None else json.dumps(payload).encode()
            with urllib.request.urlopen(urllib.request.Request(origin + "/api/v1" + path,
                                        data=body, method=method, headers=headers), timeout=5) as response:
                return json.load(response)

        def wait_for(predicate):
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                state = request("/state")
                if predicate(state):
                    return state
                time.sleep(0.04)
            raise AssertionError("Expected source availability transition did not arrive")

        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    state = request("/state")
                    break
                except OSError:
                    if time.monotonic() > deadline or child.poll() is not None:
                        raise AssertionError("Native test server failed to start")
                    time.sleep(0.03)
            prefix = "gpu.GPU-SYNTHETIC-HTTP-TEST."
            availability = state["inventory"]["availability"]
            values = state["snapshot"]["values"]
            assert availability["cpu.utilization"]["status"] == "warming_up"
            assert values["cpu.utilization"] is None
            assert values[prefix + "temperature"] == 0
            assert availability[prefix + "temperature"]["status"] == "available"
            for metric, expected, code in [("utilization", "unsupported", 3),
                                           ("memory_used", "permission_denied", 4),
                                           ("memory_total", "permission_denied", 4),
                                           ("power", "invalid", 2)]:
                assert values[prefix + metric] is None
                assert availability[prefix + metric]["status"] == expected
                assert str(code) in availability[prefix + metric]["reason"]
            request("/settings", "PUT", state["settings"] | {"sampling_seconds": 1, "confirm_delete": False})
            mode.write_text("1")
            lost = wait_for(lambda s: s["inventory"]["availability"][prefix + "temperature"]["status"] == "lost")
            assert lost["snapshot"]["values"][prefix + "temperature"] is None
            assert "15" in lost["inventory"]["availability"][prefix + "temperature"]["reason"]
            assert lost["snapshot"]["values"]["memory.total"] > 0
            mode.write_text("2")
            refreshed = request("/refresh", "POST")
            metric = next(m for m in refreshed["metrics"] if m["id"] == prefix + "temperature")
            assert metric["label"].startswith("Renamed synthetic P100")
            assert "Renamed synthetic P100" in metric["source"]
            recovered = wait_for(lambda s: s["inventory"]["availability"][prefix + "temperature"]["status"] == "available")
            assert recovered["snapshot"]["values"][prefix + "temperature"] == 0
            mode.write_text("3")
            absent = request("/refresh", "POST")
            assert absent["availability"][prefix + "temperature"]["status"] == "lost"
            assert any(m["id"] == prefix + "temperature" for m in absent["metrics"])
            print("PASS: HTTP zero/missing, warming, unsupported, permissions, invalid query, "
                  "device loss/recovery, retained identities, refreshed metadata")
        finally:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=10)
            child.stderr.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/oyasys")
    arguments = parser.parse_args()
    run(Path(arguments.binary).resolve())
