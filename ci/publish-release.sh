#!/bin/sh
# Publish tested artifacts once; an existing SHA release is verification-only.
set +x
set -eu
command -v gh >/dev/null 2>&1 || { echo 'gh is required' >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo 'python3 is required' >&2; exit 1; }
python3 - "${1:-release}" <<'PY'
import filecmp
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def require(value, message):
    if not value:
        raise ValueError(message)


def command(arguments, optional=False):
    result = subprocess.run(['gh', *arguments], text=True, capture_output=True, check=False)
    if result.returncode:
        if optional and 'HTTP 404' in result.stderr:
            return None
        raise ValueError('GitHub release request failed; check repository access and token permissions')
    return result.stdout


def api(path, optional=False):
    result = command(['api', path], optional)
    return None if result is None else json.loads(result)


def digest(path):
    checksum = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            checksum.update(block)
    return checksum.hexdigest()


def resolve_tag(repo, reference):
    for _ in range(8):
        if reference['type'] == 'commit':
            return reference['sha']
        require(reference['type'] == 'tag' and re.fullmatch(r'[0-9a-f]{40}', reference['sha']), 'unexpected release tag object')
        reference = api(f'repos/{repo}/git/tags/{reference["sha"]}')['object']
    raise ValueError('release tag nesting exceeds the verification limit')


try:
    require(os.environ.get('GH_TOKEN'), 'GH_TOKEN is required for release publication')
    revision = os.environ.get('GITHUB_SHA', '')
    repo = os.environ.get('GITHUB_REPOSITORY', '')
    require(re.fullmatch(r'[0-9a-f]{40}', revision), 'GITHUB_SHA must be a full commit SHA')
    require(re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repo), 'GITHUB_REPOSITORY is required')
    directory = Path(sys.argv[1]).resolve()
    archives = {'flmlet-linux-amd64.tar.gz', 'flmlet-linux-arm64.tar.gz', 'flame-cluster-charts.tar.gz'}
    payloads = archives | {'setup.sh'}
    expected_assets = payloads | {'SHA256SUMS'}
    require({path.name for path in directory.glob('*.tar.gz')} == archives, 'release directory must contain exactly the tested worker and chart archives')
    for name in payloads:
        path = directory / name
        require(path.is_file() and not path.is_symlink(), f'tested release asset is missing or not regular: {name}')
    checksums = {}
    for architecture in ('amd64', 'arm64'):
        for line in (directory / f'SHA256SUMS-{architecture}').read_text().splitlines():
            fields = line.split()
            require(len(fields) == 2 and re.fullmatch(r'[0-9a-fA-F]{64}', fields[0]), 'invalid architecture checksum manifest')
            name = fields[1].removeprefix('*')
            require(name in archives and name not in checksums, 'unexpected or duplicate architecture checksum entry')
            checksums[name] = fields[0].lower()
    require(set(checksums) == archives, 'architecture manifests must cover every expected archive')
    for name, checksum in checksums.items():
        require(digest(directory / name) == checksum, f'tested artifact checksum mismatch: {name}')
    checksums['setup.sh'] = digest(directory / 'setup.sh')
    (directory / 'SHA256SUMS').write_text(''.join(f'{checksums[name]}  {name}\n' for name in sorted(payloads)))
    tag = f'sha-{revision}'
    reference = api(f'repos/{repo}/git/ref/tags/{tag}', optional=True)
    if reference is not None:
        require(resolve_tag(repo, reference['object']) == revision, 'existing release tag does not resolve to GITHUB_SHA')
    release = api(f'repos/{repo}/releases/tags/{tag}', optional=True)
    if release is not None:
        require(reference is not None, 'existing release is missing its verified Git tag')
        require(release.get('tag_name') == tag and release.get('draft') is False, 'existing SHA release must already be published under the expected tag')
        names = [asset['name'] for asset in release['assets']]
        require(set(names) == expected_assets and len(names) == len(expected_assets), 'existing immutable release has missing, duplicate, or unexpected assets')
        with tempfile.TemporaryDirectory(prefix='flame-release-verify-') as temporary:
            downloaded = Path(temporary)
            for name in sorted(expected_assets):
                command(['release', 'download', tag, '--repo', repo, '--pattern', name, '--dir', str(downloaded)])
                actual = downloaded / name
                require(actual.is_file() and not actual.is_symlink() and filecmp.cmp(directory / name, actual, shallow=False), f'existing immutable release asset differs: {name}')
                if name in checksums:
                    require(digest(actual) == checksums[name], f'existing immutable release checksum differs: {name}')
        print(f'Verified existing immutable release {tag}; no assets were modified.')
    else:
        command(['release', 'create', tag, *[str(directory / name) for name in sorted(expected_assets)], '--target', revision, '--latest', '--title', f'Flame Cluster {tag}', '--notes', 'Tested native worker binaries and Helm charts.', '--repo', repo])
        print(f'Published immutable release {tag}.')
except (ValueError, OSError, KeyError, TypeError) as error:
    print(f'Release publication failed: {error}', file=sys.stderr)
    sys.exit(1)
PY
