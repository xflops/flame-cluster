"""Render the actual chart with Helm; no Kubernetes cluster or Python dependencies."""
import json
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

CHART = Path(__file__).resolve().parents[1]


def render(values, template=None, check=True, overrides=()):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / 'values.json'
        path.write_text(json.dumps(values))
        args = ['helm', 'template', 'test-release', str(CHART), '--namespace', 'team-a', '-f', str(path)]
        if template:
            args += ['--show-only', f'templates/{template}']
        for override in overrides:
            args += ['--set', override]
        return subprocess.run(args, text=True, capture_output=True, check=check)


class Render(unittest.TestCase):
    def test_rollout_override_preserves_the_full_stored_flames_list(self):
        values = {'flames': [
            {'name': 'training', 'namespace': 'training-ns', 'endpoint': '192.168.3.10:32080', 'nodePort': 32080,
             'gateway': {'hostname': 'training.example.test', 'sessionPort': 32443, 'cachePort': 32444}},
            {'name': 'inference', 'namespace': 'inference-ns', 'endpoint': '192.168.3.11:32180', 'nodePort': 32180,
             'rolloutRevision': 'keep-inference', 'gateway': {'hostname': 'inference.example.test', 'sessionPort': 32543, 'cachePort': 32544}},
        ]}
        before = render(values, 'flames.yaml').stdout
        after = render(values, 'flames.yaml', overrides=['flames[0].rolloutRevision=functional-restart']).stdout
        self.assertEqual(after, before.replace('rolloutRevision: ""', 'rolloutRevision: functional-restart', 1))
        self.assertEqual(re.findall(r'^  name: (.+)$', after, re.M), ['training-1', 'inference-2'])
        self.assertEqual(re.findall(r'^  namespace: (.+)$', after, re.M), ['training-ns', 'inference-ns'])
        for endpoint in ['192.168.3.10:32080', '192.168.3.11:32180']:
            self.assertIn(f'endpoint: {endpoint}', after)
        for port in [32080, 32180, 32443, 32444, 32543, 32544]:
            self.assertRegex(after, rf'(?:nodePort|sessionPort|cachePort): {port}\b')
        self.assertEqual(render(values, 'connections.yaml').stdout,
                         render(values, 'connections.yaml', overrides=['flames[0].rolloutRevision=functional-restart']).stdout)

    def test_multiple_flames_have_matching_connections_and_distinct_ports(self):
        values = {'flames': [
            {'name': 'training', 'endpointHost': '192.168.3.10', 'gateway': {'hostname': '192.168.3.10'}},
            {'name': 'training', 'endpointHost': '192.168.3.11', 'gateway': {'hostname': '192.168.3.11', 'issuerName': 'other-ca'}},
        ]}
        resources = render(values, 'flames.yaml').stdout
        names = re.findall(r'^  name: (.+)$', resources, re.M)
        self.assertEqual(names, ['training-1', 'training-2'])
        self.assertEqual(re.findall(r'^  nodePort: (\d+)$', resources, re.M), ['30080', '30090'])
        self.assertEqual(re.findall(r'^    sessionPort: (\d+)$', resources, re.M), ['30443', '30453'])
        self.assertEqual(re.findall(r'^    cachePort: (\d+)$', resources, re.M), ['31443', '31453'])
        self.assertIn('endpoint: 192.168.3.11:30090', resources)
        config = render(values, 'connections.yaml').stdout.split('  connections.json: |\n', 1)[1]
        connections = json.loads(config)
        self.assertEqual([connection['id'] for connection in connections], names)
        self.assertEqual([connection['displayName'] for connection in connections], ['training-1/training-1', 'training-2/training-2'])
        deployment = render(values, 'deployment.yaml').stdout
        self.assertIn('  name: cluster\n', deployment)
        self.assertIn('FLAME_CONNECTIONS_FILE', deployment)
        self.assertIn('name: "other-ca"', deployment)
        self.assertNotIn('secret:', deployment)
        self.assertIn('serviceAccountName: cluster', deployment)
        self.assertIn('automountServiceAccountToken: true', deployment)
        for connection in connections:
            name = connection['id']
            self.assertEqual(connection['endpoint'], f'https://{name}-session-manager.{connection["namespace"]}.svc:8080')
            self.assertEqual(connection['caFile'], f'/etc/flame/connections/{name}/ca.crt')
            self.assertEqual(connection['name'], name)
            self.assertEqual(connection['joinNamespace'], connection['namespace'])
            self.assertEqual(connection['joinSecret'], f'{name}-join')
            self.assertEqual(connection['clientConfigSecret'], f'{name}-client')
            self.assertNotIn('joinTokenFile', connection)
            self.assertNotIn('joinExpiryFile', connection)
            self.assertIn(f'mountPath: "/etc/flame/connections/{name}"', deployment)
        self.assertIn('  name: cluster\n', render(values, 'service.yaml').stdout)

    def test_namespaces_and_join_permissions_are_isolated(self):
        values = {'flames': [
            {'gateway': {'hostname': '192.168.3.10'}},
            {'namespace': 'custom-flame-ns', 'gateway': {'hostname': '192.168.3.11'}},
        ]}
        resources = render(values, 'flames.yaml').stdout
        self.assertEqual(re.findall(r'^  namespace: (.+)$', resources, re.M), ['flame-1', 'custom-flame-ns'])
        namespaces = render(values, 'namespaces.yaml').stdout
        self.assertEqual(re.findall(r'^  name: (.+)$', namespaces, re.M), ['flame-1', 'custom-flame-ns'])
        self.assertEqual(namespaces.count('helm.sh/resource-policy: keep'), 2)
        self.assertEqual(namespaces.count('meta.helm.sh/release-name: "test-release"'), 2)
        self.assertEqual(namespaces.count('meta.helm.sh/release-namespace: "team-a"'), 2)
        rbac = render(values, 'rbac.yaml').stdout
        self.assertEqual(rbac.count('kind: Role\n'), 4) # Role resources and RoleBinding roleRef.
        self.assertEqual(rbac.count('    verbs: [get]'), 4)
        self.assertEqual(rbac.count('    resources: [secrets]'), 2)
        self.assertEqual(rbac.count('    resources: [configmaps]'), 2)
        self.assertEqual(rbac.count('  - apiGroups: [flame.xflops.io]'), 2)
        self.assertEqual(rbac.count('    resources: [flameworkers]'), 2)
        self.assertEqual(rbac.count('    verbs: [get, list]'), 2)
        self.assertCountEqual(re.findall(r'^  namespace: (.+)$', rbac, re.M), ['team-a', 'flame-1', 'flame-1', 'custom-flame-ns', 'custom-flame-ns'])
        self.assertEqual(rbac.count('    namespace: team-a'), 2)
        for name in ['flame-1', 'flame-2']:
            self.assertIn(f'resourceNames: ["{name}-join", "{name}-client"]', rbac)
            self.assertIn(f'resources: [configmaps]\n    resourceNames: ["{name}-config"]\n    verbs: [get]', rbac)
        for forbidden in ['ClusterRole', 'verbs: [list]', 'create', 'update', 'delete', 'watch', '*', '-control-plane-tls']:
            self.assertNotIn(forbidden, rbac)
        values['flames'][0]['namespace'] = 'custom-flame-ns'
        failure = render(values, check=False)
        self.assertNotEqual(failure.returncode, 0)
        self.assertIn('distinct namespace', failure.stderr)

    def test_explicit_existing_namespace_survives_new_default_naming(self):
        values = {'flames': [
            {'namespace': 'flame-ns-1', 'gateway': {'hostname': '192.168.3.10'}},
            {},
        ]}
        resources = render(values, 'flames.yaml').stdout
        self.assertEqual(re.findall(r'^  namespace: (.+)$', resources, re.M), ['flame-ns-1', 'flame-2'])
        config = render(values, 'connections.yaml').stdout.split('  connections.json: |\n', 1)[1]
        connections = json.loads(config)
        self.assertEqual([item['namespace'] for item in connections], ['flame-ns-1', 'flame-2'])
        self.assertEqual(connections[0]['endpoint'], 'https://flame-1-session-manager.flame-ns-1.svc:8080')

    def test_partial_flames_inherit_only_shared_endpoint_discovery_defaults(self):
        values = {'flames': [
            {'endpointHost': '192.168.3.10', 'kubernetesEndpoint': 'https://192.168.3.10:6443', 'gateway': {'hostname': '192.168.3.10'}},
            {},
            {'endpointHost': '192.168.3.12', 'kubernetesEndpoint': 'https://192.168.3.12:6443', 'gateway': {'hostname': 'gateway.example.test'}},
        ]}
        text = render(values, 'flames.yaml').stdout
        self.assertIn('endpoint: 192.168.3.10:30090', text)
        self.assertIn('endpoint: 192.168.3.12:30100', text)
        self.assertEqual(text.count('hostname: 192.168.3.10'), 2)
        self.assertIn('hostname: gateway.example.test', text)
        self.assertEqual(text.count('kubernetesEndpoint: https://192.168.3.10:6443'), 2)
        self.assertIn('kubernetesEndpoint: https://192.168.3.12:6443', text)

    def test_explicit_ports_are_respected_and_collisions_rejected(self):
        first = {'gateway': {'hostname': '192.168.3.10'}}
        second = {'nodePort': 32180, 'gateway': {'hostname': '192.168.3.11', 'sessionPort': 32443, 'cachePort': 32444}}
        values = {'flames': [first, second]}
        text = render(values, 'flames.yaml').stdout
        for value in ['nodePort: 32180', 'sessionPort: 32443', 'cachePort: 32444']:
            self.assertIn(value, text)
        second['nodePort'] = 30080
        failure = render(values, check=False)
        self.assertNotEqual(failure.returncode, 0)
        self.assertIn('distinct NodePorts', failure.stderr)

    def test_cluster_service_always_present_for_external_flm(self):
        text = render({'flames': [], 'cluster': {'flame': {'endpoint': 'http://external:8080'}}}).stdout
        self.assertIn('kind: Deployment\nmetadata:\n  name: cluster\n', text)
        self.assertIn('kind: Service\nmetadata:\n  name: cluster\n', text)
        self.assertIn('value: "http://external:8080"', text)
        self.assertNotIn('kind: FlameCluster', text)
        self.assertNotIn('FLAME_CONNECTIONS_FILE', text)
        self.assertIn('automountServiceAccountToken: false', text)
        self.assertNotIn('kind: Role', text)
        self.assertNotIn('name: flame-join', text)
        failure = render({'flames': []}, check=False)
        self.assertNotEqual(failure.returncode, 0)
        self.assertIn('cluster.flame.endpoint is required', failure.stderr)

    def test_single_flame_defaults_and_resource_name_limits(self):
        values = {'flames': [{'gateway': {'hostname': '192.168.3.10'}}]}
        resources = render(values, 'flames.yaml').stdout
        self.assertIn('name: flame-1', resources)
        self.assertIn('namespace: flame-1', resources)
        self.assertIn('nodePort: 30080', resources)
        self.assertIn('storageSize: 20Gi', resources)
        self.assertIn('clientAuth: true', resources)
        values['flames'][0]['name'] = 'a' * 32
        failure = render(values, check=False)
        self.assertNotEqual(failure.returncode, 0)
        self.assertIn('at most 32 characters', failure.stderr)


if __name__ == '__main__':
    unittest.main()
