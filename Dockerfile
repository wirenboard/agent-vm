FROM node:26.4.0-slim
RUN apt-get update && apt install -y --no-install-recommends curl nano bash ca-certificates ssh git wget bash-completion \ 
    && rm -rf /var/lib/apt/lists/*
WORKDIR /claude-work-dir
ENV PATH=$PATH:/claude-work-dir/.npm-global/bin
CMD npm config set prefix /claude-work-dir/.npm-global && bash
