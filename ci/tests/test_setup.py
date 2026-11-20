#!/usr/bin/env python3
"""Installer preflight tests use fake tools; no host service or cluster is changed."""
import base64
import hashlib
import io
import json
import http.server
import socket
import ssl
import threading
import os
from pathlib import Path
import stat
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)


def archive(path, files):
    with tarfile.open(path, 'w:gz') as output:
        for name, content in files.items():
            raw = content.encode()
            info = tarfile.TarInfo(name)
            info.size = len(raw)
            info.mode = 0o755 if name == 'bin/flmlet' else 0o644
            output.addfile(info, io.BytesIO(raw))


class Installer(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.crypto = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.crypto.cleanup)
        cls.certificate = Path(cls.crypto.name) / 'ca.crt'
        cls.private_key = Path(cls.crypto.name) / 'ca.key'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-keyout', str(cls.private_key), '-out', str(cls.certificate), '-subj', '/CN=localhost', '-addext', 'subjectAltName=IP:127.0.0.1'], check=True, capture_output=True)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.artifacts = self.root / 'artifacts'
        self.artifacts.mkdir()
        self.log = self.root / 'operations.log'
        self.env = os.environ.copy()
        self.env.update(PATH=f'{self.bin}:{os.environ["PATH"]}', INSTALL_TEST_LOG=str(self.log), NO_PROXY='127.0.0.1,localhost')
        self.cluster_objects = self.root / 'cluster-objects.json'
        self.env['INSTALL_TEST_CLUSTER_OBJECTS'] = str(self.cluster_objects)
        self.rendered_flames([('flame-ns-1', 'flame-1', 'flame-client-ca')])
        write(self.bin / 'helm', '''#!/bin/sh
printf 'helm %s\\n' "$*" >> "$INSTALL_TEST_LOG"
case "$*" in
  *'template flame-cluster '*) printf 'nodePort: 30080\\ngateway:\\n  className: flame-envoy\\n  issuerName: flame-client-ca\\n  sessionPort: %s\\n  cachePort: 31443\\n' "${INSTALL_TEST_SESSION_PORT:-30443}" ;;
  *'template flame-ui '*) [ "${INSTALL_TEST_BAD_RENDER:-0}" != 1 ] || exit 1 ;;
esac
if [ "${INSTALL_TEST_MULTI:-0}" = 1 ]; then
  case "$*" in *'template flame-cluster '*) printf 'gateway:\\n  className: second-envoy\\n  issuerName: second-ca\\n' ;; esac
fi
''')
        write(self.bin / 'kubectl', '''#!/bin/sh
printf 'kubectl %s\\n' "$*" >> "$INSTALL_TEST_LOG"
case "$*" in
  *'config view --minify'*) printf '%s\\n' "${INSTALL_TEST_KUBE_SERVER-https://192.168.3.10:6443}" ;;
  *'create --dry-run=client --validate=false'*) cat "$INSTALL_TEST_CLUSTER_OBJECTS" ;;
  *'get configmap '*)
    [ "${INSTALL_TEST_MISSING_CA:-0}" != 1 ] || exit 1
    printf '%s\\n' '{"apiVersion":"v1","kind":"ConfigMap","data":{"ca.crt":"PUBLIC-CA","unrelated":"MUST-NOT-COPY"}}' ;;
  *'apply -f '*)
    for candidate in "$@"; do
      case "$candidate" in */namespaces.json) cp "$candidate" "$INSTALL_TEST_LOG.namespaces.json" ;;
        */public-trust.json) cp "$candidate" "$INSTALL_TEST_LOG.public-trust.json" ;; esac
    done ;;
  *'get crd certificates.cert-manager.io'*) [ "${INSTALL_TEST_MISSING_DEP:-0}" != 1 ] ;;
  *'get crd backendtlspolicies.gateway.networking.k8s.io'*) [ "${INSTALL_TEST_MISSING_BACKEND_TLS:-0}" != 1 ] ;;
esac
''')
        write(self.bin / 'id', '#!/bin/sh\nprintf "0\\n"\n')
        write(self.bin / 'uname', '#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo x86_64;; esac\n')
        write(self.bin / 'systemctl', '#!/bin/sh\nprintf "systemctl %s\\n" "$*" >> "$INSTALL_TEST_LOG"\n')
        charts = {f'charts/{name}/Chart.yaml': f'name: {name}\nversion: 0.1.0\n' for name in ['flame-operator', 'flame-cluster', 'flame-ui']}
        charts['charts/flame-operator/crds/flameclusters.yaml'] = 'kind: CustomResourceDefinition\n'
        archive(self.artifacts / 'flame-cluster-charts.tar.gz', charts)
        archive(self.artifacts / 'flmlet-linux-amd64.tar.gz', {'bin/flmlet': '#!/bin/sh\nprintf "flmlet-check %s\\n" "$*" >> "$INSTALL_TEST_LOG"\nexit "${INSTALL_TEST_CHECK_EXIT:-0}"\n', 'LICENSE': 'Apache-2.0\n'})
        self.checksums()
        self.prefix = self.root / 'runtime'
        write(self.prefix / 'bin/flame-executor-manager', '#!/bin/sh\nexit 0\n')
        self.systemd = self.root / 'systemd'
        self.token = self.root / 'join.token'
        self.enroll_status = 200
        self.pending_once = False
        self.disconnect = False
        self.enroll_requests = []
        self.rejections = []
        self.created_workers = set()
        self.durable_credential = 'durable-worker-secret-never-print'
        fixture = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                payload = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                fixture.enroll_requests.append(payload)
                if self.path != '/api/v1/namespaces/operator-system/services/http:flame-operator:8081/proxy/join' or self.headers.get('Authorization') != f'Bearer {fixture.credential}' or payload.get('token') != fixture.token.read_text():
                    fixture.rejections.append((self.path, self.headers.get('Authorization') == f'Bearer {fixture.credential}', payload.get('token') == fixture.token.read_text()))
                    self.send_response(403); self.end_headers(); return
                if fixture.enroll_status != 200:
                    self.send_response(fixture.enroll_status); self.end_headers(); return
                fixture.created_workers.add(payload['worker'])
                if fixture.disconnect:
                    self.connection.shutdown(socket.SHUT_RDWR); self.connection.close(); return
                if fixture.pending_once:
                    fixture.pending_once = False
                    self.send_response(202); self.end_headers(); return
                response = {'namespace': payload['namespace'], 'worker': payload['worker'], 'kubeconfig': json.dumps(fixture.kubeconfig(fixture.durable_credential))}
                encoded = json.dumps(response).encode()
                self.send_response(200); self.send_header('Content-Type', 'application/json'); self.send_header('Content-Length', str(len(encoded))); self.end_headers(); self.wfile.write(encoded)
        self.server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(self.certificate, self.private_key)
        self.server.socket = tls.wrap_socket(self.server.socket, server_side=True)
        self.api = f'https://127.0.0.1:{self.server.server_port}'
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.join_token()

    def checksums(self):
        (self.artifacts / 'SHA256SUMS').write_text(''.join(f'{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n' for path in sorted(self.artifacts.glob('*.tar.gz'))))

    def kubeconfig(self, credential):
        return {'apiVersion': 'v1', 'kind': 'Config', 'current-context': 'worker',
                'clusters': [{'name': 'flame', 'cluster': {'server': self.api, 'certificate-authority-data': base64.b64encode(self.certificate.read_bytes()).decode()}}],
                'users': [{'name': 'worker', 'user': {'token': credential}}],
                'contexts': [{'name': 'worker', 'context': {'cluster': 'flame', 'user': 'worker', 'namespace': 'flame-system'}}]}

    def join_token(self, mutate=None):
        self.credential = 'bootstrap-secret-never-print'
        config = self.kubeconfig(self.credential)
        if mutate:
            mutate(config)
        envelope = {'namespace': 'flame-system', 'cluster': 'flame', 'serviceNamespace': 'operator-system', 'service': 'flame-operator', 'kubeconfig': json.dumps(config)}
        self.token.write_bytes(base64.b64encode(json.dumps(envelope).encode()))

    def rendered_flames(self, flames, list_output=False):
        items = []
        for namespace, name, issuer in flames:
            items.extend([
                {'apiVersion': 'v1', 'kind': 'Namespace', 'metadata': {'name': namespace,
                    'annotations': {'meta.helm.sh/release-name': 'flame-cluster', 'meta.helm.sh/release-namespace': 'flame-system', 'helm.sh/resource-policy': 'keep'},
                    'labels': {'app.kubernetes.io/managed-by': 'Helm'}}},
                {'apiVersion': 'flame.xflops.io/v1alpha1', 'kind': 'FlameCluster',
                    'metadata': {'name': name, 'namespace': namespace}, 'spec': {'gateway': {'issuerName': issuer}}},
            ])
        # kubectl create -o json prints each YAML document as a separate JSON object.
        output = json.dumps({'apiVersion': 'v1', 'kind': 'List', 'items': items}) if list_output else '\n'.join(json.dumps(item, indent=2) for item in items)
        self.cluster_objects.write_text(output)

    def run_setup(self, *args, trace=False, **updates):
        environment = self.env | updates
        result = subprocess.run(['sh', *(['-x'] if trace else []), str(ROOT / 'setup.sh'), '--artifacts-dir', str(self.artifacts), *args], capture_output=True, text=True, env=environment)
        self.assertNotIn(self.credential, result.stdout + result.stderr)
        self.assertNotIn(self.durable_credential, result.stdout + result.stderr)
        self.assertNotIn(self.token.read_text(), result.stdout + result.stderr)
        return result

    def operations(self):
        return self.log.read_text() if self.log.exists() else ''

    def no_install(self):
        operations = self.operations()
        self.assertNotIn('helm upgrade', operations)
        self.assertNotIn('kubectl apply', operations)
        self.assertNotIn('systemctl ', operations)
        self.assertFalse((self.prefix / 'bin/flmlet').exists())
        self.assertFalse(self.systemd.exists())

    def worker_args(self):
        return ('--join', '--token-file', str(self.token), '--worker', 'worker-01', '--address', '192.168.3.8', '--shims', 'host', '--prefix', str(self.prefix), '--systemd-dir', str(self.systemd))

    def assert_ui_exposure(self, result, address, kube_host):
        operations = self.operations().splitlines()
        ui_invocations = [line for line in operations if 'template flame-ui ' in line or '--install flame-ui ' in line]
        self.assertEqual(len(ui_invocations), 2)
        for invocation in ui_invocations:
            self.assertIn(f'service.externalIPs[0]={address}', invocation)
        cluster_invocations = [line for line in operations if 'template flame-cluster ' in line or '--install flame-cluster ' in line]
        self.assertEqual(len(cluster_invocations), 2)
        for invocation in cluster_invocations:
            encoded = invocation.split('--set-json cluster.security.allowedHosts=', 1)[1]
            hosts, _ = json.JSONDecoder().raw_decode(encoded)
            self.assertTrue({'localhost', '127.0.0.1', '::1', address, kube_host}.issubset(hosts))
        self.assertIn(f'Console: http://{address}/', result.stdout)
        self.assertNotIn('port-forward', result.stdout)

    def test_help_never_reads_cluster_or_writes_host(self):
        result = self.run_setup('--help')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--token-file', result.stdout)
        self.assertNotIn('--ui-address', result.stdout)
        self.assertEqual(self.operations(), '')

    def test_cluster_defaults_to_existing_kubeconfig_and_nodeports(self):
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        operations = self.operations().splitlines()
        self.assertTrue(any('config view --minify' in line for line in operations))
        cluster_invocations = [line for line in operations if 'template flame-cluster ' in line or '--install flame-cluster ' in line]
        self.assertEqual(len(cluster_invocations), 2)
        for invocation in cluster_invocations:
            self.assertIn('flames[0].gateway.hostname=192.168.3.10', invocation)
            self.assertIn('flames[0].endpointHost=192.168.3.10', invocation)
            self.assertIn('flames[0].kubernetesEndpoint=https://192.168.3.10:6443', invocation)
        self.assertNotIn('flames[0].name=', self.operations())
        self.assertNotIn('flames[0].version=', self.operations())
        self.assert_ui_exposure(result, '192.168.3.10', '192.168.3.10')

    def test_explicit_kubeconfig_is_used_for_endpoint_discovery(self):
        kubeconfig = self.root / 'selected-kubeconfig'
        kubeconfig.write_text('fixture')
        result = self.run_setup('--kubeconfig', str(kubeconfig), INSTALL_TEST_KUBE_SERVER='https://192.168.3.20:7443')
        self.assertEqual(result.returncode, 0, result.stderr)
        operations = self.operations()
        self.assertIn(f'kubectl --kubeconfig {kubeconfig} config view --minify', operations)
        self.assertIn('flames[0].gateway.hostname=192.168.3.20', operations)
        self.assertIn('flames[0].endpointHost=192.168.3.20', operations)
        self.assertIn('flames[0].kubernetesEndpoint=https://192.168.3.20:7443', operations)
        self.assert_ui_exposure(result, '192.168.3.20', '192.168.3.20')

    def test_non_lan_kubeconfig_ui_address_keeps_explicit_worker_frontend(self):
        result = self.run_setup('--endpoint', '192.168.3.10:30080', INSTALL_TEST_KUBE_SERVER='https://100.117.27.41:6443')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_ui_exposure(result, '100.117.27.41', '100.117.27.41')
        self.assertIn('flames[0].endpoint=192.168.3.10:30080', self.operations())
        for invocation in self.operations().splitlines():
            if 'template flame-cluster ' in invocation or '--install flame-cluster ' in invocation:
                self.assertIn('flames[0].endpointHost=192.168.3.10', invocation)
                self.assertNotIn('flames[0].endpointHost=100.117.27.41', invocation)

    def test_invalid_explicit_worker_frontend_changes_nothing(self):
        for endpoint in ['192.168.3.10', 'https://192.168.3.10:30080', 'example.test:30080',
                         '100.117.27.41:30080', '127.0.0.1:30080', '192.168.3.10:29999',
                         '192.168.3.10:32767', '192.168.3.10:+30080', '192.168.3.10:']:
            with self.subTest(endpoint=endpoint):
                result = self.run_setup('--endpoint', endpoint)
                self.assertNotEqual(result.returncode, 0)
                self.no_install()


    def test_kind_installer_uses_reachable_selected_kubeconfig(self):
        kubeconfig = self.root / 'kind-installer-kubeconfig'
        kubeconfig.write_text('fixture')
        result = self.run_setup('--kubeconfig', str(kubeconfig), '--endpoint', '192.168.3.10:30080', '--kubernetes-endpoint', 'https://192.168.3.10:6443', INSTALL_TEST_KUBE_SERVER='https://192.168.3.10:6443')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_ui_exposure(result, '192.168.3.10', '192.168.3.10')
        self.assertIn(f'kubectl --kubeconfig {kubeconfig} config view --minify', self.operations())
        cluster_invocations = [line for line in self.operations().splitlines() if 'template flame-cluster ' in line or '--install flame-cluster ' in line]
        for invocation in cluster_invocations:
            self.assertIn('flames[0].endpoint=192.168.3.10:30080', invocation)
            self.assertIn('flames[0].kubernetesEndpoint=https://192.168.3.10:6443', invocation)

    def test_non_unicast_kubeconfig_ui_host_changes_nothing(self):
        for address in ['127.0.0.1', '0.0.0.0', '224.0.0.1', '255.255.255.255']:
            with self.subTest(address=address):
                result = self.run_setup('--endpoint', '192.168.3.10:30080', INSTALL_TEST_KUBE_SERVER=f'https://{address}:6443')
                self.assertNotEqual(result.returncode, 0)
                self.no_install()

    def test_endpoint_overrides_are_preserved(self):
        result = self.run_setup('--hostname', 'gateway.example.test', '--endpoint', '192.168.3.30:32080', '--kubernetes-endpoint', 'https://enroll.example.test:6443')
        self.assertEqual(result.returncode, 0, result.stderr)
        for invocation in self.operations().splitlines():
            if 'template flame-cluster ' in invocation or '--install flame-cluster ' in invocation:
                self.assertIn('flames[0].gateway.hostname=gateway.example.test', invocation)
                self.assertIn('flames[0].endpoint=192.168.3.30:32080', invocation)
                self.assertIn('flames[0].kubernetesEndpoint=https://enroll.example.test:6443', invocation)

    def test_optional_values_preserve_chart_nodeport_selection(self):
        values = self.root / 'cluster-values.yaml'
        values.write_text('flames:\n  - nodePort: 32080\n')
        result = self.run_setup('--values', str(values))
        self.assertEqual(result.returncode, 0, result.stderr)
        for invocation in self.operations().splitlines():
            if 'template flame-cluster ' in invocation or '--install flame-cluster ' in invocation:
                self.assertIn(f'--values {values}', invocation)
                self.assertIn('flames[0].endpointHost=192.168.3.10', invocation)
                self.assertNotIn('--set-string flames[0].endpoint=', invocation)
                self.assertNotIn('--set-string flames[0].nodePort=', invocation)

    def test_values_file_preserves_custom_first_flame_name_and_version(self):
        values = self.root / 'custom-flame.yaml'
        values.write_text('flames:\n  - name: training\n    version: v0.8.0\n')
        self.rendered_flames([('training-ns-1', 'training-1', 'flame-client-ca')])
        result = self.run_setup('--values', str(values))
        self.assertEqual(result.returncode, 0, result.stderr)
        operations = self.operations()
        self.assertNotIn('flames[0].name=', operations)
        self.assertNotIn('flames[0].version=', operations)
        self.assertIn('-n training-ns-1 wait flamecluster/training-1 ', operations)
        self.assertIn('Flame training-ns-1/training-1 is ready.', result.stdout)
        for invocation in operations.splitlines():
            if 'template flame-cluster ' in invocation or '--install flame-cluster ' in invocation:
                self.assertIn(f'--values {values}', invocation)

    def test_explicit_first_flame_name_and_version_override_values(self):
        values = self.root / 'custom-flame.yaml'
        values.write_text('flames:\n  - name: training\n    version: v0.8.0\n')
        self.rendered_flames([('inference-ns-1', 'inference-1', 'flame-client-ca')])
        result = self.run_setup('--values', str(values), '--cluster', 'inference', '--flame-version', 'v0.9.0')
        self.assertEqual(result.returncode, 0, result.stderr)
        for invocation in self.operations().splitlines():
            if 'template flame-cluster ' in invocation or '--install flame-cluster ' in invocation:
                self.assertIn('flames[0].name=inference', invocation)
                self.assertIn('flames[0].version=v0.9.0', invocation)
        self.assertIn('Flame inference-ns-1/inference-1 is ready.', result.stdout)

    def test_invalid_kubeconfig_server_changes_nothing(self):
        for server in ['', 'not-a-url', 'http://node.example.test:6443', 'https://user:secret@node.example.test:6443', 'https://node.example.test:6443/path', 'https://node.example.test:6443?token=secret']:
            with self.subTest(server=server):
                result = self.run_setup(INSTALL_TEST_KUBE_SERVER=server)
                self.assertNotEqual(result.returncode, 0)
                self.no_install()

    def test_missing_sre_dependency_changes_nothing(self):
        result = self.run_setup('--hostname', 'flame.example.test', INSTALL_TEST_MISSING_DEP='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('SRE prerequisite is missing', result.stderr)
        self.no_install()

    def test_missing_backend_tls_policy_dependency_changes_nothing(self):
        result = self.run_setup('--hostname', 'flame.example.test', INSTALL_TEST_MISSING_BACKEND_TLS='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('backendtlspolicies.gateway.networking.k8s.io', result.stderr)
        self.no_install()

    def test_checksum_failure_prevents_all_cluster_installations(self):
        with (self.artifacts / 'flame-cluster-charts.tar.gz').open('ab') as output:
            output.write(b'tampered')
        result = self.run_setup('--hostname', 'flame.example.test')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checksum mismatch', result.stderr)
        self.no_install()

    def test_every_chart_renders_before_any_cluster_write(self):
        result = self.run_setup('--hostname', 'flame.example.test', INSTALL_TEST_BAD_RENDER='1')
        self.assertNotEqual(result.returncode, 0)
        self.no_install()

    def test_success_installs_only_flame_charts_with_requested_image_tag(self):
        result = self.run_setup('--hostname', 'flame.example.test', '--image-tag', 'sha-test-amd64', '--kubernetes-endpoint', 'https://api.example.test:6443')
        self.assertEqual(result.returncode, 0, result.stderr)
        operations = self.operations().splitlines()
        rendered = [index for index, line in enumerate(operations) if 'helm template' in line]
        writes = [index for index, line in enumerate(operations) if 'kubectl apply' in line or 'helm upgrade' in line]
        self.assertEqual(len(rendered), 3)
        self.assertLess(max(rendered), min(writes))
        upgrades = [line for line in operations if 'helm upgrade' in line]
        self.assertEqual(len(upgrades), 3)
        self.assertTrue(all(any(f'--install {chart} ' in line for chart in ['flame-operator', 'flame-cluster', 'flame-ui']) for line in upgrades))
        self.assertNotIn('--install flame-worker', '\n'.join(upgrades))
        self.assertIn('image.tag=sha-test-amd64', '\n'.join(upgrades))
        self.assertIn('flames[0].kubernetesEndpoint=https://api.example.test:6443', '\n'.join(upgrades))
        self.assertNotIn('--install flame-cluster-api ', '\n'.join(upgrades))
        self.assertNotIn('charts/flame-cluster-api', self.operations())
        cluster = next(line for line in upgrades if '--install flame-cluster ' in line)
        self.assertIn('cluster.image.tag=sha-test-amd64', cluster)
        cluster_render = next(line for line in operations if 'template flame-cluster ' in line)
        self.assertIn('cluster.image.tag=sha-test-amd64', cluster_render)
        self.assertNotIn('cluster.flame.endpoint=', self.operations())
        ui = next(line for line in upgrades if '--install flame-ui ' in line)
        self.assertIn('consoleUpstream=cluster.flame-system.svc:3000', ui)

    def test_explicit_console_endpoint_configures_merged_cluster_api(self):
        result = self.run_setup('--console-endpoint', 'https://external-flm.example.test:8080')
        self.assertEqual(result.returncode, 0, result.stderr)
        for invocation in self.operations().splitlines():
            if 'template flame-cluster ' in invocation or '--install flame-cluster ' in invocation:
                self.assertIn('cluster.flame.endpoint=https://external-flm.example.test:8080', invocation)


    def test_namespaces_and_public_trust_precede_managed_flame_installation(self):
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        operations = self.operations().splitlines()
        namespace_apply = next(i for i, line in enumerate(operations) if 'apply -f ' in line and 'namespaces.json' in line)
        trust_apply = next(i for i, line in enumerate(operations) if 'apply -f ' in line and 'public-trust.json' in line)
        install = next(i for i, line in enumerate(operations) if '--install flame-cluster ' in line)
        dryrun = next(i for i, line in enumerate(operations) if 'create --dry-run=client --validate=false' in line)
        self.assertLess(dryrun, namespace_apply)
        self.assertLess(namespace_apply, trust_apply)
        self.assertLess(trust_apply, install)
        self.assertIn('kubectl -n flame-ns-1 wait flamecluster/flame-1 ', self.operations())
        namespaces = json.loads(Path(str(self.log) + '.namespaces.json').read_text())['items']
        self.assertEqual(namespaces[0]['metadata']['name'], 'flame-ns-1')
        self.assertEqual(namespaces[0]['metadata']['annotations']['meta.helm.sh/release-name'], 'flame-cluster')
        self.assertEqual(namespaces[0]['metadata']['annotations']['helm.sh/resource-policy'], 'keep')
        trust = json.loads(Path(str(self.log) + '.public-trust.json').read_text())['items']
        self.assertEqual(trust, [{'apiVersion': 'v1', 'kind': 'ConfigMap', 'metadata': {'name': 'flame-client-ca', 'namespace': 'flame-ns-1'}, 'data': {'ca.crt': 'PUBLIC-CA'}}])
        self.assertNotIn('get secret', self.operations())

    def test_multiple_flames_check_every_dependency_and_wait_in_their_namespaces(self):
        self.rendered_flames([('flame-ns-1', 'flame-1', 'flame-client-ca'), ('custom-ns', 'training-2', 'second-ca')])
        result = self.run_setup(INSTALL_TEST_MULTI='1')
        self.assertEqual(result.returncode, 0, result.stderr)
        operations = self.operations()
        for dependency in ['gatewayclass flame-envoy', 'gatewayclass second-envoy', 'clusterissuer flame-client-ca', 'clusterissuer second-ca']:
            self.assertIn(f'get {dependency}', operations)
        self.assertEqual(operations.count('get gatewayclass flame-envoy'), 1)
        self.assertIn('-n flame-ns-1 wait flamecluster/flame-1 ', operations)
        self.assertIn('-n custom-ns wait flamecluster/training-2 ', operations)
        trust = json.loads(Path(str(self.log) + '.public-trust.json').read_text())['items']
        self.assertEqual({item['metadata']['namespace'] for item in trust}, {'flame-ns-1', 'custom-ns'})
        self.assertTrue(all(set(item['data']) == {'ca.crt'} for item in trust))
        self.assertNotIn('get secret', operations)

    def test_namespace_preflight_also_accepts_a_kubernetes_list(self):
        self.rendered_flames([('flame-ns-1', 'flame-1', 'flame-client-ca')], list_output=True)
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('-n flame-ns-1 wait flamecluster/flame-1 ', self.operations())

    def test_namespace_preflight_reports_malformed_json_without_echoing_contents(self):
        with self.cluster_objects.open('a') as output:
            output.write('\n{"kind":"Namespace","data":"MUST-NOT-PRINT-CONTENTS"')
        result = self.run_setup()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('cannot parse rendered Kubernetes JSON', result.stderr)
        self.assertNotIn('MUST-NOT-PRINT-CONTENTS', result.stdout + result.stderr)
        self.assertNotIn('helm upgrade', self.operations())
        self.assertFalse(Path(str(self.log) + '.namespaces.json').exists())
        self.assertFalse(Path(str(self.log) + '.public-trust.json').exists())

    def test_namespace_preflight_reports_missing_managed_objects(self):
        self.rendered_flames([], list_output=True)
        result = self.run_setup()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('no managed Flame namespaces were rendered', result.stderr)
        self.assertNotIn('helm upgrade', self.operations())
        self.assertFalse(Path(str(self.log) + '.namespaces.json').exists())

    def test_missing_public_trust_changes_nothing(self):
        result = self.run_setup(INSTALL_TEST_MISSING_CA='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('public trust ConfigMap is missing', result.stderr)
        self.no_install()

    def test_invalid_join_token_never_installs(self):
        self.token.write_text('not-base64')
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.no_install()

    def test_inline_operator_join_token_installs_without_logging_credentials(self):
        result = self.run_setup('--join', self.token.read_text(), '--worker', 'worker-01', '--address', '192.168.3.8', '--shims', 'host', '--prefix', str(self.prefix), '--systemd-dir', str(self.systemd), trace=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        config = self.prefix / 'conf/flmlet/kubeconfig'
        self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
        self.assertEqual(json.loads(config.read_text())['users'][0]['user']['token'], self.durable_credential)
        self.assertIn('systemctl restart flmlet.service', self.operations())
        self.assertNotIn(self.token.read_text(), self.operations())

    def test_invalid_inline_join_token_is_never_printed_or_installed(self):
        invalid = 'secret-invalid-join-token'
        result = self.run_setup('--join', invalid, '--prefix', str(self.prefix), '--systemd-dir', str(self.systemd))
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(invalid, result.stdout + result.stderr)
        self.no_install()

    def test_inline_and_file_join_forms_are_mutually_exclusive(self):
        result = self.run_setup('--join', self.token.read_text(), '--token-file', str(self.token), '--prefix', str(self.prefix), '--systemd-dir', str(self.systemd))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('not both', result.stderr)
        self.no_install()

    def test_missing_join_token_fails_without_host_changes(self):
        result = self.run_setup('--join', '--prefix', str(self.prefix), '--systemd-dir', str(self.systemd))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('requires a token', result.stderr)
        self.no_install()

    def test_join_cluster_mismatch_never_installs(self):
        result = self.run_setup(*self.worker_args(), '--cluster', 'different-cluster')
        self.assertNotEqual(result.returncode, 0)
        self.no_install()

    def test_join_tokens_cannot_supply_exec_credentials(self):
        self.join_token(lambda config: config['users'][0]['user'].update(exec={'command': '/bin/false'}))
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.no_install()

    def test_join_tokens_cannot_disable_tls_verification(self):
        self.join_token(lambda config: config['clusters'][0]['cluster'].update({'insecure-skip-tls-verify': True}))
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.no_install()

    def test_worker_checksum_failure_never_installs(self):
        with (self.artifacts / 'flmlet-linux-amd64.tar.gz').open('ab') as output:
            output.write(b'tampered')
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checksum mismatch', result.stderr)
        self.no_install()

    def test_worker_preflight_failure_retains_only_enrollment_state(self):
        result = self.run_setup(*self.worker_args(), INSTALL_TEST_CHECK_EXIT='1')
        self.assertNotEqual(result.returncode, 0)
        self.no_install()

    def test_join_without_preinstalled_runtime_delegates_bootstrap_to_flmlet(self):
        (self.prefix / 'bin/flame-executor-manager').unlink()
        (self.prefix / 'bin').rmdir()
        self.prefix.rmdir()
        result = self.run_setup(*self.worker_args())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.prefix / 'bin/flmlet').is_file())
        invocations = [line for line in self.operations().splitlines() if line.startswith('flmlet-check ')]
        self.assertEqual(len(invocations), 2)
        self.assertIn('--check', invocations[0])
        self.assertIn('--configure', invocations[1])
        self.assertIn(f'--prefix {self.prefix}', invocations[1])
        self.assertLess(self.operations().index('--configure'), self.operations().index('systemctl restart'))

    def test_join_installs_flmlet_and_private_kubeconfig(self):
        result = self.run_setup(*self.worker_args())
        self.assertEqual(result.returncode, 0, result.stderr)
        config = self.prefix / 'conf/flmlet/kubeconfig'
        self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
        self.assertEqual(json.loads(config.read_text())['users'][0]['user']['token'], self.durable_credential)
        service = (self.systemd / 'flmlet.service').read_text()
        self.assertNotIn(self.credential, service)
        self.assertIn('--worker "worker-01"', service)
        self.assertIn('systemctl enable flmlet.service', self.operations())
        self.assertIn('systemctl restart flmlet.service', self.operations())
        self.assertNotIn('helm ', self.operations())
        self.assertNotIn('kubectl ', self.operations())

    def test_rejected_fresh_host_join_removes_new_enrollment_directories(self):
        (self.prefix / 'bin/flame-executor-manager').unlink()
        (self.prefix / 'bin').rmdir()
        self.prefix.rmdir()
        self.enroll_status = 403
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('HTTP 403', result.stderr)
        self.assertFalse(self.prefix.exists())
        self.no_install()

    def test_existing_service_for_another_worker_requires_explicit_adoption(self):
        service = '# Managed by Flame Cluster setup.sh\nExecStart=/bin/flmlet --worker "another-worker" --namespace "flame-system"\n'
        write(self.systemd / 'flmlet.service', service)
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('--adopt-existing', result.stderr)
        self.assertEqual((self.systemd / 'flmlet.service').read_text(), service)
        self.assertFalse((self.prefix / 'bin/flmlet').exists())
        self.assertNotIn('systemctl ', self.operations())

    def test_reinstalling_same_worker_restarts_the_running_service(self):
        first = self.run_setup(*self.worker_args())
        self.assertEqual(first.returncode, 0, first.stderr)
        self.log.write_text('')
        second = self.run_setup(*self.worker_args())
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn('systemctl restart flmlet.service', self.operations())

    def test_cluster_name_over_32_characters_changes_nothing(self):
        result = self.run_setup('--hostname', 'flame.example.test', '--cluster', 'a' * 33)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.operations(), '')
        self.no_install()

    def test_expired_bootstrap_token_creates_no_worker_or_local_state(self):
        self.enroll_status = 403
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('HTTP 403', result.stderr)
        self.assertEqual(self.created_workers, set())
        self.assertFalse((self.prefix / 'conf').exists())
        self.no_install()

    def test_rejection_never_deletes_a_preexisting_enrollment_key(self):
        state = self.prefix / 'conf/flmlet'
        state.mkdir(parents=True)
        key = state / 'enrollment-key'
        key.write_text('a' * 64)
        key.chmod(0o600)
        self.enroll_status = 403
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(key.read_text(), 'a' * 64)
        self.no_install()

    def test_pending_enrollment_reuses_key_and_persists_only_durable_credentials(self):
        self.pending_once = True
        result = self.run_setup(*self.worker_args(), '--max-executors', '12', '--manage-object-cache')
        self.assertEqual(result.returncode, 0, result.stderr + repr(self.rejections))
        self.assertEqual(len(self.enroll_requests), 2)
        key = self.prefix / 'conf/flmlet/enrollment-key'
        self.assertEqual(stat.S_IMODE(key.stat().st_mode), 0o600)
        self.assertEqual(self.enroll_requests[0]['enrollmentKey'], key.read_text())
        self.assertEqual(self.enroll_requests[1]['enrollmentKey'], key.read_text())
        self.assertEqual(self.enroll_requests[0]['maxExecutors'], 12)
        self.assertIs(self.enroll_requests[0]['manageObjectCache'], True)
        self.assertEqual(self.enroll_requests[0]['address'], '192.168.3.8')
        self.assertEqual(self.enroll_requests[0]['token'], self.token.read_text())
        for path in self.prefix.rglob('*'):
            if path.is_file():
                self.assertNotIn(self.credential, path.read_text())
                self.assertNotIn(self.token.read_text(), path.read_text())
        log = self.operations()
        self.assertLess(log.index('--configure'), log.index('systemctl restart'))

    def test_uncertain_delivery_retains_key_for_safe_retry(self):
        self.disconnect = True
        result = self.run_setup(*self.worker_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('transport failed', result.stderr)
        key = self.prefix / 'conf/flmlet/enrollment-key'
        self.assertEqual(len(key.read_text()), 64)
        self.assertIn('worker-01', self.created_workers)
        self.no_install()

    def test_minimal_join_detects_hostname_and_uses_lan_instead_of_overlay_route(self):
        site = self.root / 'python-site'
        site.mkdir()
        (site / 'sitecustomize.py').write_text('''import socket
original = socket.socket
class RoutedSocket(original):
    def connect(self, address):
        if self.type == socket.SOCK_DGRAM:
            self.local_route = '192.168.3.42' if address[0] == '192.0.2.1' else '100.88.1.2'
            return None
        return super().connect(address)
    def getsockname(self):
        if self.type == socket.SOCK_DGRAM and hasattr(self, 'local_route'):
            return (self.local_route, 50000)
        return super().getsockname()
socket.socket = RoutedSocket
socket.gethostname = lambda: 'auto-worker'
''')
        result = self.run_setup('--join', self.token.read_text(), '--prefix', str(self.prefix), '--systemd-dir', str(self.systemd), PYTHONPATH=str(site))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.enroll_requests[0]['worker'], 'auto-worker')
        self.assertEqual(self.enroll_requests[0]['address'], '192.168.3.42')
        self.assertEqual(self.enroll_requests[0]['maxExecutors'], 10)
        self.assertIs(self.enroll_requests[0]['manageObjectCache'], False)
        self.assertEqual(self.enroll_requests[0]['shims'], ['cri'])

    def test_host_runtime_is_explicit_opt_in(self):
        result = self.run_setup(*self.worker_args())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.enroll_requests[0]['shims'], ['host'])

    def test_both_prepared_runtimes_can_be_selected(self):
        result = self.run_setup(*self.worker_args(), '--shims', 'cri,host')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.enroll_requests[0]['shims'], ['cri', 'host'])

    def test_duplicate_or_unknown_runtime_selection_is_rejected_before_enrollment(self):
        for shims in ['cri,cri', 'host,host', 'docker', 'cri,', 'unknown,host']:
            with self.subTest(shims=shims):
                result = self.run_setup(*self.worker_args(), '--shims', shims)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('--shims', result.stderr)
                self.assertEqual(self.enroll_requests, [])
                self.assertFalse((self.prefix / 'conf').exists())
                self.no_install()


