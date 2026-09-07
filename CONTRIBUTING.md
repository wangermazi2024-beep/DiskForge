# 贡献指南

感谢考虑为 DiskForge WMS 做贡献！这是一个 Windows 平台的 NTFS 磁盘空间分析器，
用 Rust + egui 构建。下面是一些让协作更顺滑的约定。

## 环境

- Rust stable（2024 edition；用 `rustup update stable` 保持最新）
- Windows 10/11 上原生开发最省事；Linux/macOS 上也可以用
  `cargo check --target x86_64-pc-windows-gnu` 做交叉类型检查，
  但涉及 Win32 API 的行为只能在真实 Windows 上验证
- 纯逻辑测试（不依赖 GUI/Win32）在任意平台跑：`cargo test --no-default-features`

## 提交前检查

CI 会用下面三条守住主干，本地先跑一遍可以省一轮往返：

```bash
cargo check --all-targets --target x86_64-pc-windows-gnu
cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo test --no-default-features
```

- **clippy 零警告是硬性要求**。如果某个警告在你的场景里确实是误报，
  在那一行加 `#[allow(...)]` 并用一句话说明理由——我们接受"有解释的例外"，
  不接受无解释的全局放宽。
- 改动公共行为（UI 效果、扫描结果、导出格式）时请同步更新 `README.md`
  和 `CHANGELOG.md`。

## 代码风格约定

这个项目有几个刻意坚持的原则，新代码请保持一致（老代码就是按这些原则
一步步改出来的，注释里通常写了"为什么"）：

1. **后台任务必须有终结保证**：任何后台线程/线程池任务，panic 或提前
   退出都必须保证 UI 端能收到终结消息（Done/Failed/通道断开兜底），不允许
   "转圈转到天荒地老"的路径。
2. **不做静默降级**：扫描/枚举/导出这类"产出结果"的路径，失败就明确报错
   或回退到正确但慢的方案；绝不把部分结果冒充完整结果返回。
3. **UI 线程每帧路径零浪费**：每帧都要跑的代码（列表渲染、查找输入）不
   分配可以不分配的东西；大数据遍历/哈希/导出全部扔后台线程，UI 线程上
   只保留分帧构建（每帧几毫秒预算）这一种形态。
4. **显式栈代替原生递归**：所有遍历用户目录树/索引的函数都用显式栈，
   栈深度与数据深度无关（默认线程 2MB 栈、扫描线程 64MB 栈都不该被
   "目录太深" 打穿）。
5. **常量唯一定义点**：颜色进 `theme.rs`，文件属性位进 `fs_attrs.rs`，
   其余魔数在所属模块顶部命名；跨 crate 复用的用户可见文案集中放置，
   为将来的 i18n 留路。
6. **注释写"为什么"，不写"是什么"**：踩过的坑（explorer /select 的引号、
   `FileTimeToLocalFileTime` 的夏令时陷阱、NO_BUFFERING 的对齐要求……）
   请把出处和推理留在注释里，这是本项目最值钱的部分。

## 提交 PR

- 一个 PR 做一件事；重构、修 bug、改 UI 分开提。
- commit message 用一句话说清"改了什么、为什么"。
- 涉及数据安全的改动（删除/迁移/覆盖）必须在 PR 描述里写清楚失败路径上
  用户数据如何保全。

## 报告问题

提交 issue 时请附上 `diskforge_log.txt`（程序目录下），里面有序列号级的
时间戳、扫描统计和 panic 记录，90% 的问题靠它就能定位。涉及的磁盘/目录
规模（条目数、树深度）对复现性能问题尤其重要。
