FROM ubuntu:24.04

ARG UID=501
ARG GID=501

# node + claude CLI + gh + common tools; dev user with host UID/GID.
# GitHub's SSH host keys are baked into /etc/ssh/ssh_known_hosts from the
# authenticated api.github.com/meta rather than left to ssh-keyscan/TOFU:
# the sandbox runs non-interactively, so an unknown host key is not a
# prompt anyone can answer, it is `git push` dying on "Host key
# verification failed" — with [container] ssh = true forwarding a perfectly
# good agent that never gets consulted.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git sudo ripgrep less vim openssh-client jq tmux && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y nodejs && npm i -g @anthropic-ai/claude-code && \
    curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /usr/share/keyrings/githubcli-archive-keyring.gpg && \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list && \
    apt-get update && apt-get install -y --no-install-recommends gh && \
    curl -fsSL https://api.github.com/meta -o /tmp/gh-meta.json && \
    jq -r '.ssh_keys[] | "github.com \(.)"' /tmp/gh-meta.json > /tmp/known_hosts.gh && \
    test -s /tmp/known_hosts.gh && \
    cat /tmp/known_hosts.gh >> /etc/ssh/ssh_known_hosts && \
    rm -f /tmp/gh-meta.json /tmp/known_hosts.gh && \
    (getent group ${GID} || groupadd -g ${GID} dev) && \
    useradd -m -u ${UID} -g ${GID} -s /bin/bash dev && \
    echo 'dev ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/dev

# tmux ships for Claude Code's agent-teams split-pane display (README:
# "Claude Code agent teams (split panes)"); keep the chrome minimal by default.
RUN printf '%s\n' \
      '# pall8t: keep the tmux chrome minimal inside agent sessions.' \
      '# Users can override in ~/.tmux.conf (persistent home).' \
      'set -g status off' \
      > /etc/tmux.conf

USER dev
WORKDIR /work
