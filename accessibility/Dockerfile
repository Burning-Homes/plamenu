# Browser engines remain pinned; the distro encoder is only a test publisher,
# not an additional Plamenu runtime service or production dependency.
FROM mcr.microsoft.com/playwright@sha256:eff16c30e6f3f4af0a03fa4b706120d5e9b0891c344a27d64559aff5900a4a27
# Only Ubuntu's signed repositories are needed; the base image's unrelated
# NodeSource repository must not gate installing this distro encoder.
RUN apt-get -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/ubuntu.sources -o Dir::Etc::sourceparts=- -o Acquire::Retries=1 -o Acquire::http::Timeout=30 update \
    && apt-get -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/ubuntu.sources -o Dir::Etc::sourceparts=- -o Acquire::Retries=1 -o Acquire::http::Timeout=30 install -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/*
