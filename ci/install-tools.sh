#!/usr/bin/env bash
set -euo pipefail
: "${RUNNER_TEMP:?}" "${GITHUB_PATH:?}"
tools="$RUNNER_TEMP/flame-tools"
mkdir -p "$tools"
for tool in "$@"; do
  case "$tool" in
    helm)
      version="$(gh api repos/helm/helm/releases/latest --jq .tag_name)"
      archive="helm-$version-linux-amd64.tar.gz"
      curl -fsSL "https://get.helm.sh/$archive" -o "$tools/$archive"
      curl -fsSL "https://get.helm.sh/$archive.sha256sum" -o "$tools/helm.sha256sum"
      (cd "$tools" && sha256sum -c helm.sha256sum && tar -xzf "$archive")
      install -m 0755 "$tools/linux-amd64/helm" "$tools/helm"
      ;;
    kind)
      version="$(gh api repos/kubernetes-sigs/kind/releases/latest --jq .tag_name)"
      curl -fsSL "https://github.com/kubernetes-sigs/kind/releases/download/$version/kind-linux-amd64" -o "$tools/kind-linux-amd64"
      curl -fsSL "https://github.com/kubernetes-sigs/kind/releases/download/$version/kind-linux-amd64.sha256sum" -o "$tools/kind.sha256sum"
      (cd "$tools" && sha256sum -c kind.sha256sum)
      install -m 0755 "$tools/kind-linux-amd64" "$tools/kind"
      ;;
    *) echo "Unknown test tool: $tool" >&2; exit 2 ;;
  esac
done
echo "$tools" >> "$GITHUB_PATH"
