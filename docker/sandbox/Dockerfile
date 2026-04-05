FROM python:3.11-slim

# 安装系统工具
RUN apt-get update && apt-get install -y --no-install-recommends \
    ripgrep \
    ffmpeg \
    tesseract-ocr \
    tesseract-ocr-chi-sim \
    tesseract-ocr-eng \
    && rm -rf /var/lib/apt/lists/*

# 预装常用包
RUN pip install --no-cache-dir \
    pandas \
    numpy \
    openpyxl \
    requests \
    opencv-python-headless \
    av \
    pytesseract \
    faster-whisper \
    "scenedetect[opencv]"

# 工作目录
WORKDIR /work

# 容器池模式：sleep infinity 等待 docker exec
CMD ["sleep", "infinity"]
