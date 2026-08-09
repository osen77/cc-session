# Repository Migration Bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将主开发仓库迁移到全新历史的 `osen77/cc-session`，通过旧仓库永久保留的 `v0.5.3` 桥接版本，让所有 `ccs <= v0.5.2` 安装继续自动更新。

**Architecture:** 先在当前已脱敏、已提交的工作树上完成 updater、安装脚本、文档和 `v0.5.3` 版本修改，再用最终 Git tree 创建一个不带父提交的新根 commit 并发布到新仓库。新旧仓库发布逐文件一致的 `v0.5.3` 资产；旧仓库随后压缩为单根兼容快照，桥接验证完成后只在新仓库发布 `v0.5.4`。

**Tech Stack:** Rust 2021、Clap、Git/GitHub CLI、GitHub Actions、Bash、PowerShell、GitHub Releases

## Global Constraints

- 新主仓库固定为 `osen77/cc-session`；旧兼容仓库固定为 `osen77/claude-code-sync-cn`。
- 命令、二进制、安装目录、hooks 和 wrapper 继续使用 `ccs`。
- 桥接版本固定为 `v0.5.3`；新仓库独占验证版本固定为 `v0.5.4`。
- 新仓库必须只有一个全新根提交，不得继承旧 Git 历史。
- 当前已提交的 `43a565b697a34cd7ea0ac695c0da0e218ecc732d`（session search、短 ID、JSON 错误输出改进）及执行开始时所有更新的已提交内容必须包含在新仓库初始 tree 中。
- 实施时不得回退 `README.md`、`docs/user-guide.md`、`src/main.rs`、`src/handlers/session.rs` 的现有改动；不得使用 `git add -A`。
- Release 资产名称保持为 `ccs-linux-x86_64.tar.gz`、`ccs-linux-x86_64-musl.tar.gz`、`ccs-macos-x86_64.tar.gz`、`ccs-macos-aarch64.tar.gz`、`ccs-windows-x86_64.zip` 及对应 `.sha256`。
- 新旧仓库的 `v0.5.3` 十个资产必须逐文件大小和 SHA256 一致。
- 旧仓库不能删除或改名；最终只 Archive，Release 和 raw 安装脚本必须继续公开可读。
- 所有 GitHub 创建、发布、force-push 和 Archive 操作前都先执行对应的只读 preflight，并使用精确 refspec；旧仓库重写使用 `--force-with-lease`。
- `local/notes.md` 必须记录迁移过程，但它保持 ignored，不加入提交。

---

## File Map

- `src/handlers/update.rs`：唯一的内置 updater 发布仓库配置、API URL 和资产下载 URL。
- `tests/repository_migration_tests.rs`：安装脚本、文档和 release helper 的仓库地址回归测试。
- `install.sh`：macOS/Linux 安装器，旧 URL 和新 URL 访问到的脚本都从新仓库下载。
- `install.ps1`：Windows ZIP 下载、解压和安装逻辑。
- `README.md`：新主仓库徽章、安装、源码、更新和 Issues 链接。
- `docs/user-guide.md`：新主仓库安装、更新、故障排查和资源链接。
- `scripts/release.sh`：发布完成后的 Actions 地址。
- `Cargo.toml`、`Cargo.lock`：`0.5.3` 和之后 `0.5.4` 版本号。
- `local/notes.md`：本机迁移记录，不提交。
- `local/repository-migration.env`：实施期间保存精确 commit、备份和资产目录，不提交。
- `local/github-support-request.txt`：包含最终重写 SHA 的 GitHub Support 请求正文，不提交。

---

### Task 1: 实现并验证 `v0.5.3` 桥接代码

**Files:**
- Modify: `src/handlers/update.rs:15-16,75-96,128-145,189-197,406-467`
- Create: `tests/repository_migration_tests.rs`
- Modify: `install.sh:1-9,43-45,71-108`
- Modify: `install.ps1:1-10,57-81,102-111`
- Modify: `README.md:1-57,279-284`
- Modify: `docs/user-guide.md:23-72,807-818,867-871`
- Modify: `scripts/release.sh:181-185`
- Modify: `Cargo.toml:1-4`
- Modify: `Cargo.lock:210-213`
- Modify locally only: `local/notes.md`

