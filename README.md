# agentrun

agentrun 是一个 Rust 命令行工具，在本机 macOS、本机 Linux、云端容器和 docker 容器里用同一条命令运行 claude-code、codex 和 pi，并在每种环境能提供的范围内隔离 agent。

目前处于开发初期，还没有可用的功能。设计文档发布在 https://yhnocoder.github.io/agentrun/ ，源文件从 `docs/index.html` 开始。

## 开发

```sh
scripts/test.sh
```

依次运行格式检查、clippy、单元测试与注释检查。需要 Rust 工具链与 uv。
