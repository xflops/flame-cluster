# flame-api

Shared Kubernetes contracts for the Flame controller and `flmlet`:

- `FlameCluster` selects session-manager, ingress, storage, and manual cache configuration.
- `FlameWorker` selects SRE-installed host services and publishes observed service, registration, and cache health.
- Endpoint/configuration validation is shared by the controller and host reconciler.

The API crate also owns CRD generation. Regenerate the two schemas with:

```sh
cargo run -p flame-api --bin crdgen -- flame
cargo run -p flame-api --bin crdgen -- worker
```

The Helm chart installs these generated CRDs. Controller resource rendering and
host systemd reconciliation stay in their respective components.