**Interfaces:**
- Produces: `latest_release_api_path() -> String`
- Produces: `latest_release_api_url() -> String`
- Produces: `release_asset_url(version: &str, asset_name: &str) -> String`
- Produces: `RELEASE_REPO = "osen77/cc-session"`
- Produces: Windows 安装器从 `ccs-windows-x86_64.zip` 解压并安装 `ccs.exe`

- [ ] **Step 1: 固定当前提交基线并确认新提交已纳入**

Run:

```bash
git status --short --branch
git log -3 --oneline --decorate
git merge-base --is-ancestor 43a565b697a34cd7ea0ac695c0da0e218ecc732d HEAD
test -z "$(git status --porcelain)"
```

Expected: `HEAD` 包含 `43a565b`，工作树为空；若其他会话又产生已提交 commit，则继续以新的 `HEAD` 为源，不回退它。

- [ ] **Step 2: 在 updater 中先写 URL 构造失败测试**

在 `src/handlers/update.rs` 的 `tests` 模块加入：

```rust
#[test]
fn test_release_endpoints_use_new_repository() {
    assert_eq!(
        latest_release_api_path(),
        "repos/osen77/cc-session/releases/latest"
    );
    assert_eq!(
        latest_release_api_url(),
        "https://api.github.com/repos/osen77/cc-session/releases/latest"
    );
    assert_eq!(
        release_asset_url("v0.5.3", "ccs-macos-aarch64.tar.gz"),
        "https://github.com/osen77/cc-session/releases/download/v0.5.3/ccs-macos-aarch64.tar.gz"
    );
}
```

- [ ] **Step 3: 创建安装器和文档地址失败测试**

创建 `tests/repository_migration_tests.rs`：

```rust
use std::fs;
use std::path::PathBuf;

const NEW_REPO: &str = "osen77/cc-session";
const OLD_REPO: &str = "osen77/claude-code-sync-cn";

fn repo_file(path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn public_installers_download_from_new_repository() {
    let shell = repo_file("install.sh");
    let powershell = repo_file("install.ps1");

    assert!(shell.contains(&format!("REPO=\"{NEW_REPO}\"")));
    assert!(!shell.contains(&format!("REPO=\"{OLD_REPO}\"")));
    assert!(powershell.contains(&format!("$REPO = \"{NEW_REPO}\"")));
    assert!(powershell.contains("$ASSET_NAME = \"ccs-windows-x86_64.zip\""));
    assert!(powershell.contains("Expand-Archive"));
    assert!(powershell.contains("ccs.exe"));
    assert!(!powershell.contains("ccs-windows-x64.exe"));
}

#[test]
fn main_project_links_use_new_repository() {
    for path in ["README.md", "docs/user-guide.md", "scripts/release.sh"] {
        let content = repo_file(path);
        assert!(content.contains(NEW_REPO), "{path} must link to {NEW_REPO}");
        assert!(
            !content.contains(OLD_REPO),
            "{path} must not retain the legacy repository URL"
        );
    }
}
```

- [ ] **Step 4: 运行测试并确认 RED**

Run:

```bash
cargo test handlers::update::tests::test_release_endpoints_use_new_repository --lib
cargo test --test repository_migration_tests
```

Expected: 第一条因 URL helper 不存在而编译失败；添加 helper 声明后，第二条因脚本和文档仍使用旧仓库而失败。不要通过放宽断言绕过失败。

- [ ] **Step 5: 集中 updater 的仓库和 URL 构造**

将 `src/handlers/update.rs` 的仓库常量与 URL 构造改为：

```rust
/// GitHub repository for releases.
const RELEASE_REPO: &str = "osen77/cc-session";

fn latest_release_api_path() -> String {
    format!("repos/{RELEASE_REPO}/releases/latest")
}

fn latest_release_api_url() -> String {
    format!("https://api.github.com/{}", latest_release_api_path())
}

fn release_asset_url(version: &str, asset_name: &str) -> String {
    format!(
        "https://github.com/{RELEASE_REPO}/releases/download/{version}/{asset_name}"
    )
}
```

`fetch_latest_version()` 和 `check_for_update_silent()` 均调用 `latest_release_api_path()` / `latest_release_api_url()`；`download_and_replace()` 调用：

```rust
let url = release_asset_url(version, &asset_name);
```

- [ ] **Step 6: 修复两个安装脚本**

