FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl bubblewrap socat procps \
  && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
  && apt-get install -y --no-install-recommends nodejs \
  && npm install -g @anthropic-ai/claude-code @openai/codex @earendil-works/pi-coding-agent \
  && apt-get clean && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /root/.pi/agent \
  && printf '{"defaultProvider":"deepseek","defaultModel":"deepseek-flash"}\n' > /root/.pi/agent/settings.json

WORKDIR /root
