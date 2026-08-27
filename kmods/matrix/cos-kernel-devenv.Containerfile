ARG COS_DEVENV_BASE
FROM ${COS_DEVENV_BASE}

RUN apt-get -y update && \
    apt-get -y install apt-transport-https ca-certificates gnupg curl libncurses-dev && \
    echo "deb [signed-by=/usr/share/keyrings/cloud.google.gpg] https://packages.cloud.google.com/apt cloud-sdk main" \
      > /etc/apt/sources.list.d/google-cloud-sdk.list && \
    curl --fail --location --silent --show-error \
      https://packages.cloud.google.com/apt/doc/apt-key.gpg | \
      gpg --dearmor -o /usr/share/keyrings/cloud.google.gpg && \
    apt-get -y update && \
    DEBIAN_FRONTEND=noninteractive TZ=Etc/UTC apt-get -y install tzdata && \
    apt-get -y install make python3 git libssl-dev bc bison flex cpio kmod \
      dwarves google-cloud-cli xz-utils libelf-dev && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

COPY devenv.sh /devenv.sh
ENTRYPOINT ["/devenv.sh"]