`install.sh` 使用：

```bash
REPO="osen77/cc-session"
```

并把文件头、Windows 提示中的 raw URL 改为新仓库 `master` 分支。

`install.ps1` 的下载/解压核心改为：

```powershell
$REPO = "osen77/cc-session"
$INSTALL_DIR = "$env:LOCALAPPDATA\Programs\ccs"
$ASSET_NAME = "ccs-windows-x86_64.zip"
$DEST_PATH = "$INSTALL_DIR\ccs.exe"
$TEMP_DIR = Join-Path ([System.IO.Path]::GetTempPath()) ("ccs-install-" + [guid]::NewGuid())
$ARCHIVE_PATH = Join-Path $TEMP_DIR $ASSET_NAME

New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null
New-Item -ItemType Directory -Force -Path $TEMP_DIR | Out-Null

try {
    $DOWNLOAD_URL = "https://github.com/$REPO/releases/download/$LATEST_VERSION/$ASSET_NAME"
    if (Get-Command Start-BitsTransfer -ErrorAction SilentlyContinue) {
        Start-BitsTransfer -Source $DOWNLOAD_URL -Destination $ARCHIVE_PATH -Description "Downloading ccs"
    } else {
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $DOWNLOAD_URL -OutFile $ARCHIVE_PATH -UseBasicParsing
    }
    Expand-Archive -Path $ARCHIVE_PATH -DestinationPath $TEMP_DIR -Force
    $EXTRACTED_BINARY = Join-Path $TEMP_DIR "ccs.exe"
    if (-not (Test-Path $EXTRACTED_BINARY)) {
        throw "ccs.exe was not found in $ASSET_NAME"
    }
    Copy-Item -Path $EXTRACTED_BINARY -Destination $DEST_PATH -Force
} finally {
    if (Test-Path $TEMP_DIR) {
        Remove-Item -Path $TEMP_DIR -Recurse -Force
    }
}
```

保留现有 PATH、版本验证和 setup 流程。

- [ ] **Step 7: 更新新主仓库文档和发布链接**

在 `README.md`、`docs/user-guide.md`、`scripts/release.sh` 中将仓库、Release、raw、clone、Actions 和 Issues URL 全部改为 `osen77/cc-session`；raw URL 使用 `master`。README 标题改为 `# cc-session`，正文继续说明命令名为 `ccs`，不改配置目录名称。

Run:

```bash
rg -n "osen77/claude-code-sync-cn|ccs-windows-x64\.exe" README.md docs/user-guide.md scripts/release.sh install.sh install.ps1 src/handlers/update.rs
```

Expected: 无输出。

- [ ] **Step 8: 将桥接版本提升到 `0.5.3`**

修改：

```toml
# Cargo.toml
version = "0.5.3"
```

然后运行：

```bash
cargo check
cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])'
```

Expected: 输出 `0.5.3`，`Cargo.lock` 中 `claude-code-sync` package 版本同步为 `0.5.3`。

- [ ] **Step 9: 记录本机迁移说明**

在 ignored 的 `local/notes.md` 顶部新增 `2026-08-09：仓库改名与旧版本更新桥接`，按项目规定写明问题描述、根本原因、解决方案、影响范围和预防措施；明确 `local/notes.md` 不得 stage。

- [ ] **Step 10: 运行完整验证**

Run:

```bash
bash -n install.sh
if command -v pwsh >/dev/null 2>&1; then pwsh -NoProfile -Command '$errors=$null; [System.Management.Automation.Language.Parser]::ParseFile("install.ps1", [ref]$null, [ref]$errors) > $null; if ($errors.Count) { $errors | ForEach-Object { Write-Error $_ }; exit 1 }'; fi
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
git diff --check
git diff 43a565b697a34cd7ea0ac695c0da0e218ecc732d -- src/main.rs src/handlers/session.rs
```

Expected: 全部退出码为 0；最后一条无输出，证明迁移没有覆盖另一个会话刚提交的核心实现。

- [ ] **Step 11: 精确提交桥接代码**

Run:

```bash
git add src/handlers/update.rs tests/repository_migration_tests.rs install.sh install.ps1 README.md docs/user-guide.md scripts/release.sh Cargo.toml Cargo.lock
git diff --cached --stat
git diff --cached --check
git commit -m "feat: bridge updates to cc-session"
```

