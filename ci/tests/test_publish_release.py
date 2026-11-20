#!/usr/bin/env python3
"""Verify immutable publishing with local artifacts and a fake GitHub CLI."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
REVISION = 'a' * 40


def manifest(directory, names):
    return ''.join(f'{hashlib.sha256((directory / name).read_bytes()).hexdigest()}  {name}\n' for name in sorted(names))


class PublishRelease(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.artifacts = self.root / 'artifacts'
        self.artifacts.mkdir()
        self.remote = self.root / 'remote'
        self.remote.mkdir()
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.log = self.root / 'requests.jsonl'
        for name in ['flmlet-linux-amd64.tar.gz', 'flmlet-linux-arm64.tar.gz', 'flame-cluster-charts.tar.gz']:
            (self.artifacts / name).write_bytes(f'Tested {name}'.encode())
        (self.artifacts / 'setup.sh').write_text('#!/bin/sh\nexit 0\n')
        (self.artifacts / 'SHA256SUMS-amd64').write_text(manifest(self.artifacts, ['flmlet-linux-amd64.tar.gz', 'flame-cluster-charts.tar.gz']))
        (self.artifacts / 'SHA256SUMS-arm64').write_text(manifest(self.artifacts, ['flmlet-linux-arm64.tar.gz']))
        for path in self.artifacts.glob('*.tar.gz'):
            (self.remote / path.name).write_bytes(path.read_bytes())
        (self.remote / 'setup.sh').write_bytes((self.artifacts / 'setup.sh').read_bytes())
        (self.remote / 'SHA256SUMS').write_text(manifest(self.artifacts, ['flmlet-linux-amd64.tar.gz', 'flmlet-linux-arm64.tar.gz', 'flame-cluster-charts.tar.gz', 'setup.sh']))
        self.env = os.environ | {'PATH': f'{self.bin}:{os.environ["PATH"]}', 'GH_TOKEN': 'publisher-secret-never-print', 'GITHUB_SHA': REVISION, 'GITHUB_REPOSITORY': 'xflops/flame-cluster', 'TEST_REMOTE': str(self.remote), 'TEST_LOG': str(self.log), 'TEST_RELEASE_STATE': 'missing', 'TEST_TAG_STATE': 'missing'}
        program = self.bin / 'gh'
        program.write_text('''#!/usr/bin/env python3
import json,os,shutil,sys
from pathlib import Path
args=sys.argv[1:]
with open(os.environ['TEST_LOG'],'a') as log: log.write(json.dumps(args)+'\\n')
def missing():
    print('gh: Not Found (HTTP 404)',file=sys.stderr);sys.exit(1)
if args[0]=='api':
    path=args[1]
    if os.environ.get('TEST_FORBIDDEN')=='1':
        print('gh: Forbidden (HTTP 403)',file=sys.stderr);sys.exit(1)
    if '/git/ref/tags/' in path:
        state=os.environ['TEST_TAG_STATE']
        if state=='missing': missing()
        print(json.dumps({'object':{'type':'tag' if state=='annotated' else 'commit','sha':'b'*40 if state in ('wrong','annotated') else os.environ['GITHUB_SHA']}}))
    elif '/git/tags/' in path:
        print(json.dumps({'object':{'type':'commit','sha':os.environ['GITHUB_SHA']}}))
    elif '/releases/tags/' in path:
        if os.environ['TEST_RELEASE_STATE']=='missing': missing()
        names=sorted(path.name for path in Path(os.environ['TEST_REMOTE']).iterdir())
        print(json.dumps({'tag_name':'sha-'+os.environ['GITHUB_SHA'],'draft':False,'assets':[{'name':name} for name in names]}))
    else: sys.exit(2)
elif args[:2]==['release','download']:
    name=args[args.index('--pattern')+1];target=Path(args[args.index('--dir')+1]);target.mkdir(exist_ok=True)
    shutil.copyfile(Path(os.environ['TEST_REMOTE'])/name,target/name)
elif args[:2]==['release','create']:
    print('https://github.com/xflops/flame-cluster/releases/test')
else:
    print('unexpected GitHub mutation',file=sys.stderr);sys.exit(3)
''')
        program.chmod(0o755)

    def run_publish(self, **updates):
        result = subprocess.run(['sh', '-x', str(ROOT / 'ci/publish-release.sh'), str(self.artifacts)], text=True, capture_output=True, env=self.env | updates)
        self.assertNotIn(self.env['GH_TOKEN'], result.stdout + result.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        self.assertFalse(any('--clobber' in call for call in calls))
        self.assertFalse(any(call[:2] == ['release', 'upload'] for call in calls))
        return result, calls

    def test_first_publication_uses_tested_assets_and_exact_commit(self):
        result, calls = self.run_publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        create = [call for call in calls if call[:2] == ['release', 'create']]
        self.assertEqual(len(create), 1)
        self.assertEqual(create[0][create[0].index('--target') + 1], REVISION)
        self.assertIn('--latest', create[0])
        self.assertEqual({Path(value).name for value in create[0] if value.startswith(str(self.artifacts.resolve()))}, {path.name for path in self.remote.iterdir()})
        self.assertEqual((self.artifacts / 'SHA256SUMS').read_bytes(), (self.remote / 'SHA256SUMS').read_bytes())

    def test_identical_existing_release_is_verified_without_mutation(self):
        result, calls = self.run_publish(TEST_RELEASE_STATE='published', TEST_TAG_STATE='correct')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len([call for call in calls if call[:2] == ['release', 'download']]), 5)
        self.assertFalse(any(call[:2] == ['release', 'create'] for call in calls))
        self.assertIn('no assets were modified', result.stdout)

    def test_annotated_tag_is_resolved_to_its_commit(self):
        result, calls = self.run_publish(TEST_RELEASE_STATE='published', TEST_TAG_STATE='annotated')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any('/git/tags/' in call[-1] for call in calls if call[0] == 'api'))

    def test_existing_tag_for_another_commit_prevents_publication(self):
        result, calls = self.run_publish(TEST_RELEASE_STATE='published', TEST_TAG_STATE='wrong')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('does not resolve', result.stderr)
        self.assertFalse(any(call[0] == 'release' for call in calls))

    def test_preexisting_wrong_tag_without_release_is_not_published(self):
        result, calls = self.run_publish(TEST_TAG_STATE='wrong')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(call[:2] == ['release', 'create'] for call in calls))

    def test_mismatched_published_asset_fails_without_upload(self):
        (self.remote / 'flmlet-linux-amd64.tar.gz').write_bytes(b'changed remote bytes')
        result, calls = self.run_publish(TEST_RELEASE_STATE='published', TEST_TAG_STATE='correct')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('asset differs', result.stderr)
        self.assertFalse(any(call[:2] == ['release', 'create'] for call in calls))

    def test_missing_published_asset_fails_without_upload(self):
        (self.remote / 'setup.sh').unlink()
        result, calls = self.run_publish(TEST_RELEASE_STATE='published', TEST_TAG_STATE='correct')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing, duplicate, or unexpected assets', result.stderr)
        self.assertFalse(any(call[0] == 'release' for call in calls))

    def test_unexpected_published_asset_is_not_silently_accepted(self):
        (self.remote / 'extra.tar.gz').write_bytes(b'unexpected')
        result, calls = self.run_publish(TEST_RELEASE_STATE='published', TEST_TAG_STATE='correct')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(call[0] == 'release' for call in calls))

    def test_local_checksum_failure_prevents_any_github_request(self):
        (self.artifacts / 'flmlet-linux-arm64.tar.gz').write_bytes(b'tampered')
        result, calls = self.run_publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checksum mismatch', result.stderr)
        self.assertEqual(calls, [])

    def test_github_permission_errors_do_not_create_a_release(self):
        result, calls = self.run_publish(TEST_FORBIDDEN='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(call[0] == 'release' for call in calls))


if __name__ == '__main__':
    unittest.main()
