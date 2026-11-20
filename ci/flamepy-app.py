#!/usr/bin/env python3
"""Run real Flame App/cache workloads through the deployed Gateway identity."""

import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import shutil
import socket
import sys
import tempfile
import time
import unittest
import uuid

import flamepy
import flamepy.app as app

OBSERVED = {"applications": [], "cleanup": []}
EXPECTED_WORKER = ""


class FlameAppTest(unittest.TestCase):
    def setUp(self):
        self.project = tempfile.TemporaryDirectory(prefix="flamepy-app-project-")
        self.addCleanup(self.project.cleanup)
        previous = Path.cwd()
        self.addCleanup(os.chdir, previous)
        os.chdir(self.project.name)
        # Package code only. HOME and its inline TLS identity are outside this tree.
        shutil.copy2(__file__, "flamepy-app-case.py")
        self.name = f"flame-ci-app-{uuid.uuid4().hex[:12]}"
        self.assertEqual(sys.version_info[:2], (3, 12))
        self.assertNotEqual(socket.gethostname(), EXPECTED_WORKER)
        context = flamepy.FlameContext()
        self.assertTrue(context.endpoint.startswith("https://"), context.endpoint)
        self.assertTrue(
            context.cache_endpoint.startswith("grpcs-proxy://"), context.cache_endpoint
        )
        self.template = context.app or "flmrun"
        self.assertIsNotNone(
            flamepy.get_application(self.template), "flmrun template is required"
        )
        app.init(self.name, fail_if_exists=True, dependencies=[], python_version="3.12")
        self.addCleanup(self.destroy_app)
        self.assertIsNotNone(flamepy.get_application(self.name))
        OBSERVED["applications"].append({"name": self.name, "template": self.template})

    def destroy_app(self):
        app.destroy()
        deadline = time.monotonic() + 60
        while True:
            application = flamepy.get_application(self.name)
            if (
                application is None
                or application.state == flamepy.ApplicationState.DISABLED
            ):
                OBSERVED["cleanup"].append(
                    {
                        "name": self.name,
                        "state": "absent"
                        if application is None
                        else application.state.name,
                    }
                )
                return
            if time.monotonic() >= deadline:
                self.fail(
                    f"Application {self.name} remains enabled after app.destroy()"
                )
            time.sleep(1)

    def test_cache_round_trip_and_remote_object_arguments(self):
        payload = {
            "marker": self.name,
            "data": bytes(range(256)) * 512,
            "values": [1, "two", {"three": 3}],
        }
        reference = app.put(payload)
        self.assertIsInstance(reference, flamepy.ObjectRef)
        restored = flamepy.get_object(reference)
        self.assertEqual(restored, payload)
        inline_payload = {**payload, "data": bytes(range(256)) * 8}
        OBSERVED["cache"] = {
            "bytes": len(restored["data"]),
            "sha256": hashlib.sha256(restored["data"]).hexdigest(),
            "marker": restored["marker"],
        }

        @app.service(autoscale=True, resreq="cpu=1,mem=256m")
        def echo(index, item, *, label):
            import hashlib
            import socket
            import sys

            return {
                "index": index,
                "marker": item["marker"],
                "values": item["values"],
                "bytes": len(item["data"]),
                "sha256": hashlib.sha256(item["data"]).hexdigest(),
                "label": label,
                "worker": socket.gethostname(),
                "python": list(sys.version_info[:2]),
            }

        # Small inline arguments and large explicit ObjectRefs must preserve exact values.
        # Serializing the large object inline would correctly exceed the SDK's limit.
        calls = [
            echo.remote(
                index, reference if index % 2 else inline_payload, label=f"call-{index}"
            )
            for index in range(4)
        ]
        returned = app.get(calls)
        self.assertEqual(len(returned), 4)
        self.assertEqual({row["index"] for row in returned}, set(range(4)))
        for index, row in enumerate(returned):
            expected = payload if index % 2 else inline_payload
            self.assertEqual(
                row,
                {
                    "index": index,
                    "marker": self.name,
                    "values": payload["values"],
                    "bytes": len(expected["data"]),
                    "sha256": hashlib.sha256(expected["data"]).hexdigest(),
                    "label": f"call-{index}",
                    "worker": EXPECTED_WORKER,
                    "python": [3, 12],
                },
            )
        OBSERVED["remoteCalls"] = returned

    def test_chained_object_futures(self):
        @app.service(autoscale=True, resreq="cpu=1,mem=256m")
        def add(left, right):
            return left + right

        actual = add.remote(add.remote(2, 4), add.remote(3, 5)).get()
        self.assertEqual(actual, 14)
        OBSERVED["chainedResult"] = actual

    def test_stateful_class_handle(self):
        @app.service(autoscale=False, warmup=1, resreq="cpu=1,mem=256m")
        class Counter:
            def __init__(self):
                self.total = 0

            def add(self, amount):
                self.total += amount
                return self.total

            def value(self):
                import socket

                return {"total": self.total, "worker": socket.gethostname()}

        counter = Counter.remote()
        try:
            values = [counter.add(2).get(), counter.add(4).get()]
            actual = counter.value().get()
            self.assertEqual(values, [2, 6])
            self.assertEqual(actual, {"total": 6, "worker": EXPECTED_WORKER})
            OBSERVED["counter"] = {"increments": values, "result": actual}
        finally:
            counter.close()


def main():
    global EXPECTED_WORKER
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--worker", required=True)
    args = parser.parse_args()
    EXPECTED_WORKER = args.worker
    started = time.monotonic()
    result = unittest.TextTestRunner(verbosity=2).run(
        unittest.defaultTestLoader.loadTestsFromTestCase(FlameAppTest)
    )
    report = {
        "coverage": "flamepy.app",
        "sdkVersion": importlib.metadata.version("flamepy"),
        "python": sys.version.split()[0],
        "clientHost": socket.gethostname(),
        "testsRun": result.testsRun,
        "successful": result.wasSuccessful(),
        "durationSeconds": round(time.monotonic() - started, 3),
        "failures": [
            {"test": str(case), "traceback": detail}
            for case, detail in result.failures + result.errors
        ],
        **OBSERVED,
    }
    args.results.parent.mkdir(parents=True, exist_ok=True)
    args.results.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
