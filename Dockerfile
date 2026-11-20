FROM docker.io/library/rust:1.95-bookworm AS build
WORKDIR /workspace
COPY Cargo.toml Cargo.lock ./
COPY flame-api flame-api
COPY flame-operator flame-operator
COPY flame-cluster flame-cluster
COPY flmlet flmlet
RUN cargo build --release --locked --workspace --bins

FROM scratch AS flmlet-binary
COPY --from=build /workspace/target/release/flmlet /bin/flmlet

FROM docker.io/library/debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 flame \
    && useradd --uid 10001 --gid flame --create-home flame
USER 10001

FROM runtime AS flame-operator
COPY --from=build /workspace/target/release/flame-operator /usr/local/bin/flame-operator
EXPOSE 8081
ENTRYPOINT ["/usr/local/bin/flame-operator"]

FROM runtime AS flame-cluster
COPY --from=build /workspace/target/release/flame-cluster /usr/local/bin/flame-cluster
ENV CONSOLE_BIND=0.0.0.0:3000
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/flame-cluster"]

FROM docker.io/library/node:lts-alpine AS ui-build
WORKDIR /ui
COPY flame-ui/package.json flame-ui/package-lock.json ./
RUN npm ci --include=dev --include=optional
COPY flame-ui/build.mjs ./
COPY flame-ui/static/ ./static/
RUN npm run build

FROM docker.io/nginxinc/nginx-unprivileged:alpine AS flame-ui
ENV CONSOLE_API_UPSTREAM=cluster:3000
COPY --from=ui-build /ui/dist/ /usr/share/nginx/html/
COPY flame-ui/nginx.conf.template /etc/nginx/templates/default.conf.template
EXPOSE 8080
