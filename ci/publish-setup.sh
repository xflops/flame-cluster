#!/bin/sh
# CI supplies GH_TOKEN from the XFLOPS_CLUSTER_TOKEN Actions secret.
set +x
set -eu
root="$(CDPATH= cd "$(dirname "$0")/.." && pwd)"
command -v gh >/dev/null 2>&1 || { echo 'gh is required' >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo 'python3 is required' >&2; exit 1; }
python3 - "${1:-$root/setup.sh}" <<'PY'
import base64, json, os, subprocess, sys
from pathlib import Path
from urllib.parse import quote

repo = 'xflops/xflops.github.io'

def api(path, payload=None, missing=False):
    args = ['gh', 'api', path]
    if payload is not None:
        args += ['--method', 'PUT', '--input', '-']
    result = subprocess.run(args, input=json.dumps(payload) if payload is not None else None, text=True, capture_output=True, check=False)
    if result.returncode:
        if missing and 'HTTP 404' in result.stderr:
            return None
        raise RuntimeError('GitHub request failed; check repository access and token permissions')
    return json.loads(result.stdout)

try:
    if not os.environ.get('GH_TOKEN'):
        raise RuntimeError('GH_TOKEN must be supplied from the XFLOPS_CLUSTER_TOKEN Actions secret')
    source = Path(sys.argv[1]).read_bytes()
    if not source.startswith(b'#!/bin/sh\n'):
        raise RuntimeError('installer must be a POSIX shell script')
    repository = api(f'repos/{repo}')
    branch = repository['default_branch']
    existing = api(f'repos/{repo}/contents/cluster/setup.sh?ref={quote(branch, safe="")}', missing=True)
    if existing is not None and base64.b64decode(existing['content']) == source:
        print('Cluster installer is already current.')
        sys.exit(0)
    payload = {'message': 'Update Flame Cluster installer', 'branch': branch, 'content': base64.b64encode(source).decode('ascii')}
    if existing is not None:
        payload['sha'] = existing['sha']
    api(f'repos/{repo}/contents/cluster/setup.sh', payload)
    print('Published cluster/setup.sh to the website repository.')
except (RuntimeError, ValueError, OSError, KeyError) as error:
    print(f'Installer publication failed: {error}', file=sys.stderr)
    sys.exit(1)
PY