Expected: 提交不包含 `local/`、`src/main.rs`、`src/handlers/session.rs` 或无关文件。

---

### Task 2: 创建全新历史的新仓库并发布 `v0.5.3`

**Files:**
- Create locally only: `local/repository-migration.env`
- GitHub create: `osen77/cc-session`
- GitHub release: `osen77/cc-session@v0.5.3`

**Interfaces:**
- Consumes: Task 1 的已验证 `0.5.3` source tree
- Produces: 单根 commit `NEW_ROOT_COMMIT`
- Produces: 新仓库 `v0.5.3` 十个已校验 Release 资产
- Produces: 权限为 `0600` 的旧仓库 refs bundle

- [ ] **Step 1: 做创建前 preflight 和权限受限备份**

Run:

```bash
test -z "$(git status --porcelain)"
git merge-base --is-ancestor 43a565b697a34cd7ea0ac695c0da0e218ecc732d HEAD
gh repo view osen77/claude-code-sync-cn --json defaultBranchRef,isArchived,url
test "$(gh repo view osen77/claude-code-sync-cn --json defaultBranchRef --jq .defaultBranchRef.name)" = "master"
if gh repo view osen77/cc-session >/dev/null 2>&1; then echo "osen77/cc-session already exists" >&2; exit 1; fi
mkdir -p local
umask 077
BACKUP_BUNDLE="$HOME/claude-code-sync-cn-before-migration-$(git rev-parse --short HEAD).bundle"
git bundle create "$BACKUP_BUNDLE" --all
chmod 600 "$BACKUP_BUNDLE"
git bundle verify "$BACKUP_BUNDLE"
```

Expected: 新仓库不存在；bundle 验证通过。若新仓库已存在，停止而不是覆盖。

- [ ] **Step 2: 从最终 tree 创建无父提交的新根 commit**

Run:

```bash
SOURCE_HEAD=$(git rev-parse HEAD)
SOURCE_TREE=$(git rev-parse HEAD^{tree})
NEW_ROOT_COMMIT=$(printf '%s\n' 'Initial public release of cc-session' | git commit-tree "$SOURCE_TREE")
test "$(git rev-list --parents -n 1 "$NEW_ROOT_COMMIT" | wc -w | tr -d ' ')" = "1"
test "$(git rev-parse "$NEW_ROOT_COMMIT^{tree}")" = "$SOURCE_TREE"
printf 'SOURCE_HEAD=%q\nSOURCE_TREE=%q\nNEW_ROOT_COMMIT=%q\nBACKUP_BUNDLE=%q\n' \
  "$SOURCE_HEAD" "$SOURCE_TREE" "$NEW_ROOT_COMMIT" "$BACKUP_BUNDLE" \
  > local/repository-migration.env
chmod 600 local/repository-migration.env
```

Expected: `NEW_ROOT_COMMIT` 只有自身一个字段，即没有父 commit；tree 与包含最新 `43a565b` 内容的 Task 1 最终 tree 完全一致。

- [ ] **Step 3: 创建新公开仓库并只推送全新根 commit**

Run:

```bash
source local/repository-migration.env
gh repo create osen77/cc-session --public --description "Cross-agent session search and Claude Code history sync CLI"
git remote add cc-session git@github.com:osen77/cc-session.git
git push cc-session "${NEW_ROOT_COMMIT}:refs/heads/master"
gh repo edit osen77/cc-session --default-branch master
test "$(git ls-remote cc-session refs/heads/master | cut -f1)" = "$NEW_ROOT_COMMIT"
test "$(gh api repos/osen77/cc-session/commits --paginate --jq 'length')" = "1"
```

Expected: `master` 指向 `NEW_ROOT_COMMIT`，GitHub API 只返回一个 commit。

- [ ] **Step 4: 在新仓库触发并等待 `v0.5.3` Release**

Run:

```bash
source local/repository-migration.env
git tag -a v0.5.3 "$NEW_ROOT_COMMIT" -m "Release v0.5.3"
git push cc-session refs/tags/v0.5.3
RUN_ID=""
for _ in {1..30}; do
  RUN_ID=$(gh run list --repo osen77/cc-session --workflow release-new.yml --branch v0.5.3 --limit 1 --json databaseId --jq '.[0].databaseId // empty')
  [ -n "$RUN_ID" ] && break
  sleep 2
done
test -n "$RUN_ID"
gh run watch "$RUN_ID" --repo osen77/cc-session --exit-status
```

