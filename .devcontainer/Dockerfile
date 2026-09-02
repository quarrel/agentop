FROM node:22-trixie-slim AS node

FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie

ARG DEBIAN_FRONTEND=noninteractive
ARG TILTH_VERSION=0.10.1
ARG TILTH_SHA256=3df16574ed9fc33e4d9c63107994c78d48f93ab0099a37232b4410223e2ad2ea

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash-completion \
        ca-certificates \
        curl \
        dnsutils \
        fd-find \
        file \
        gdb \
        gh \
        git \
        inotify-tools \
        iproute2 \
        iputils-ping \
        jq \
        less \
        lsof \
        netcat-openbsd \
        openssh-client \
        procps \
        ripgrep \
        rsync \
        shellcheck \
        socat \
        strace \
        sysstat \
        tcpdump \
        time \
        tmux \
        traceroute \
        unzip \
        vim \
        xxd \
        zip \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
    && ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && npm install --global @openai/codex@latest \
    && architecture="$(dpkg --print-architecture)" \
    && test "$architecture" = amd64 \
    && curl -fsSL \
        -o /tmp/tilth.tar.gz \
        "https://github.com/jahala/tilth/releases/download/v${TILTH_VERSION}/tilth-x86_64-unknown-linux-musl.tar.gz" \
    && echo "${TILTH_SHA256}  /tmp/tilth.tar.gz" | sha256sum -c - \
    && tar xzf /tmp/tilth.tar.gz -C /usr/local/bin \
    && chmod 0755 /usr/local/bin/tilth \
    && unlink /tmp/tilth.tar.gz \
    && rustup component add clippy rustfmt \
    && codex --version \
    && tilth --version \
    && apt-get clean \
    && find /var/lib/apt/lists -type f -delete

USER vscode
WORKDIR /workspaces/agentop
