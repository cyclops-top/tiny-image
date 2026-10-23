# tinyimg

**离线、目标质量驱动的图片优化命令行工具，优先面向 Android/App 资源图。**

给出质量目标，tinyimg 在 PNG、WebP 和可选 JPEG/AVIF 候选中搜索并验证，保留满足规则且更小的结果。核心使用真实 SSIMULACRA2，不用 PSNR 或文件大小冒充感知评分。它不是 TinyPNG 服务的客户端，也不上传图片。

> 开发预览 0.2.0。预设尚未经过真实素材的人眼校准，不能保证“肉眼无损”。默认原地写回会替换原图，转格式会删除旧文件。先用 `--dry-run`、`analyze` 或 `-o`，并保留版本控制/备份。

[源码](https://github.com/cyclops-top/tiny-image) · [文件安全](docs/SAFETY.md) · [参与开发](CONTRIBUTING.md) · [依赖与许可](docs/DEPENDENCIES.md) · [更新记录](CHANGELOG.md)

## 当前支持

- `tinyimg-core` 库与 `tinyimg` CLI；离线批处理和有界图片级并行。
- PNG 无损优化（oxipng 10.2.1）、PNG 量化（imagequant 4.4.1）。
- WebP 无损/有损（webp 0.3.1），显式启用的 JPEG（MozJPEG 0.10.13，仅不透明图）。
- 可选 AVIF（ravif 0.13.0）：本进程生成的候选由真实 AV1 解码器回验；限 SDR、400 万像素、串行处理，不要求 NASM。
- 已处理输出的持久标记：内容哈希、有效设置与编码器版本完全匹配时跳过重复处理。
- SSIMULACRA2 0.5.1；透明图分别合成到黑、白底，取较低分。每个最终候选重新解码验收。
- Android 资源密度分组及限定符信息；各密度独立处理。nine-patch、启动图标、XML、动画及其他不适用输入保守跳过。
- 默认尝试 PNG/WebP；`assets` 路径保留格式；冲突目标不覆盖。改名清单及写死文件名的引用警告，不自动改源码。
- `analyze`、`refs`、`init`、dry-run、CI 检查、输出目录、非覆盖备份、TOML 配置和 JSON 报告。

当前不支持外部 AVIF 输入、GIF/SVG/视频/动画处理、自动缩放、GUI、完整候选结果缓存或完整 ICC/EXIF 色彩/方向转换。AVIF 是输出候选，不是通用 AVIF 转换器；为避开上游已知恶意输入崩溃路径，不解码用户提供的任意 AVIF。带复杂色彩/方向/高位深信息的图片会限制为安全无损路径或跳过。

没有已发布的 crates.io 包、Homebrew 配方或安装包承诺。请从源码构建。默认 CI 只验证 Linux；macOS/Windows 保留可选手动验证入口，留待目标系统编译检查，不宣称已经实测通过。

## 从源码构建

需要 Rust **1.98+**、Cargo，以及平台原生 C/C++ 工具链。WebP、MozJPEG、libdeflate 会从依赖源码编译。使用 Makefile 还需要 **GNU Make**，Windows 可在装有 GNU Make 的开发者终端中运行，不能用 `nmake` 替代。

- Linux：GCC 或 Clang、常规开发头文件和链接器。
- macOS：Xcode Command Line Tools、对应 macOS SDK。
- Windows：MSVC Build Tools 的 C++ 工作负载、Windows SDK，推荐 Developer PowerShell。

```sh
git clone https://github.com/cyclops-top/tiny-image.git
cd tiny-image
make release
# 不使用 Make 的等价命令：
cargo build --locked --release -p tinyimg-cli

# 本机运行；Windows 的文件名为 tinyimg.exe
./target/release/tinyimg --help

# 可选：仅从当前源码安装，不依赖同名 crates.io 包
cargo install --locked --path crates/tinyimg-cli
```

### 多平台 Makefile

默认 `make build` / `make release` 使用本机 Rust host target。指定 `TARGET` 后产物位于 `target/<triple>/debug/` 或 `target/<triple>/release/`。

```sh
make check
make test
make build
make release

make build-linux       # x86_64-unknown-linux-gnu
make build-macos       # aarch64-apple-darwin
make build-windows     # x86_64-pc-windows-msvc

# Intel Mac、Linux ARM64 或 Windows GNU 目标可显式指定：
make release TARGET=x86_64-apple-darwin
make build-linux LINUX_TARGET=aarch64-unknown-linux-gnu
make build-windows WINDOWS_TARGET=x86_64-pc-windows-gnu

# 只安装目标 Rust 标准库，不安装编译器、链接器或 SDK：
make target-install TARGET=aarch64-unknown-linux-gnu
```

`build-*` 是真实 Cargo target 构建入口，不是跨平台模拟器，也不会偷偷把失败目标替换成本机产物。推荐在对应系统原生构建；在其他系统交叉编译时，需要自行配置目标 C 工具链、链接器、sysroot/SDK，以及必要的 `CC_<target>`、`AR_<target>`、`CARGO_TARGET_<TRIPLE>_LINKER`。macOS 目标需要可合法使用的 Apple SDK；Linux 上安装 Rust Windows target 并不等于具备 MSVC 环境。交叉运行 `make test TARGET=...` 还需要相应 runner。

默认 CI 在 Linux 原生 runner 执行格式、Clippy、测试、release 构建与 CLI smoke test。手动触发 CI 时可选 macOS、Windows 或两者，不是默认必需检查。手动 `Build review artifacts` 工作流默认也只构建 Linux，可显式选择其他原生平台；它只生成供审查的构建文件，不发布 crate 或 GitHub Release。

### Windows、macOS 与 Linux 路径

文件系统路径使用 Rust 的原生 `Path` / `PathBuf` 拼接，不通过拼接 `/` 或硬编码 Unix 根路径构造。带空格或中文的路径需要用当前 shell 的引号保护；Windows 原生路径与配置 glob 是两个不同概念。

```powershell
# Windows PowerShell：反斜杠是原生路径分隔符
.\target\release\tinyimg.exe '.\图片 资源\res' --dry-run
.\target\release\tinyimg.exe '.\图片 资源\res' -o '.\优化 输出'
```

```sh
# macOS / Linux：斜杠是原生路径分隔符
./target/release/tinyimg './图片 资源/res' --dry-run
./target/release/tinyimg './图片 资源/res' -o './优化 输出'
```

`tinyimg.toml` 中 glob 统一使用 `/`，例如 `drawable*/**` 或 `**/vendor/**`，不要写成 Windows 反斜杠路径；匹配路径在代码中归一化。TOML 中也不要把反斜杠误当作未转义的普通字符串内容。

盘符、UNC、大小写碰撞、只读属性和原子替换都有平台差异。当前仅要求实际验证 Linux；Windows/macOS 侧使用原生路径与编译依赖设计，并保留平台测试入口，但这里不声称已经在 Windows 或 macOS 运行验证。首次在目标系统使用，仍应先执行 `make test`、`--dry-run` 和 `-o` 抽查。

## 命令帮助

```sh
tinyimg -h                    # 简要参数说明
tinyimg --help                # 完整参数、默认值、安全限制、退出码和示例
tinyimg analyze --help
tinyimg refs --help
tinyimg init --help
```

目录默认递归扫描，不需要 `-r` 或 `--recursive`；不传路径时使用当前目录。带空格的路径请加引号，例如 `tinyimg "./图片 资源" --dry-run`。帮助中列出的是内置默认值，配置文件和显式命令行参数仍按原有优先级生效。

## 建议的第一轮使用

```sh
# 只分析，不创建任何文件；显示每张图测试过的候选
tinyimg analyze app/src/main/res

# 看计划，不写回
tinyimg app/src/main/res --dry-run

# 保留输入，把有收益的结果写到独立目录
tinyimg app/src/main/res -o optimized-res/

# 人工抽查之后，原地写回并保存原始文件备份
tinyimg app/src/main/res --backup original-res/
```

`-o` **只输出优化成功的文件**，不复制跳过或无收益项，不是可直接替换原项目的完整镜像。备份保持输入相对路径，已有备份不会被覆盖；`-o` 和 `--backup` 互斥。备份/输出最好放在输入目录之外。

不传路径时，默认处理当前目录。单目录输入保留其相对层级；多目录输入增加各根目录名称前缀。无法无歧义映射到输出路径的输入会报错，不会静默合并。

只要输入中包含目录（包括默认当前目录、文件与目录混合输入），原地写入前会先扫描筛选，显示符合条件的图片数量，再询问 `Continue in place? [y/N]`。只有输入 `y` 或 `yes`（不区分大小写）才继续；`n`、直接回车、其他内容或 stdin 结束均取消，退出码为 0，不修改图片、缓存、备份或报告文件。`--quiet` 不会隐藏安全确认。指定 `--output` / `-o`、只读模式、仅显式文件输入或筛选后无符合条件图片时不询问。

数量按 include/exclude、支持的格式、受保护资源规则及有效 `min_bytes` 筛选；同时显示已确定跳过的扫描项数（不包含被目录过滤规则直接排除的项）；不保证每张最终都能优化，解码、已处理标记和质量/收益校验仍可能跳过。确认后沿用同一份文件清单，不把等待期间新加入的图片纳入处理。提示写到 stderr，不污染 JSON stdout；`--report json` 取消时输出包含 `cancelled: true`、`eligible_images` 和 `known_skipped` 的 JSON 对象，不创建 `--report-file`。

## 质量与格式

```sh
tinyimg res --preset high --dry-run
tinyimg res --score 90 --keep-format --dry-run
tinyimg res --formats png --dry-run
tinyimg photos --formats png,webp,jpeg --score 85 -o optimized/
tinyimg a.png b.png --min-bytes 0 --dry-run
```

| 预设 | SSIMULACRA2 目标 | 状态 |
| --- | ---: | --- |
| high | 88 | 实验值，优先用于人工抽查 |
| balanced | 80 | 默认，实验值 |
| small | 70 | 实验值，允许更多失真 |

`--score 0..100` 覆盖同层预设。算法按编码器选择起点（PNG 量化 90、WebP 80、JPEG 85、AVIF 70），用实测分数建立区间并动态插值，只有预测无效、重复或停滞时才二分。PNG/WebP/JPEG 最多各试 8 次，AVIF 最多 6 次；这些起点不是质量保证，也不是不可突破的最低质量。编码器分数不保证单调，不宣称找到全局最小文件。PNG 量化未达标候选跳过无损重压缩（其 JSON size 是重压缩前大小）；合格候选仍重压缩。最多为 100 万像素输入复用参考图的线性光合成数据，每个图片工作线程额外缓存不超过 24 MB；最终验证仍重新解码实际输出并独立计算分数。无损候选以像素一致性验收，报告可能不显示感知分数；这不等于没有校验。

默认 `--formats png,webp`。`--keep-format` 限制为原格式，但格式必须也在允许的候选集中。所有保存结果必须比原图小。`--min-saving PCT` 默认 10：PNG 转格式时，相对可用的最优 PNG 候选（没有则相对原 PNG）至少节省该百分比。各格式的 Android/浏览器兼容性由使用者确认，工具不推断 minSdk。

默认跳过小于 2048 字节的文件，可用 `--min-bytes` 调整。输入文件上限 256 MiB，图片最大边 8000、总像素上限 1600 万；这些是处理保护，不是对恶意输入的完整沙箱。默认工作线程数为 `min(4, CPU 并行度)`；`--jobs 1..64` 可调整，增大并行度会增加内存占用。

## 配置

```sh
tinyimg init                         # 新建 tinyimg.toml，不覆盖已有文件
tinyimg res --config config/tinyimg.toml --dry-run
```

自动发现从第一个输入路径向上寻找 `tinyimg.toml`。优先级从低到高：内置默认 → 顶层配置 → 按顺序匹配的 `[[overrides]]` → 命令行。覆盖层单独设置 `preset` 会清除低层 `score`；同一层同时给出 `score` 时以 `score` 为准。

```toml
preset = "balanced"
formats = ["png", "webp"]
min_saving = 10
min_bytes = 2048
exclude = ["**/vendor/**"]

[[overrides]]
glob = "drawable*/**"
score = 88
keep_format = true
```

glob 相对扫描输入根路径，而不是配置文件所在目录。示例对应 `tinyimg app/src/main/res`。图片级覆盖用于质量、格式、元数据策略与最小大小；include/exclude 只允许写在顶层，放入覆盖块会报配置错误。glob 统一用 `/` 分隔，Windows 也一样。完整示例见 [examples/tinyimg.toml](examples/tinyimg.toml)。

## 报告与 CI

```sh
tinyimg res --dry-run --report json
tinyimg analyze res --report json
tinyimg res --check
tinyimg refs . --report json

# 写入模式可额外写报告文件；该路径必须尚不存在
tinyimg res -o optimized/ --report json --report-file tinyimg-report.json
```

`--dry-run`、`analyze`、`--check` 严格不落盘，包括不写报告、缓存、备份或输出目录；与 `--report-file` 组合会报参数错误。若使用 shell 重定向，创建报告文件的是 shell。`--no-cache` 完全绕过已处理标记的读写；`analyze` 总是重新分析。默认原地写回成功后在所选输入根目录（单文件为其父目录）写 `.tinyimg-cache`，`-o` 只在输出根目录写标记，不修改输入树。标记不包含图片或质量分数，不是对旧候选字节的信任。损坏/不可读标记只报警并重新计算。

| 退出码 | 含义 |
| --- | --- |
| 0 | 完成；跳过不支持或不安全图片不等于运行错误 |
| 1 | 运行/读写错误；可能有其他文件已成功处理，需看逐图报告 |
| 2 | 参数或配置错误 |
| 3 | `--check` 找到可执行的优化项 |

交互终端为正在处理的图片显示独立动画行：文件名、实际解码/编码尝试/质量验证/写入阶段及耗时；单张图片同样可见。编码器内部没有精确百分比，因此使用持续转动的指示符，不虚构单图百分比。完成的行从活动区移除并留下结果记录，底部显示已处理/总数、活动数及总耗时。默认最多同时处理 4 张，活动区随终端高度最多显示 8 行，额外任务计入活动数且仍并发执行。跳过和失败也计入已处理。进度仅写入交互终端 stderr；JSON、`-q`、不支持的/过小的终端或重定向 stdout/stderr 时禁用，目录确认前不会启动。

文本结果与总计使用一位小数的 B/KB/MB/GB（十进制，1 KB = 1000 B），显示原始大小 → 结果大小，以及节省百分比和节省大小。例如 `1.5 MB -> 820.4 KB, saved 45.3% (679.6 KB)`；百分比按未舍入字节数计算，JSON 中的大小仍为精确整数字节。UTF-8 终端使用流动点阵动画，旧终端或非 UTF-8 环境回退为 ASCII 动画。交互结果按状态、编号、文件名和大小/节省列对齐（按 Unicode 显示宽度处理中文），窄终端改为紧凑多行。成功为绿色、跳过为黄色、错误为红色，辅助字段为灰色；`NO_COLOR` 禁用颜色，`TERM=dumb` 或重定向输出不显示 ANSI 动画或颜色。

预设未校准的说明保留在帮助、JSON 和 `-v` 中，不再每次打印到普通文本报告；实际错误和操作相关警告仍然显示。

文本报告列出逐图结果、跳过原因和引用警告；`analyze` 或 `-v` 显示候选细节。JSON 包括 `files`、`summary`、`renames`、`reference_warnings`、`presets_calibrated: false` 和 `cache_enabled`、`summary.cache_hits`。`-q` 抑制普通文本输出，JSON 仍输出。

引用检查只读取支持的 UTF-8 文本文件，单文件上限 2 MiB；不修改源码，不是静态分析器。优化后的引用检查最多向上查找 8 层，在最近的 AndroidManifest.xml 所在目录或 Git 边界停止，以覆盖资源目录旁的源码和 assets；找不到边界则只查输入目录。`tinyimg refs .` 可显式检查更大的项目范围。动态路径、外部文件和二进制引用仍可能遗漏。

## 安全、元数据与当前边界

- 原地操作在非 Git 工作区或未提交工作区打印提醒，不阻塞操作；Git 提醒不是备份。
- 同路径替换与跨扩展名转换不同：后者是先写新文件、后删旧文件，不是跨文件原子事务。错误报告会提示需检查的残留文件。
- 默认仅在安全路径上处理元数据。ICC/色度信息、EXIF 方向与 16 位输入受保守限制，不做未经验证的 8 位降级或色彩转换。
- `--keep-metadata` 当前仅允许 PNG 无损优化；其他输入保留原图。`--strip-all` 不会强行绕过方向/色彩保护。
- 符号链接不会跟随；已存在的目标与备份不会静默覆盖。文件系统竞态、掉电及元数据复制边界见 [SAFETY.md](docs/SAFETY.md)。

尚待完成：真实素材质量校准、全平台运行记录、性能基准、完整候选结果复用、外部 AVIF 安全导入及经过审查的发布流程。

## English summary

tinyimg is an offline Rust image optimizer for Android/App resources. It searches PNG, WebP and optional JPEG/AVIF candidates against real SSIMULACRA2 scores, then decodes and validates the final bytes. This is an experimental source release: AVIF output is genuinely decoded and verified, arbitrary AVIF import is deliberately disabled, processed-output markers avoid same-settings repeat work, presets remain uncalibrated, and complex color/orientation metadata is handled conservatively.

Default operation modifies originals and may change extensions. Start with `tinyimg analyze PATH`, `--dry-run`, or `-o OUT`; output directories contain optimized files only, not a complete mirror. `make release` builds natively; `TARGET=...` selects a real Rust target and requires the corresponding C toolchain/linker/SDK. No public package publication is implied.

## 许可证

GPL-3.0-or-later，见 [LICENSE](LICENSE)。本项目依赖的第三方组件另有各自许可，声明与生成完整清单的流程见 [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md)。

非 Unicode 文件名也不会让 JSON 报告在写回后失败：正常路径是 JSON 字符串，无法表示为 Unicode 的路径改为带 `encoding`、`hex`、`display` 的对象，完整保存操作系统原始路径。详见 [安全说明](docs/SAFETY.md)。显式指定的输出/备份根目录会先解析系统路径别名（例如 macOS `/var`）；该根目录下面的符号链接仍会被拒绝。

本次 Linux 编译与完整测试记录见 [验证记录](docs/VALIDATION.md)。默认标记可避免同设置重复处理；更改质量设置、使用 `--no-cache`、丢失标记或移动输出后仍可能再次有损压缩。建议保留原图并从原图使用 `-o` 输出。
