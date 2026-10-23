# 文件、色彩与质量安全

## 开始前

**默认运行会原地替换文件；跨格式转换成功后会删除旧扩展名的源文件。** 首次运行应使用 `--dry-run` 或 `analyze`。正式处理前提交版本控制，或使用独立备份。推荐先用 `-o` 生成待审查结果，再在实际应用、不同密度和深浅主题下检查。

程序会尽力避免破坏源文件，但任何程序都无法保证在硬件故障、进程被杀、并发修改、恶意目录替换或文件系统异常时绝对安全。没有经过恢复演练的备份不应被当作唯一保障。

## 只读模式

`--dry-run`、`analyze`、`--check` 只在内存中编码、评分和组织报告，不写图片、备份、临时文件、输出目录或缓存，也不写 `--report-file`。在这些模式下指定文件报告会被拒绝；使用 stdout 即可查看结果。

Shell 的 `> report.json` 重定向由 shell 创建文件，不属于 tinyimg 的写入。若需要“整个命令绝不创建文件”，不要使用重定向。

`--check` 会执行真实候选评估，不是只检查扩展名或猜测文件是否能压缩。发现可优化项的退出码为 3，错误不应被 CI 当作“没有可优化项”。

## 输出验收

保存候选前重新解码并检查尺寸、透明度、大小和质量。只选择小于原图且满足当前质量目标的结果。格式转换还需满足收益门槛；原图始终是无损保底选择。

SSIMULACRA2 是数值代理，不是“人眼无差别”的保证。针对文字、锐利边缘和小图标尤其需要人工检查。透明图在黑底和白底上分别比较，取更差的分数；这仍不覆盖所有应用背景与混合模式。隐藏在全透明像素下的 RGB 值不作为视觉承诺。

high/balanced/small = 88/80/70 尚未在真实 Android 素材上完成校准。也没有承诺每张图都能压缩、全局最优编码参数、固定搜索次数或固定速度。

## 写入与恢复

### 同扩展名

在目标目录创建临时文件，写完并同步后使用文件系统替换机制提交。原子替换是单文件层面的保证，不是整批事务，也不等于在所有断电场景下都完全持久化。源内容会在提交前再次校验，检测到扫描后修改则中止该文件的写入。文件权限位会保留，但所有者、ACL、扩展属性和时间戳不是保留承诺。

### 扩展名变化

先以不覆盖已有目标的方式提交新文件，确认源仍与读取时一致，然后删除旧文件。这两步**不是跨文件原子操作**。删除阶段失败可能需要清理新文件；若清理也失败，两份文件可能同时存在。Android 同目录同资源名的多个扩展名会造成构建冲突，应依据错误报告检查目录，不要直接继续批量覆盖。

不会为了“看起来成功”而静默覆盖已存在的同名目标。对大小写不敏感文件系统，需要让对应平台 CI 实际验证冲突行为，不能只依赖 Linux 测试。

### 备份与输出目录

- `--backup DIR` 在原地写入前保存原始字节；已有备份不能被新运行悄悄覆盖。若备份位置冲突或写入失败，该文件不能继续破坏性写回。
- `-o DIR` 不删除输入；只输出真正可优化的图片，**不会复制跳过或无收益文件**，因此输出目录不是完整项目镜像。
- 将备份与输出放在输入目录之外，避免下一次扫描重复处理；不要让其他程序同时修改输入、输出或备份目录。
- 失败后先看逐图错误和实际文件，再决定恢复。tinyimg 没有自动回滚整批、撤销命令或恢复日志数据库。

## 元数据与色彩边界

安全优先于强行压缩。当前不是完整 ICC 色彩管理或 EXIF 方向归一化实现。带 ICC/EXIF、16 位高精度或其他未安全处理语义的输入会保守跳过有损/转格式路径，必要时保持原图。不要把 `--strip-all` 理解为“已经先做了正确色彩转换和旋转”。

`--keep-metadata` 也不是跨所有格式逐字段转移元数据的承诺。为保全不理解的元数据，可用候选可能减少。当前 PNG 无损路径默认保留非 EXIF 的其他块；带 EXIF 的输入为避免方向变化会提前跳过。量化/转格式路径不承诺复制普通文本元数据；若无法安全处理显示语义，则宁可跳过。最终应用显示效果应以原图和结果在正确色彩管理的查看器中的比较为准。

JPEG 不用于含透明像素的图片。动画、nine-patch、启动图标、符号链接、损坏文件及超出安全处理范围的输入按扫描/验证规则跳过并报告。不进行缩放，不修复 Android XML 或源码引用。

## 引用检查的能力边界

引用检查针对写死的文件名字符串输出候选警告，不解析整个构建图，也不证明所有引用都安全。优化后扫描最多向上查找 8 层，在最近的 Android manifest 目录或 Git 边界停止；找不到边界则扫描输入目录。仅检查支持的 UTF-8 文本类型，单文件最大 2 MiB。动态拼接、压缩文本、生成代码、二进制、外部引用或运行时下载路径可能遗漏；注释或无关同名文本也可能误报。改名后应运行 Android 构建和相关界面/资源测试。

## 运行环境与资源

图片编码和评分可能消耗较多 CPU/内存。`--jobs` 增大并行度会提高同时在内存中的图片数量。输入文件上限 256 MiB、最大边 8000、1600 万像素上限与常规解码错误处理不能代替对恶意输入的安全沙箱；不要用高权限身份处理不可信的图片集合。

应用运行无需联网；从源码构建及安装依赖通常需要网络。当前没有增量缓存，因此重复分析会重新编码。错误输入会在报告中呈现，批量任务的最终退出码仍需检查。

## Native paths and selected roots

Filesystem operations use native `Path`/`PathBuf`, not string concatenation. Explicit output/backup roots and the report parent resolve their existing ancestors once, so macOS system aliases such as `/var` and `/tmp` work. The resolved root is frozen before scanning and writing. Symlinks/reparse points encountered beneath that selected output tree are rejected. Source images that are symlinks are not followed.

Unicode paths remain strings in JSON. Non-Unicode paths use an object containing `encoding`, `hex` and a lossy `display` for humans: `unix-bytes-hex` stores every original byte, while `windows-utf16-hex` stores each original 16-bit code unit as four hexadecimal digits. The raw fields, not `display`, preserve identity. Android group labels similarly use a raw encoding for non-Unicode absolute roots. Filesystem decisions never use those display labels.

`--report-file` is checked before image processing: existing paths are never overwritten, its parent must exist, and image filename extensions are rejected. A later I/O or race can still make report creation fail after images have been processed; exit 1 does not imply the entire batch was rolled back.

## Repeated lossy processing

This baseline has no persistent processed-image marker or incremental cache. Running it repeatedly can recompress a prior lossy result relative to that result and accumulate quality loss. Retain originals, prefer `-o` into a fresh directory, and rerun from the originals rather than repeatedly applying in-place lossy processing. `--check` and `analyze` measure the supplied files, not an unavailable original generation.