Expected: Release workflow 成功并将 draft 发布为公开 Release。

- [ ] **Step 5: 下载并验证新仓库全部资产**

Run:

```bash
ASSET_DIR="$HOME/cc-session-v0.5.3-assets"
mkdir -p "$ASSET_DIR"
gh release download v0.5.3 --repo osen77/cc-session --dir "$ASSET_DIR"
find "$ASSET_DIR" -maxdepth 1 -type f -print | sort
test "$(find "$ASSET_DIR" -maxdepth 1 -type f | wc -l | tr -d ' ')" = "10"
(cd "$ASSET_DIR" && shasum -a 256 -c *.sha256)
tar -tzf "$ASSET_DIR/ccs-linux-x86_64.tar.gz" | grep -Fx ccs
tar -tzf "$ASSET_DIR/ccs-linux-x86_64-musl.tar.gz" | grep -Fx ccs
tar -tzf "$ASSET_DIR/ccs-macos-x86_64.tar.gz" | grep -Fx ccs
tar -tzf "$ASSET_DIR/ccs-macos-aarch64.tar.gz" | grep -Fx ccs
unzip -t "$ASSET_DIR/ccs-windows-x86_64.zip"
printf 'ASSET_DIR=%q\n' "$ASSET_DIR" >> local/repository-migration.env
```

Expected: 十个文件、全部 checksum 通过，四个 tar 包含 `ccs`，ZIP 测试通过并包含 `ccs.exe`。

---

### Task 3: 发布旧仓库桥接资产并压缩为兼容快照

**Files:**
- Create temporarily: `$HOME/claude-code-sync-cn-compat-v0.5.3/`
- GitHub release: `osen77/claude-code-sync-cn@v0.5.3`
- Rewrite: `osen77/claude-code-sync-cn:master`
- Rewrite: `osen77/claude-code-sync-cn:v0.5.3`

**Interfaces:**
- Consumes: Task 2 的 `NEW_ROOT_COMMIT` 和 `ASSET_DIR`
- Produces: 与新仓库逐文件一致的旧仓库 `v0.5.3`
- Produces: 真实 `v0.5.2 -> v0.5.3` 自更新证据
- Produces: 旧仓库单根兼容快照

- [ ] **Step 1: 在旧仓库创建 draft，并上传同一批资产**

Run:

```bash
source local/repository-migration.env
test "$(gh release view v0.5.2 --repo osen77/claude-code-sync-cn --json isDraft --jq .isDraft)" = "false"
if gh release view v0.5.3 --repo osen77/claude-code-sync-cn >/dev/null 2>&1; then echo "legacy v0.5.3 already exists" >&2; exit 1; fi
gh release create v0.5.3 "$ASSET_DIR"/* \
  --repo osen77/claude-code-sync-cn \
  --target master \
  --title "Release v0.5.3 — cc-session migration bridge" \
  --notes "Compatibility bridge: after installing v0.5.3, ccs update reads future releases from https://github.com/osen77/cc-session." \
  --draft
```

Expected: 旧仓库的 draft 恰好包含 Task 2 下载的十个文件；此时旧仓库 latest 仍是 `v0.5.2`。

- [ ] **Step 2: 比较 draft 资产后发布旧仓库 `v0.5.3`**

Run:

```bash
source local/repository-migration.env
OLD_ASSET_DIR="$HOME/claude-code-sync-cn-v0.5.3-assets"
mkdir -p "$OLD_ASSET_DIR"
gh release download v0.5.3 --repo osen77/claude-code-sync-cn --dir "$OLD_ASSET_DIR"
for file in "$ASSET_DIR"/*; do cmp "$file" "$OLD_ASSET_DIR/$(basename "$file")"; done
gh release edit v0.5.3 --repo osen77/claude-code-sync-cn --draft=false
test "$(gh release view --repo osen77/claude-code-sync-cn --json tagName --jq .tagName)" = "v0.5.3"
printf 'OLD_ASSET_DIR=%q\n' "$OLD_ASSET_DIR" >> local/repository-migration.env
```

Expected: 所有文件字节一致，旧仓库 latest 变为 `v0.5.3`。

