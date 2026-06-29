FROM node:26.4.0-slim
RUN apt-get update && apt install -y --no-install-recommends curl nano bash ca-certificates ssh git wget bash-completion \ 
    && rm -rf /var/lib/apt/lists/*
WORKDIR /agent-vm-work-dir
ENV PATH=$PATH:/root/.local/.npm-global/bin
CMD npm config set prefix /root/.local/.npm-global && bash
