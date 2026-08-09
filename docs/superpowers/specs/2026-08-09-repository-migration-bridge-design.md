# 仓库改名与旧版本更新桥接设计

## 目标

将主开发仓库迁移到全新的公开仓库 `osen77/cc-session`，命令和二进制继续使用 `ccs`，同时保证已安装且硬编码旧地址 `osen77/claude-code-sync-cn` 的版本仍可通过 `ccs update` 自动迁移。

## 约束

- 新仓库必须从当前脱敏后的工作树创建全新根提交，不继承旧 Git 历史。
- 旧仓库不得删除或改名；它作为长期兼容入口继续占用 `osen77/claude-code-sync-cn`。
- 旧仓库不再日常开发，但必须长期保留桥接 Release、安装脚本和兼容说明。
- 命令、可执行文件、安装目录、hooks 和 wrapper 继续使用 `ccs`，不引入命令重命名。
- Release 资产名称保持不变，兼容现有 updater：
  - `ccs-linux-x86_64.tar.gz`
  - `ccs-linux-x86_64-musl.tar.gz`
  - `ccs-macos-x86_64.tar.gz`
  - `ccs-macos-aarch64.tar.gz`
  - `ccs-windows-x86_64.zip`

## 现状

`src/handlers/update.rs`、`install.sh`、`install.ps1`、README 和用户指南均硬编码 `osen77/claude-code-sync-cn`。旧版本通过旧仓库的 GitHub latest release API 获取版本，再从旧仓库 Release 下载资产。如果旧仓库被删除，所有尚未升级的安装都会失去自动更新入口。

当前最新版为 `v0.5.2`，迁移桥接版使用 `v0.5.3`。

## 仓库架构

### 新主仓库：`osen77/cc-session`

- 从脱敏工作树创建单个全新初始提交。
- 承担 `v0.5.3` 之后的源码、Issues、Actions、文档和 Releases。
- updater 的主仓库常量改为 `osen77/cc-session`。
- README、用户指南、安装脚本、release 脚本和状态徽章全部使用新地址。
- Release workflow 继续通过 `${{ github.repository }}` 生成当前仓库下载 URL。

### 旧兼容仓库：`osen77/claude-code-sync-cn`

- 保留原名称，避免旧二进制和旧安装 URL 失效。
- 主分支压缩为一个新的兼容快照提交，包含 `v0.5.3` 桥接源码、安装脚本和迁移 README，不保留日常开发历史。
- 发布与新仓库内容相同的 `v0.5.3` Release 和全部平台资产。
- README 首屏声明主项目已迁移到 `https://github.com/osen77/cc-session`。
- `install.sh` 和 `install.ps1` 保留在旧 URL，但脚本内部从新仓库获取 latest release 和资产。
- 验证完成后可以 Archive，但不能删除；Release、raw 安装脚本和仓库内容必须继续公开可读。

## 更新迁移流程

```text
旧版 ccs <= v0.5.2
  -> 查询旧仓库 releases/latest
  -> 发现并下载旧仓库 v0.5.3 桥接资产
  -> 安装后的 v0.5.3 内部指向 osen77/cc-session
  -> 后续检查和下载全部来自新仓库 v0.5.4+
```

旧仓库的 `v0.5.3` 必须永久保留。用户即使数月后才运行 `ccs update`，仍能先升级到桥接版，再进入新仓库更新链。

## Updater 设计

在 `src/handlers/update.rs` 中将仓库地址集中为明确的发布配置，避免 API 路径和下载 URL 再次散落：

```rust
const RELEASE_REPO: &str = "osen77/cc-session";
```

以下路径统一由该常量生成：

- `repos/{repo}/releases/latest`
- `https://api.github.com/repos/{repo}/releases/latest`
- `https://github.com/{repo}/releases/download/{version}/{asset}`

新增纯 URL 构造函数和单元测试，断言桥接版只使用新仓库。旧仓库兼容由旧版本自身的硬编码地址和旧仓库 `v0.5.3` Release 完成，新二进制不需要继续查询旧仓库。

## 安装脚本

新旧两个仓库的安装脚本均将下载源设为 `osen77/cc-session`。

`install.ps1` 同时修正当前与 Release 资产不一致的问题：下载 `ccs-windows-x86_64.zip`，解压得到 `ccs.exe` 后安装，而不是请求不存在的 `ccs-windows-x64.exe`。

旧 URL 因旧仓库继续存在而保持可用：

```text
https://raw.githubusercontent.com/osen77/claude-code-sync-cn/master/install.sh
https://raw.githubusercontent.com/osen77/claude-code-sync-cn/master/install.ps1
```

## Release 发布顺序

1. 保持旧仓库和现有 `v0.5.2` Release 在线。
2. 完成 updater、安装脚本、文档和测试修改，将版本提升到 `v0.5.3`。
3. 从当前脱敏工作树创建新仓库 `osen77/cc-session` 的全新根提交。
4. 在新仓库创建并发布 `v0.5.3`，确认全部平台资产和 SHA256 文件齐全。
5. 将同一批 `v0.5.3` 资产发布到旧仓库，使旧仓库 latest release 变为 `v0.5.3`。
6. 使用真实旧版 `v0.5.2` 二进制在临时目录执行 `ccs update`，确认从旧仓库升级成功。
7. 验证升级后的 `v0.5.3` URL 构造和更新检查均指向 `osen77/cc-session`。
8. 将旧仓库主分支压缩为单个兼容快照提交，并 force-push；保留 `v0.5.3` Release。
9. 更新 GitHub Support 清理请求，包含最终历史重写后的旧 commits 和缓存 URL。
10. 发布首个仅存在于新仓库的正常版本 `v0.5.4`，确认桥接版能够发现并安装。
11. 验证完成后 Archive 旧仓库，长期保留，不删除。

## 失败与回滚

- 新仓库 `v0.5.3` 未验证前，不修改旧仓库 latest release。
- 旧仓库 `v0.5.3` 资产上传不完整时，不压缩或 Archive 旧仓库。
- 桥接实测失败时，旧仓库继续保留 `v0.5.2`，修复后重新构建 `v0.5.3`。
- 旧仓库分支重写前创建权限受限的本地 bundle；确认新旧更新链通过后删除 bundle。
- 不对新仓库和旧仓库使用不同内容但相同版本号的二进制资产；两边 `v0.5.3` 必须逐文件 SHA256 一致。

## 验证

- URL 构造单元测试覆盖 API、Release 下载和所有平台资产名称。
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --all-targets` 全部通过。
- 新旧仓库 `v0.5.3` 资产名称、大小和 SHA256 一致。
- 真实 `v0.5.2 -> v0.5.3` 自更新在 macOS Apple Silicon 上通过，并至少验证 Linux/Windows 资产可下载解压。
- 旧 raw 安装脚本 URL 能成功安装新仓库版本。
- 新仓库 `v0.5.4` 发布后，`v0.5.3` 能发现并更新；`v0.5.2` 仍能通过旧仓库先迁移到 `v0.5.3`。

## 用户影响

- 已安装用户无需修改配置、hooks、wrapper 或命令名。
- 已运行过桥接更新的用户自动切换到新仓库。
- 从未运行更新的旧安装仍可依赖旧仓库长期保留的 `v0.5.3` 完成迁移。
- 源码用户访问旧仓库时会看到迁移说明，并可从兼容快照构建桥接版。
