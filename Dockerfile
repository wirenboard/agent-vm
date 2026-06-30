FROM node:26.4.0-slim
RUN apt-get update && apt install -y --no-install-recommends curl nano bash ca-certificates ssh git wget bash-completion \ 
    && rm -rf /var/lib/apt/lists/*
ENV PATH=$PATH:/agent-vm-work-dir/.local/.npm-global/bin
USER nobody
WORKDIR /agent-vm-work-dir
ENV HOME=/agent-vm-work-dir
CMD npm config set prefix /agent-vm-work-dir/.local/.npm-global && bash
