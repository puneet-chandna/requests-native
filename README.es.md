<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
# Requests Native

[English](https://github.com/puneet-chandna/requests-native/blob/main/README.md) · [Español](https://github.com/puneet-chandna/requests-native/blob/main/README.es.md) · [简体中文](https://github.com/puneet-chandna/requests-native/blob/main/README.zh-CN.md) · [Français](https://github.com/puneet-chandna/requests-native/blob/main/README.fr.md) · [हिन्दी](https://github.com/puneet-chandna/requests-native/blob/main/README.hi.md) · [日本語](https://github.com/puneet-chandna/requests-native/blob/main/README.ja.md)

Requests Native lleva la conocida API de Python `requests` a un núcleo HTTP compartido escrito en Rust.
Ese mismo núcleo también está disponible directamente como clientes nativos asíncronos y bloqueantes para Rust.

[Versión 1.0.0](https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0) ·
[crates.io](https://crates.io/crates/requests-native) ·
[Instalación](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst) · [Incidencias](https://github.com/puneet-chandna/requests-native/issues)

## Rust: versión estable 1.0.0

El crate de Rust está publicado en crates.io. Usa **Rust 1.98.1 o posterior** y añade:

```toml
[dependencies]
requests-native = "1.0.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

El crate se llama `requests-native`; la biblioteca se importa como `requests_native`.
El ejemplo asíncrono requiere Tokio. El cliente bloqueante está incluido de forma predeterminada.

### Asíncrono

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

### Bloqueante

```rust
use requests_native::blocking::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send()?;
    println!("{}", response.text()?);
    Ok(())
}
```

Reutiliza un cliente para reutilizar su grupo de conexiones.

## Python: instala el código fuente mejorado de 1.0.0

La publicación en PyPI está prevista para los próximos días; se está ultimando
la configuración de publicación. La distribución de Python **todavía no está publicada en PyPI**;
por ahora, usa el código fuente mejorado de `v1.0.0`. Esta versión no incluye wheels estables de Python.

Necesitas **CPython 3.10+**, Git, la cadena de herramientas **Rust 1.98.1** fijada por el repositorio
y un compilador nativo de C/C++ (MSVC Build Tools en Windows). Instala Rust mediante
[rustup](https://rustup.rs/) y crea un entorno nuevo:

```console
rustup toolchain install 1.98.1 --profile minimal
python -m venv .venv
```

Actívalo en Linux/macOS:

```console
source .venv/bin/activate
```

O en Windows PowerShell:

```powershell
.venv\Scripts\Activate.ps1
```

Instala el código fuente de la etiqueta:

```console
python -m pip install "requests-native @ git+https://github.com/puneet-chandna/requests-native.git@v1.0.0"
```

Usa la API de siempre:

```python
import requests

with requests.Session() as session:
    response = session.get("https://httpbin.org/get", timeout=10)
    response.raise_for_status()
    print(response.json())
```

La distribución es `requests-native` **1.0.0**; `import requests` y
`requests.__version__ == "2.34.2"` conservan la identidad de compatibilidad con Requests.

**Usa un entorno separado del paquete `requests` original.** Ambas distribuciones
ocupan las mismas rutas de importación; las dependencias del paquete llamado `requests`
pueden reinstalar Requests original. Consulta los [detalles de instalación](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst).

Las comprobaciones actuales de Windows pasaron sin aceptar fallos de TLS; la [incidencia histórica #1](https://github.com/puneet-chandna/requests-native/issues/1), que no se ha reproducido, sigue abierta.

## Rendimiento

El núcleo de Rust publicado en 1.0.0 se midió en una comparación local por loopback en cinco pares, con 16 cargas de trabajo equivalentes por cliente nativo.

| Cliente nativo | Solicitudes por segundo frente a Requests original | Solicitudes por segundo frente a la beta |
| --- | --- | --- |
| Asíncrono | 2.65x | 1.005x |
| Bloqueante | 2.23x | 1.055x |

Cada valor es la mediana de las 16 razones medianas por carga de trabajo calculadas en los cinco pares.
Las mejoras varían según la carga; la inestabilidad de los controles del oráculo deja incertidumbre estadística.
Consulta la [metodología](https://github.com/puneet-chandna/requests-native/blob/main/benchmarks/README.md) y las [pruebas vinculadas al código fuente exacto](https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md).

## Compatibilidad y desarrollo

El tráfico de los componentes integrados `Session`/`HTTPAdapter` usa el transporte nativo. Los adaptadores
personalizados, las subclases, el monkeypatching y los comportamientos de extensión no compatibles pueden
recurrir a Python antes de las operaciones de E/S nativas. Prueba tus integraciones con tu propia carga de trabajo.

Consulta el [inventario de compatibilidad de la API](https://github.com/puneet-chandna/requests-native/blob/main/API_COMPATIBILITY.tsv),
la [guía de migración](https://github.com/puneet-chandna/requests-native/blob/main/PORTING.md) y
la [guía de contribución](https://github.com/puneet-chandna/requests-native/blob/main/.github/CONTRIBUTING.md).
Informa de vulnerabilidades siguiendo la [política de seguridad](https://github.com/puneet-chandna/requests-native/blob/main/.github/SECURITY.md).

## Atribución

Requests Native es una reescritura **no oficial e independiente** de
[PSF Requests](https://github.com/psf/requests), mantenida por
[Puneet Chandna](https://github.com/puneet-chandna). No está afiliada a la Python Software Foundation
ni a los responsables de Requests original, ni cuenta con su respaldo.

La obra derivada conserva la licencia Apache-2.0, los avisos, el historial y el registro
original de colaboradores. Consulta [LICENSE](LICENSE), [NOTICE](NOTICE),
[AUTHORS.rst](AUTHORS.rst), los [avisos del entorno de ejecución](RUST_RUNTIME_NOTICES.html) e
[HISTORY.md](https://github.com/puneet-chandna/requests-native/blob/main/HISTORY.md).
