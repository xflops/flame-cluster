#!/usr/bin/env bash
set -euo pipefail
: "${ARCH:?}" "${GITHUB_SHA:?}" "${RUNNER_TEMP:?}"
[[ "$ARCH" == amd64 || "$ARCH" == arm64 ]]
images=()
for component in flame-operator flame-console flame-ui; do
  image="xflops/$component:sha-$GITHUB_SHA-$ARCH"
  docker buildx build --platform "linux/$ARCH" --target "$component" --tag "$image" --load --provenance=false .
  test "$(docker image inspect "$image" --format '{{.Architecture}}')" = "$ARCH"
  if [[ "$component" == flame-ui ]]; then
    docker run --rm -e CONSOLE_API_UPSTREAM=127.0.0.1:3000 "$image" nginx -t
  else
    docker run --rm --entrypoint sh "$image" -ec 'test "$(id -u)" = 10001; test -x "/usr/local/bin/$1"; openssl version' check "$component"
  fi
  images+=("$image")
done
docker save "${images[@]}" | gzip > "$RUNNER_TEMP/flame-images-$ARCH.tar.gz"
artifacts="$RUNNER_TEMP/flame-release-$ARCH"
mkdir -p "$artifacts/worker"
docker buildx build --platform "linux/$ARCH" --target flmlet-binary \
  --output "type=local,dest=$artifacts/worker" --provenance=false .
cp LICENSE "$artifacts/worker/LICENSE"
tar -C "$artifacts/worker" -czf "$artifacts/flmlet-linux-$ARCH.tar.gz" bin/flmlet LICENSE
if [[ "$ARCH" == amd64 ]]; then
  tar -czf "$artifacts/flame-cluster-charts.tar.gz" charts
  cp setup.sh "$artifacts/setup.sh"
fi
(
  cd "$artifacts"
  sha256sum *.tar.gz > SHA256SUMS
  cp SHA256SUMS "SHA256SUMS-$ARCH"
)
