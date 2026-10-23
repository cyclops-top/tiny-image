# 依赖与第三方许可证

项目代码以 **GPL-3.0-or-later** 许可提供，完整 GPLv3 正文在根目录 [LICENSE](../LICENSE)。`or-later` 选择由 Cargo 元数据和本文声明。以下是当前锁定依赖中的主要组件，不是完整的传递依赖清单，也不替代各许可证原文。

| 组件 | 版本 | Cargo 声明的许可 | 用途 |
| --- | --- | --- | --- |
| indicatif | 0.18.6 | MIT | 多图片终端进度与动画，非终端禁用 |
| console | 0.16.6 | MIT | 跨平台终端尺寸、Unicode 宽度与安全截断 |
| oxipng | 10.2.1 | MIT | PNG 无损优化 |
| imagequant | 4.4.1 | GPL-3.0-or-later | PNG 调色板量化 |
| webp | 0.3.1 | MIT OR Apache-2.0 | WebP Rust 接口 |
| libwebp-sys | 0.9.6 | MIT | WebP 原生接口；内置 libwebp 另有 BSD-3-Clause 和专利声明 |
| mozjpeg | 0.10.13 | IJG | JPEG 编码接口 |
| mozjpeg-sys | 2.2.3 | IJG AND Zlib AND BSD-3-Clause | 内置原生 JPEG 编码库 |
| ssimulacra2 | 0.5.1 | BSD-2-Clause | 感知质量评分 |
| ravif | 0.13.0 | BSD-3-Clause | 真正的 AVIF 编码，关闭 assembly/threading 特性 |
| rav1e | 0.8.1 | BSD-2-Clause | Rust AV1 编码器 |
| avif-decode | 3.0.0（本地微小补丁） | BSD-3-Clause | 仅重新解码本次生成的 AVIF 候选 |
| rav1d | 1.1.0 | BSD-2-Clause | Rust AV1 解码器，关闭 assembly |
| avif-parse | 2.1.0 | MPL-2.0 | AVIF 容器及候选尺寸预检 |
| to_method | 1.1.0 | CC0-1.0 | AVIF 容器解析器的辅助方法 |
| libfuzzer-sys | 0.4.13 | NCSA | 锁文件中 rav1e 的可选 fuzz 依赖，默认运行时未启用 |
| libdeflate-sys | 1.26.1 | Apache-2.0 | 原生绑定；内置 libdeflate 另有 MIT 许可 |

版本与声明核对自本次 Cargo 下载后的各 crate `Cargo.toml`，最终版本以 [Cargo.lock](../Cargo.lock) 为准。可以通过 [crates.io](https://crates.io/) 按精确名称与版本查看相应源码；不能仅根据包装库的许可证推断其内置 C 库的许可。

This software is based in part on the work of the Independent JPEG Group.

## 已附带的原生库声明

[licenses/](licenses/) 保留以下原文，文件名注明当前 crate 版本：

- MozJPEG 与 mozjpeg-sys 的 LICENSE。
- libwebp-sys 的 `vendor/COPYING` 与 `vendor/PATENTS`。
- libdeflate-sys 的 `libdeflate/COPYING`。

这些文件从锁定 Cargo 包直接复制，未改写许可正文。升级相关依赖时必须重新检查和替换。不能把这些少数原生库声明当作完整 Rust 依赖清单。

## 生成与复核完整清单

在已安装 Rust、能够访问 crates.io 的开发环境中运行：

```sh
cargo fetch --locked
cargo install --locked cargo-deny
cargo install --locked cargo-about --features cli
cargo deny check licenses sources
cargo about generate --locked --workspace --fail --format json \
  -o THIRD-PARTY-LICENSES.json
python scripts/sanitize_license_inventory.py
```

`deny.toml` 与 `about.toml` 是许可策略文件，不是“已审计通过”的证明。每次分发应保留工具版本、完整输出、锁文件以及所构建的平台；查阅 [cargo-deny 许可配置](https://embarkstudios.github.io/cargo-deny/checks/licenses/cfg.html) 和 [cargo-about generate](https://embarkstudios.github.io/cargo-about/cli/generate/index.html)。生成器无法识别的许可必须人工核对，不应仅为了通过检查而删掉依赖或放宽许可清单。

如需检查当前已知依赖漏洞，可单独运行 `cargo deny check advisories`。CI 的 `licenses sources` 检查不等于已完成安全漏洞审计。

## 分发前检查

1. 对精确 `Cargo.lock`、实际 feature 集与平台生成许可清单，并确认每个 `*-sys` 包的原生源码许可都已包含。
2. 保留 GPL 正文、版权与第三方声明，按所选许可证条款提供对应源代码和构建材料。仅附一份本项目源码归档不能自动保证覆盖所有 GPL 分发义务。
3. 若生成器输出缺项、平台构建尚未验证，或依赖来源无法复核，先修复再发布。
4. 当前手动工作流只创建供审查的 CI artifacts，不执行 `cargo publish`、不创建 GitHub Release，也不证明已完成分发合规审查。

本文件是工程清单，不是法律意见；许可证原文和你的实际分发方式决定具体义务。

本次 Linux 验证中，cargo-deny 0.20.2 的 licenses/sources 检查已通过，cargo-about 0.9.2 已成功生成仓库根目录 THIRD-PARTY-LICENSES.json。详见 [验证记录](VALIDATION.md)。

## AVIF 候选验证的本地补丁

[avif-decode 补丁与来源记录](../vendor/avif-decode/TINYIMG-PATCHES.md) 记录官方 crates.io 归档及 SHA-256、保留源码范围和两处改动：关闭 x86 assembly、将解码器线程数限制为 1。原始 BSD 许可证随源码保留；没有用模拟解码或原图自比较替代验证。AVIF 编码及质量验证以全局互斥锁串行执行，输入上限为 400 万像素，8 位 SDR 输入编码为 10 位 4:4:4 AVIF，解码至完整 u16 范围后舍入回现有 RGBA8 评分域。

Rust 最低版本提升至 1.98，来自 avif-decode 3.0.0 的真实 MSRV 要求。外部 AVIF 输入仍明确拒绝：上游 rav1d 1.1.0 存在损坏 AV1 码流触发进程终止的已知路径，不能通过 catch_unwind 防护；此处解码器仅处理当前进程刚编码的候选，不应复用来读取缓存中的任意文件。
