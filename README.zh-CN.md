<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
# Requests Native

[English](https://github.com/puneet-chandna/requests-native/blob/main/README.md) · [Español](https://github.com/puneet-chandna/requests-native/blob/main/README.es.md) · [简体中文](https://github.com/puneet-chandna/requests-native/blob/main/README.zh-CN.md) · [Français](https://github.com/puneet-chandna/requests-native/blob/main/README.fr.md) · [हिन्दी](https://github.com/puneet-chandna/requests-native/blob/main/README.hi.md) · [日本語](https://github.com/puneet-chandna/requests-native/blob/main/README.ja.md)

Requests Native 将熟悉的 Python `requests` API 接入共用的 Rust HTTP 核心。
Rust 开发者也可以直接使用同一核心提供的原生异步和阻塞客户端。

[1.0.0 版本](https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0) ·
[crates.io](https://crates.io/crates/requests-native) ·
[安装](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst) · [问题反馈](https://github.com/puneet-chandna/requests-native/issues)

## Rust：稳定版 1.0.0

Rust crate 已发布到 crates.io。请使用 **Rust 1.98.1 或更高版本**，并添加：

```toml
[dependencies]
requests-native = "1.0.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

crate 名称为 `requests-native`，导入库时使用 `requests_native`。
异步示例需要 Tokio。默认已包含阻塞客户端。

### 异步

```rust
use requests_native::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send().await?;
    println!("{}", response.text().await?);
    Ok(())
}
```

### 阻塞

```rust
use requests_native::blocking::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send()?;
    println!("{}", response.text()?);
    Ok(())
}
```

复用客户端即可复用其连接池。

## Python：从源码安装改进后的 1.0.0

计划在未来几天内发布到 PyPI，目前正在完成发布配置。
Python 发行包**尚未发布到 PyPI**；现在可以使用改进后的 `v1.0.0` 源码。
本次发布不包含稳定版 Python wheel。

你需要 **CPython 3.10+**、Git、仓库固定的 **Rust 1.98.1** 工具链，
以及本机 C/C++ 编译器（Windows 上使用 MSVC Build Tools）。通过
[rustup](https://rustup.rs/) 安装 Rust，然后创建一个新环境：

```console
rustup toolchain install 1.98.1 --profile minimal
python -m venv .venv
```

在 Linux/macOS 上激活环境：

```console
source .venv/bin/activate
```

或在 Windows PowerShell 中激活：

```powershell
.venv\Scripts\Activate.ps1
```

安装带版本标签的源码：

```console
python -m pip install "requests-native @ git+https://github.com/puneet-chandna/requests-native.git@v1.0.0"
```

使用熟悉的 API：

```python
import requests

with requests.Session() as session:
    response = session.get("https://httpbin.org/get", timeout=10)
    response.raise_for_status()
    print(response.json())
```

发行包是 `requests-native` **1.0.0**；`import requests` 和
`requests.__version__ == "2.34.2"` 保留了与 Requests 兼容的标识。

**请与上游 `requests` 使用不同的环境。** 两个发行包占用相同的导入路径；
依赖名为 `requests` 的包时，可能会重新安装上游 Requests。
请参阅[安装说明](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst)。

当前 Windows 检查已通过，未将任何 TLS 失败视为可接受；尚未复现的历史[问题 #1](https://github.com/puneet-chandna/requests-native/issues/1) 仍未关闭。

## 性能

对已发布的 1.0.0 Rust 核心进行了五组本地回环配对对比测量，每个原生客户端各有 16 项对应的工作负载。

| 原生客户端 | 相对原版 Requests 的吞吐量 | 相对 beta 版的吞吐量 |
| --- | --- | --- |
| 异步 | 2.65x | 1.005x |
| 阻塞 | 2.23x | 1.055x |

每个数值都先取每项工作负载在五组配对测量中的比值中位数，再取这 16 个中位数的中位数。
提升幅度随工作负载而异；oracle 对照不稳定，仍存在统计不确定性。
请参阅[测量方法](https://github.com/puneet-chandna/requests-native/blob/main/benchmarks/README.md)和[对应源码的证据](https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md)。

## 兼容性与开发

内置 `Session`/`HTTPAdapter` 的请求使用原生传输。
自定义适配器、子类、猴子补丁和不受支持的扩展行为可能在原生 I/O 开始前回退到 Python。
请使用自己的工作负载测试集成。

请参阅 [API 兼容性清单](https://github.com/puneet-chandna/requests-native/blob/main/API_COMPATIBILITY.tsv)、
[移植指南](https://github.com/puneet-chandna/requests-native/blob/main/PORTING.md)和
[贡献指南](https://github.com/puneet-chandna/requests-native/blob/main/.github/CONTRIBUTING.md)。
请通过[安全政策](https://github.com/puneet-chandna/requests-native/blob/main/.github/SECURITY.md)报告漏洞。

## 致谢与归属

Requests Native 是 [PSF Requests](https://github.com/psf/requests) 的**非官方、独立**重写版本，
由 [Puneet Chandna](https://github.com/puneet-chandna) 维护。
本项目与 Python Software Foundation 或上游 Requests 维护者没有隶属关系，
也未获得其背书。

本衍生项目保留 Apache-2.0 许可证、声明、历史记录和原始贡献者记录。
请参阅 [LICENSE](LICENSE)、[NOTICE](NOTICE)、
[AUTHORS.rst](AUTHORS.rst)、[运行时声明](RUST_RUNTIME_NOTICES.html)和
[HISTORY.md](https://github.com/puneet-chandna/requests-native/blob/main/HISTORY.md)。
