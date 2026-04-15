---
name: 阿里云OSS管理
description: 上传、下载、列出、删除阿里云 OSS 文件，生成签名链接
category: ops
tags: [oss, aliyun, 阿里云, 上传, 下载, 签名链接, 存储]
triggers:
  - 上传到OSS
  - OSS上传
  - 上传文件
  - 列出OSS
  - OSS文件
  - 签名链接
  - 预览图片
  - 删除OSS
tool: null
risk_level: write
---
# 阿里云 OSS 管理

通过 `aliyun-oss-cli` 命令行工具管理阿里云 OSS 存储桶中的文件：上传、列出、预览、签名、删除。

## 前置条件

- 已安装 `aliyun-oss-cli`（Rust 编译的二进制），源码及文档见 https://github.com/tuyoogame/aliyun-oss-cli.rs
- 已配置 `~/.aliyun-oss-cli/config.yaml` 或通过环境变量设置凭证

### 安装

```bash
git clone https://github.com/tuyoogame/aliyun-oss-cli.rs.git
cd aliyun-oss-cli.rs
./build.sh
```

编译产物在 `build/` 目录下，将二进制放入 `$PATH` 即可使用。

### 初始化配置

```bash
aliyun-oss-cli init
```

生成 `~/.aliyun-oss-cli/config.yaml`，编辑填入 endpoint、access-key、secret-key、bucket。

### 配置格式

**扁平格式**（单配置）：

```yaml
endpoint: "oss-cn-hangzhou.aliyuncs.com"
access-key: "your-access-key-id"
secret-key: "your-access-key-secret"
bucket: "your-bucket-name"
```

**多 profile 格式**（多环境，用 `--profile` 切换）：

```yaml
default:
  endpoint: "oss-cn-hangzhou.aliyuncs.com"
  access-key: "..."
  secret-key: "..."
  bucket: "bucket-a"

beijing:
  endpoint: "oss-cn-beijing.aliyuncs.com"
  access-key: "..."
  secret-key: "..."
  bucket: "bucket-b"
```

### 配置优先级

命令行参数 > 环境变量 > 配置文件

环境变量：`ALIYUN_OSS_ENDPOINT`、`ALIYUN_OSS_ACCESS_KEY`、`ALIYUN_OSS_SECRET_KEY`、`ALIYUN_OSS_BUCKET`

## 命令速查

| 命令 | 用途 | 风险等级 |
|------|------|----------|
| `init` | 初始化配置文件 | write |
| `ls` | 列出文件 | read |
| `upload` | 上传文件/目录 | write |
| `preview` | 图片预览（签名链接） | read |
| `sign` | 生成签名访问链接 | read |
| `delete` | 删除文件 | **destructive** |

## 使用方法

### 列出文件

```bash
aliyun-oss-cli ls                           # 根目录
aliyun-oss-cli ls images/                   # 指定前缀
aliyun-oss-cli ls -l                        # 详细模式（大小+时间）
aliyun-oss-cli ls --max-keys 50             # 限制返回数量
aliyun-oss-cli ls --profile beijing         # 使用指定 profile
```

### 上传文件

```bash
aliyun-oss-cli upload myfile.jpg                  # 上传单文件
aliyun-oss-cli upload myfile.jpg images/          # 上传到指定目录
aliyun-oss-cli upload myfolder/ -d                # 上传整个目录
aliyun-oss-cli upload myfile.jpg -p assets/       # 指定前缀
aliyun-oss-cli upload myfile.jpg --public         # 公共读权限
aliyun-oss-cli upload myfile.jpg -r               # 覆盖已存在文件
```

| 选项 | 短选项 | 说明 |
|------|--------|------|
| `DESTINATION` | - | 位置参数，目标路径 |
| `--dir` | `-d` | 上传目录模式 |
| `--prefix` | `-p` | 目标路径前缀 |
| `--public` | - | 公共读 ACL |
| `--replace` | `-r` | 覆盖已存在文件 |

### 预览图片

```bash
aliyun-oss-cli preview myimage.jpg               # 生成签名链接（24h 有效）
aliyun-oss-cli preview myimage.jpg -o            # 浏览器打开
aliyun-oss-cli preview myimage.jpg --expire 3600 # 1 小时有效
```

### 生成签名链接

```bash
aliyun-oss-cli sign myfile.txt                    # 下载签名链接（24h）
aliyun-oss-cli sign myfile.txt -u                 # 上传签名链接
aliyun-oss-cli sign myfile.txt -e 3600            # 1 小时有效
```

### 删除文件

```bash
aliyun-oss-cli delete myfile.jpg                  # 删除单文件（需确认）
aliyun-oss-cli delete file1.jpg file2.png         # 批量删除
aliyun-oss-cli delete myfile.jpg -q               # 静默模式，跳过确认
```

## 安全约束

1. **删除操作前必须确认**：除非用户明确要求，禁止使用 `-q` 跳过确认
2. 禁止在日志或输出中暴露 access-key / secret-key
3. 上传公共读文件（`--public`）前需向用户确认，避免意外公开敏感文件
4. 操作多个文件时先 `ls` 确认目标，再执行写/删操作

## 常见场景

### 上传构建产物到 OSS

```bash
aliyun-oss-cli upload dist/ -d -p releases/v1.0.0/ --profile default
```

### 生成临时分享链接

```bash
aliyun-oss-cli sign docs/report.pdf -e 7200
```

### 清理过期文件

```bash
aliyun-oss-cli ls old-prefix/ -l
aliyun-oss-cli delete old-prefix/file1.txt old-prefix/file2.txt
```