class Publishing(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.record = self.root / 'request.json'
        self.env = os.environ | {'PATH': f'{self.bin}:{os.environ["PATH"]}', 'GH_TOKEN': 'never-echo-publishing-secret', 'PUBLISH_TEST_REQUEST': str(self.record), 'PUBLISH_TEST_SOURCE': str(ROOT / 'setup.sh')}
        write(self.bin / 'gh', '''#!/usr/bin/env python3
import base64,json,os,sys
path=sys.argv[2]
if '--method' in sys.argv:
    with open(os.environ['PUBLISH_TEST_REQUEST'],'w') as out: out.write(sys.stdin.read())
    print('{}')
elif '/contents/' in path:
    if os.environ.get('PUBLISH_TEST_EXISTS') == '1':
        with open(os.environ['PUBLISH_TEST_SOURCE'],'rb') as source: content=base64.b64encode(source.read()).decode()
        print(json.dumps({'content':content,'sha':'existing-sha'}))
    else:
        print('gh: Not Found (HTTP 404)',file=sys.stderr);sys.exit(1)
else: print(json.dumps({'default_branch':'main'}))
''')

    def publish(self, **updates):
        result = subprocess.run(['sh', '-x', str(ROOT / 'ci/publish-setup.sh')], capture_output=True, text=True, env=self.env | updates)
        self.assertNotIn(self.env['GH_TOKEN'], result.stdout + result.stderr)
        if self.record.exists():
            self.assertNotIn(self.env['GH_TOKEN'], self.record.read_text())
        return result

    def test_publication_updates_only_setup_file_with_default_branch(self):
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(self.record.read_text())
        self.assertEqual(payload['branch'], 'main')
        self.assertEqual(base64.b64decode(payload['content']), (ROOT / 'setup.sh').read_bytes())

    def test_identical_installer_does_not_write_a_commit(self):
        result = self.publish(PUBLISH_TEST_EXISTS='1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.record.exists())
        self.assertIn('already current', result.stdout)


if __name__ == '__main__':
    unittest.main()
