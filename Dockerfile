FROM node:26.4.0-slim
RUN apt-get update && apt install -y --no-install-recommends curl nano bash ca-certificates ssh git wget bash-completion \ 
    && rm -rf /var/lib/apt/lists/*
WORKDIR /cloude-work-dir
#ENV PATH=$PATH:
#RUN     source < (agent-vm completion bash)
COPY ./start.sh /.
RUN chmod +x /start.sh
#USER nobody
CMD ["/start.sh"]

#RUN
