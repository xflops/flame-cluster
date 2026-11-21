#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo run --locked -p flame-api --bin crdgen -- flame > "${TMPDIR:-/tmp}/flameclusters.yaml"
diff -u charts/flame-operator/crds/flameclusters.yaml "${TMPDIR:-/tmp}/flameclusters.yaml"
cargo run --locked -p flame-api --bin crdgen -- worker > "${TMPDIR:-/tmp}/flameworkers.yaml"
diff -u charts/flame-operator/crds/flameworkers.yaml "${TMPDIR:-/tmp}/flameworkers.yaml"
helm lint charts/flame-operator
helm lint charts/flame-ui
python3 charts/flame-cluster/tests/test_render.py
helm lint charts/flame-cluster --set 'flames[0].gateway.hostname=flame.example.test'
for script in ci/*.sh; do bash -n "$script"; done

python3 -m py_compile ci/flamepy-app.py
python3 ci/tests/test_setup.py
python3 ci/tests/test_publish_release.py
