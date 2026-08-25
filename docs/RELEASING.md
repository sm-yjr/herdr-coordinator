# 发布与 Tag 管理

项目使用不可移动的 SemVer tag 发布 Rust 二进制。Git tag、`herdr-plugin.toml` 和 `tower/Cargo.toml` 的版本必须完全一致。

## 发布步骤

1. 同时修改两个 manifest 的版本，例如 `0.3.1`。
2. 运行本地发布检查：

   ```bash
   cargo fmt --manifest-path tower/Cargo.toml --all --check
   cargo clippy --manifest-path tower/Cargo.toml --all-targets -- -D warnings
   cargo test --manifest-path tower/Cargo.toml --all-targets
   cargo build --manifest-path tower/Cargo.toml --release --locked
   ```

3. 合并并确认 `main` 上的 CI 通过。
4. 创建 annotated tag 并只推送该 tag：

   ```bash
   git tag -a v0.3.1 -m "v0.3.1"
   git push origin v0.3.1
   ```

5. `release.yml` 校验三个版本，构建四个平台，生成 SHA-256 校验和并创建 GitHub Release。

## 失败处理

已经推送的版本 tag 不应删除、覆盖或移动。发布代码有问题时，修复后增加 patch 版本并创建新 tag。只有 tag 尚未推送、没有其他使用者时，才可以在本地删除并重建。

Release 必须包含：

```text
herdr-coordinator-x86_64-unknown-linux-gnu.tar.gz
herdr-coordinator-aarch64-unknown-linux-gnu.tar.gz
herdr-coordinator-x86_64-apple-darwin.tar.gz
herdr-coordinator-aarch64-apple-darwin.tar.gz
checksums.txt
```

Plugin 安装脚本先验证 `checksums.txt`，再安装匹配平台的二进制。下载或校验失败时，只有本机存在 Cargo 才会回退到源码编译。