- [ ] **Step 3: 用真实 macOS Apple Silicon `v0.5.2` 验证旧更新链**

Run:

```bash
BRIDGE_TEST_DIR=$(mktemp -d)
gh release download v0.5.2 --repo osen77/claude-code-sync-cn --pattern ccs-macos-aarch64.tar.gz --dir "$BRIDGE_TEST_DIR"
tar -xzf "$BRIDGE_TEST_DIR/ccs-macos-aarch64.tar.gz" -C "$BRIDGE_TEST_DIR"
test "$("$BRIDGE_TEST_DIR/ccs" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" = "0.5.2"
printf '\n' | "$BRIDGE_TEST_DIR/ccs" update
test "$("$BRIDGE_TEST_DIR/ccs" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" = "0.5.3"
"$BRIDGE_TEST_DIR/ccs" update --check-only
printf 'BRIDGE_TEST_DIR=%q\n' "$BRIDGE_TEST_DIR" >> local/repository-migration.env
```

Expected: `v0.5.2` 从旧仓库成功替换为 `v0.5.3`；升级后检查更新成功，且代码级 URL 测试已证明它只查询新仓库。

- [ ] **Step 4: 创建旧仓库单根兼容快照**

Run:

```bash
source local/repository-migration.env
COMPAT_DIR="$HOME/claude-code-sync-cn-compat-v0.5.3"
mkdir -p "$COMPAT_DIR"
git archive "$NEW_ROOT_COMMIT" | tar -x -C "$COMPAT_DIR"
python3 - "$COMPAT_DIR/README.md" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
path.write_text("""# claude-code-sync-cn compatibility bridge

> 主项目已迁移到 [osen77/cc-session](https://github.com/osen77/cc-session)。命令和二进制仍叫 `ccs`。

此仓库仅作为旧版自动更新兼容入口长期保留：

- `ccs <= v0.5.2` 从本仓库发现并安装 `v0.5.3`。
- `v0.5.3` 之后的更新、源码、Issues 和 Releases 位于 `osen77/cc-session`。
- 旧安装脚本 URL 仍可访问，但脚本会从新仓库下载最新版本。

请勿删除此仓库或 `v0.5.3` Release。
""", encoding="utf-8")
PY
(
  cd "$COMPAT_DIR"
  git init -b master
  git add .
  git commit -m "chore: preserve cc-session update bridge"
  git tag -a v0.5.3 -m "Release v0.5.3 migration bridge"
)
printf 'COMPAT_DIR=%q\n' "$COMPAT_DIR" >> local/repository-migration.env
```

Expected: 快照包含完整 `v0.5.3` bridge source、两个安装脚本和首屏迁移 README，且只有一个根 commit。

- [ ] **Step 5: 用 lease 精确重写旧仓库 master 和桥接 tag**

Run:

```bash
source local/repository-migration.env
OLD_MASTER=$(git ls-remote origin refs/heads/master | cut -f1)
OLD_BRIDGE_TAG=$(git ls-remote origin refs/tags/v0.5.3 | cut -f1)
COMPAT_ROOT=$(git -C "$COMPAT_DIR" rev-parse master)
COMPAT_TAG=$(git -C "$COMPAT_DIR" rev-parse refs/tags/v0.5.3)
test "$(git -C "$COMPAT_DIR" rev-list --count master)" = "1"
git -C "$COMPAT_DIR" remote add legacy git@github.com:osen77/claude-code-sync-cn.git
git -C "$COMPAT_DIR" push legacy \
  --force-with-lease="master:$OLD_MASTER" \
  master:master
git -C "$COMPAT_DIR" push legacy \
  --force-with-lease="refs/tags/v0.5.3:$OLD_BRIDGE_TAG" \
  refs/tags/v0.5.3:refs/tags/v0.5.3
test "$(git ls-remote origin refs/heads/master | cut -f1)" = "$COMPAT_ROOT"
test "$(git ls-remote origin refs/tags/v0.5.3 | cut -f1)" = "$COMPAT_TAG"
printf 'OLD_MASTER=%q\nCOMPAT_ROOT=%q\nCOMPAT_TAG=%q\n' \
  "$OLD_MASTER" "$COMPAT_ROOT" "$COMPAT_TAG" \
  >> local/repository-migration.env
```

