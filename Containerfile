FROM ubuntu:24.04

ARG UID=501
ARG GID=501

# node + claude CLI + common tools; dev user with host UID/GID
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git sudo ripgrep less vim openssh-client && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y nodejs && npm i -g @anthropic-ai/claude-code && \
    (getent group ${GID} || groupadd -g ${GID} dev) && \
    useradd -m -u ${UID} -g ${GID} -s /bin/bash dev && \
    echo 'dev ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/dev

USER dev
WORKDIR /work
