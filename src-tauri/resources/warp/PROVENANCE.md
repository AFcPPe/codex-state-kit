# 内置 WARP 内核来源

## 固定版本

| 项目 | 记录 |
| --- | --- |
| 上游项目 | [usque](https://github.com/Diniboy1123/usque) |
| 版本 | [v4.2.1](https://github.com/Diniboy1123/usque/releases/tag/v4.2.1) |
| 源码提交 | `6aa03fc97d12848dce34eedbd187fb1077b5d1ea` |
| 发布压缩包 | `usque_4.2.1_windows_amd64.zip` |
| 压缩包 SHA-256 | `f6f7f0a1a2bc9bcc15cf563ec1f892d00690a92c086b23ed3211b802209099e7` |

项目随包分发上游未修改的 Windows x64 可执行文件。usque 是第三方用户态实现，并非 Cloudflare 官方桌面客户端。

应用仅使用绑定回环地址、带认证的 SOCKS 模式，不启用 TUN 模式或安装驱动。

## 许可证与构建记录

- [LICENSE.md](LICENSE.md)：上游 MIT 许可证原文。
- [DEPENDENCY-NOTICES.txt](DEPENDENCY-NOTICES.txt)：内核所链接依赖的许可与声明原文。
- [BUILD-INFO.txt](BUILD-INFO.txt)：内核构建信息。

许可证和依赖声明保留上游原文，不以中文说明替代。随安装包分发时应保留这些文件。

## 复现与更新

在仓库根目录通过 PowerShell 运行：

```powershell
./tools/prepare-warp.ps1
./tools/warp-notices.ps1
```

[下载脚本](../../../tools/prepare-warp.ps1)固定版本并校验压缩包 SHA-256。[声明生成脚本](../../../tools/warp-notices.ps1)需要 Go 与 ripgrep，用于读取构建信息并收集依赖许可。

以上是维护者的构建步骤，终端用户无需下载内核。升级时同步更新脚本中的版本、校验值、本文件及依赖声明，并重新验证安装包。

使用说明见[出站代理与 WARP](../../../docs/warp.md)。