Expected: 仅精确更新旧 `master` 和 `v0.5.3`；其他 Release tags 和 `gh-pages` 不动。

- [ ] **Step 6: 验证旧 raw URL 仍安装新仓库版本**

Run:

```bash
curl -fsSL https://raw.githubusercontent.com/osen77/claude-code-sync-cn/master/install.sh | rg -F 'REPO="osen77/cc-session"'
curl -fsSL https://raw.githubusercontent.com/osen77/claude-code-sync-cn/master/install.ps1 | rg -F '$REPO = "osen77/cc-session"'
curl -fsSL https://raw.githubusercontent.com/osen77/claude-code-sync-cn/master/README.md | rg -F '主项目已迁移'
gh release view v0.5.3 --repo osen77/claude-code-sync-cn --json isDraft,assets --jq '{isDraft, assets: [.assets[].name]}'
```

Expected: 两个旧 raw 安装 URL 指向新仓库，README 显示迁移说明，Release 公开且十个资产仍在。

---

### Task 4: 切换本地开发入口、发布 `v0.5.4` 并完成收尾

**Files:**
- Modify: `Cargo.toml:1-4`
- Modify: `Cargo.lock:210-213`
- Modify locally only: `local/notes.md`
- GitHub release: `osen77/cc-session@v0.5.4`
- Archive: `osen77/claude-code-sync-cn`

**Interfaces:**
- Consumes: Task 3 已验证的 `v0.5.2 -> v0.5.3` 桥接链
- Produces: 当前本地 `origin = osen77/cc-session`、`legacy = osen77/claude-code-sync-cn`
- Produces: `v0.5.3 -> v0.5.4` 新仓库独占更新证据
- Produces: 已 Archive 但公开可读的旧兼容仓库

- [ ] **Step 1: 将当前本地 checkout 安全切换到新根历史**

Run:

```bash
source local/repository-migration.env
test -z "$(git status --porcelain)"
test "$(git rev-parse HEAD^{tree})" = "$SOURCE_TREE"
git remote rename origin legacy
git remote rename cc-session origin
git fetch origin master --tags
git update-ref refs/heads/master "$NEW_ROOT_COMMIT"
git branch --set-upstream-to=origin/master master
test "$(git rev-parse HEAD)" = "$NEW_ROOT_COMMIT"
test "$(git rev-list --count master)" = "1"
test -z "$(git status --porcelain)"
git remote -v
```

Expected: 工作树内容不变且干净；当前 `master` 只有新根 commit；`origin` 指向新仓库，`legacy` 指向旧仓库。

- [ ] **Step 2: 先把版本提升到 `0.5.4` 并运行验证**

修改 `Cargo.toml` package version 为 `0.5.4`，运行 `cargo check` 更新 `Cargo.lock`，然后执行：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; assert json.load(sys.stdin)["packages"][0]["version"] == "0.5.4"'
```

Expected: 全部通过，版本为 `0.5.4`。

- [ ] **Step 3: 精确提交并发布仅存在于新仓库的 `v0.5.4`**

Run:

```bash
git add Cargo.toml Cargo.lock
git diff --cached --check
git commit -m "chore: bump version to 0.5.4"
git tag -a v0.5.4 -m "Release v0.5.4"
git push origin master
git push origin refs/tags/v0.5.4
RUN_ID=""
for _ in {1..30}; do
  RUN_ID=$(gh run list --repo osen77/cc-session --workflow release-new.yml --branch v0.5.4 --limit 1 --json databaseId --jq '.[0].databaseId // empty')
  [ -n "$RUN_ID" ] && break
  sleep 2
