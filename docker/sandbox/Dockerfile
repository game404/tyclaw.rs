FROM python:3.11-slim

ARG APT_MIRROR=deb.debian.org
ARG PIP_INDEX_URL=https://pypi.org/simple
ARG PIP_TRUSTED_HOST=pypi.org
ARG RUSTUP_DIST_SERVER=https://static.rust-lang.org
ARG RUSTUP_UPDATE_ROOT=https://static.rust-lang.org/rustup

# 替换 apt 源（支持国内镜像加速）
RUN sed -i "s|deb.debian.org|${APT_MIRROR}|g" /etc/apt/sources.list.d/debian.sources

# 安装系统工具 + 开发环境 + OpenCV 运行时依赖
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    curl \
    wget \
    jq \
    zip \
    unzip \
    build-essential \
    pkg-config \
    ca-certificates \
    gnupg \
    ripgrep \
    ffmpeg \
    tesseract-ocr \
    tesseract-ocr-chi-sim \
    tesseract-ocr-eng \
    libxcb1 \
    libglib2.0-0 \
    libsm6 \
    libxext6 \
    libxrender1 \
    libgl1 \
    && rm -rf /var/lib/apt/lists/*

# Node.js 22 LTS + TypeScript
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g typescript ts-node

# Rust (stable, minimal profile) —— 安装到系统级路径，任意 UID 可用
ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="/usr/local/cargo/bin:${PATH}"
RUN RUSTUP_DIST_SERVER="$RUSTUP_DIST_SERVER" \
    RUSTUP_UPDATE_ROOT="$RUSTUP_UPDATE_ROOT" \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    && RUSTUP_DIST_SERVER="$RUSTUP_DIST_SERVER" rustup component add clippy rustfmt \
    && chmod -R a+rX /usr/local/cargo /usr/local/rustup

# Python 依赖（通过 requirements.txt 管理，便于扩展）
COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir \
    -i "$PIP_INDEX_URL" \
    --trusted-host "$PIP_TRUSTED_HOST" \
    -r /tmp/requirements.txt \
    && rm /tmp/requirements.txt

# 浏览器自动化环境（Xvfb + Chromium + Playwright）
ENV PLAYWRIGHT_BROWSERS_PATH=/usr/local/share/ms-playwright
RUN apt-get update && apt-get install -y --no-install-recommends \
    xvfb \
    xauth \
    fonts-noto-cjk \
    && playwright install --with-deps chromium \
    && chmod -R a+rX /usr/local/share/ms-playwright \
    && rm -rf /var/lib/apt/lists/*

ENV DISPLAY=:99
ENV LANG=C.UTF-8
ENV LC_ALL=C.UTF-8

# 预创建非 root 用户（--user 传入的 UID 若恰好为 1000 可直接命中；
# 其他 UID 由 entrypoint.sh 动态注入 /etc/passwd）
RUN groupadd -g 1000 tyclaw \
    && useradd -u 1000 -g tyclaw -d /workspace -s /bin/bash tyclaw \
    && chmod 0666 /etc/passwd /etc/group

# 工作目录
WORKDIR /work

# 启动 Xvfb 虚拟屏幕 + sleep infinity 等待 docker exec
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
