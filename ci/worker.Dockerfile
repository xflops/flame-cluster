# Disposable host fixture; production installs flmlet on an SRE-prepared machine.
FROM docker.io/xflops/flame-executor-manager:latest
USER root
RUN apt-get update && apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus curl python3 \
    && rm -rf /var/lib/apt/lists/*
ENV container=docker
STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/sbin/init"]

CMD []