done
test -n "$RUN_ID"
gh run watch "$RUN_ID" --repo osen77/cc-session --exit-status
if gh release view v0.5.4 --repo osen77/claude-code-sync-cn >/dev/null 2>&1; then echo "v0.5.4 must not exist in the legacy repository" >&2; exit 1; fi
```

Expected: 新仓库 `v0.5.4` Release 成功，旧仓库不存在该 Release。

- [ ] **Step 4: 验证两跳真实更新链**

Run:

```bash
source local/repository-migration.env
test "$("$BRIDGE_TEST_DIR/ccs" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" = "0.5.3"
printf '\n' | "$BRIDGE_TEST_DIR/ccs" update
test "$("$BRIDGE_TEST_DIR/ccs" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" = "0.5.4"
SECOND_TEST_DIR=$(mktemp -d)
gh release download v0.5.2 --repo osen77/claude-code-sync-cn --pattern ccs-macos-aarch64.tar.gz --dir "$SECOND_TEST_DIR"
tar -xzf "$SECOND_TEST_DIR/ccs-macos-aarch64.tar.gz" -C "$SECOND_TEST_DIR"
printf '\n' | "$SECOND_TEST_DIR/ccs" update
test "$("$SECOND_TEST_DIR/ccs" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" = "0.5.3"
printf '\n' | "$SECOND_TEST_DIR/ccs" update
test "$("$SECOND_TEST_DIR/ccs" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" = "0.5.4"
```

Expected: 已有桥接二进制从新仓库升级到 `v0.5.4`；全新旧版测试先从旧仓库升级到 `v0.5.3`，再从新仓库升级到 `v0.5.4`。

- [ ] **Step 5: 更新本机记录并 Archive 旧仓库**

先在 `local/notes.md` 的迁移条目补充新旧 root SHA、两边 `v0.5.3` 资产 SHA256、两跳更新结果和 GitHub Actions run URL，然后运行：

```bash
test "$(gh repo view osen77/claude-code-sync-cn --json isArchived --jq .isArchived)" = "false"
gh repo archive osen77/claude-code-sync-cn --yes
test "$(gh repo view osen77/claude-code-sync-cn --json isArchived --jq .isArchived)" = "true"
curl -fsSL https://raw.githubusercontent.com/osen77/claude-code-sync-cn/master/install.sh | rg -F 'osen77/cc-session'
gh release view v0.5.3 --repo osen77/claude-code-sync-cn --json isDraft --jq 'select(.isDraft == false)'
```

Expected: 旧仓库已 Archive，但 raw 安装脚本和 `v0.5.3` Release 仍公开可读。

- [ ] **Step 6: 准备并提交 GitHub Support 缓存清理请求**

从状态文件生成包含最终重写 SHA 的正文：

```bash
source local/repository-migration.env
cat > local/github-support-request.txt <<EOF
Repository: https://github.com/osen77/claude-code-sync-cn

The repository history has been rewritten and force-pushed after sensitive session JSONL files were removed with git-filter-repo. The compatibility master branch was rewritten once more during the repository migration.

Previously exposed commits/first changed commits:
- rollout JSONL old commit: b9380bf4
- data JSONL old commit: 1ce3ae0
- first changed commit: 2a2dc8854ddedd417b754c5b6e914725f89e1a6a
- first changed data commit: 1ce3ae0be8052af6e2e2804abd49db38ec01a9b6
- final pre-compatibility master tip: $OLD_MASTER
- replacement compatibility root: $COMPAT_ROOT

Please purge cached commit/raw views, hidden refs and unreachable objects, and run server-side garbage collection for the rewritten repository.
EOF
pbcopy < local/github-support-request.txt
open https://support.github.com/contact
```

若浏览器自动化仍被占用，则正文已在剪贴板，由用户登录后点击最终提交。提交后在 `local/notes.md` 记录 ticket ID；`local/github-support-request.txt` 保持 ignored，不提交。

- [ ] **Step 7: 最终验证并清理迁移备份**

Run:

```bash
source local/repository-migration.env
git status --short --branch
test "$(git rev-list --count master)" = "2"
test "$(git ls-remote origin refs/heads/master | cut -f1)" = "$(git rev-parse master)"
test "$(gh release view --repo osen77/cc-session --json tagName --jq .tagName)" = "v0.5.4"
test "$(gh release view --repo osen77/claude-code-sync-cn --json tagName --jq .tagName)" = "v0.5.3"
git fsck --full --strict
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
rm -f "$BACKUP_BUNDLE"
rm -rf "$ASSET_DIR" "$OLD_ASSET_DIR" "$BRIDGE_TEST_DIR" "$SECOND_TEST_DIR" "$COMPAT_DIR"
```

Expected: 新仓库历史为根提交加 `v0.5.4` 版本提交；新旧 latest 分别为 `v0.5.4` / `v0.5.3`；Git 和 Rust 验证通过；确认全部迁移证据已写入 `local/notes.md` 后才删除权限受限备份和临时目录。
