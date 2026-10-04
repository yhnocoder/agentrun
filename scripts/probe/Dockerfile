FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl gnupg bubblewrap socat perl procps \
  && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
  && apt-get install -y --no-install-recommends nodejs \
  && npm install -g @anthropic-ai/claude-code @openai/codex @earendil-works/pi-coding-agent \
  && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY . /opt/probe/
RUN chmod +x /opt/probe/probe.sh
WORKDIR /root
ENTRYPOINT ["/opt/probe/probe.sh"]
