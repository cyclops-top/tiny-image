# 参与开发

感谢帮助 tinyimg 变得更可靠。当前版本仍是开发预览，请优先改进安全性、可复现测试和质量校准，而不是宣称“肉眼无损”。

## 本地开发

需要 Rust 1.88 或更新版本和可用的 C/C++ 编译环境。原生 WebP、MozJPEG 与 libdeflate 依赖从 Cargo 依赖源码编译。Linux 可使用 GCC/Clang，macOS 使用 Xcode Command Line Tools，Windows 使用 MSVC Build Tools。项目的 MozJPEG 默认禁用可选 SIMD 构建功能，因此不把 NASM 当作当前必需依赖。请提交 `Cargo.lock` 的有意变更。

```sh
cargo build --locked --workspace
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -p tinyimg-cli -- --help
```

默认工作流只检查 Linux；macOS/Windows 通过手动选项执行，不是必需检查。只有对应平台实际运行通过，才能声称该平台已验证。不要以 Linux 通过推断 Windows 的替换、权限或大小写行为。

## 变更要求

- 对新行为增加测试；错误路径必须检查源文件是否仍然完整。
- `--dry-run`、`analyze`、`--check` 不允许创建目录、备份、报告文件或缓存，也不能改输入文件。
- 所有写入都必须复用统一的安全写入层，不能绕开源内容校验、目标冲突检查和非覆盖备份策略。
- 给新编码器添加解码、尺寸、透明度、评分和“不变大”验证；不要只测编码成功。
- 编码器升级需检查最终产物而非仅检查质量参数。质量参数与评分不保证严格单调。
- 修改配置字段时同步 `examples/tinyimg.toml`、`init` 模板、帮助文本和配置优先级测试。
- 不要提交真实项目素材、GPS/EXIF、个人信息、密钥或不明授权图片。测试样本优先用程序生成。
- 新依赖需核对许可证；原生库的许可证不能仅靠 Rust wrapper 的 Cargo 元数据推断。流程见 [依赖说明](docs/DEPENDENCIES.md)。

## 校准与性能

当前 high/balanced/small = 88/80/70 是实验值。需要 100–200 张有明确授权的真实资源图，覆盖文字、细线、图标、照片、渐变、阴影、透明边缘、各密度及暗色主题。保留原图、所有候选的分数/大小、人工审查记录、版本和命令。不要把程序生成样本上的通过测试称作感知质量校准。

性能报告应包括操作系统、CPU、内存、Rust/编码器版本、并行数、图片数量与尺寸、总耗时和峰值内存。未实现的缓存不能用于性能承诺。

## 提交问题或补丁

缺陷报告请提供版本、完整命令、退出码、脱敏日志及可共享的最小样本。若涉及文件损坏，先保存原文件和备份，避免继续覆盖。补丁应说明动机、边界行为和已运行的检查。不要在没有测试记录的情况下声称“全平台通过”。

贡献在仓库的 GPL-3.0-or-later 许可下提供。许可证正文见 [LICENSE](LICENSE)。
