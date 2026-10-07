<!-- Requests Native modification notice: this retained file differs from Requests 2.34.2. -->
# Requests Native

<p align="center">
  <img src="https://raw.githubusercontent.com/puneet-chandna/requests-native/main/docs/assets/logo.png" alt="Requests Native" width="250">
</p>

[English](https://github.com/puneet-chandna/requests-native/blob/main/README.md) · [Español](https://github.com/puneet-chandna/requests-native/blob/main/README.es.md) · [简体中文](https://github.com/puneet-chandna/requests-native/blob/main/README.zh-CN.md) · [Français](https://github.com/puneet-chandna/requests-native/blob/main/README.fr.md) · [हिन्दी](https://github.com/puneet-chandna/requests-native/blob/main/README.hi.md) · [日本語](https://github.com/puneet-chandna/requests-native/blob/main/README.ja.md)

Requests Native relie l'API Python `requests` à un cœur HTTP commun écrit en Rust.
Ce même cœur est aussi accessible directement en Rust, avec des clients natifs asynchrones et bloquants.
Les deux clients Rust prennent en charge les pools de connexions, les délais d'attente configurables, les réglages de proxy et de TLS
et la transmission en flux des corps de requêtes et de réponses.

[Version 1.0.0](https://github.com/puneet-chandna/requests-native/releases/tag/v1.0.0) ·
[crates.io](https://crates.io/crates/requests-native) ·
[Installation](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst) · [Signalement de problèmes](https://github.com/puneet-chandna/requests-native/issues)

## Rust : version stable 1.0.0

La crate Rust est publiée sur crates.io. Utilisez **Rust 1.98.1 ou une version plus récente** et ajoutez :

```toml
[dependencies]
requests-native = "1.0.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

La crate s'appelle `requests-native` ; le nom à importer est `requests_native`.
L'exemple asynchrone nécessite Tokio. Le client bloquant est inclus par défaut.

### Asynchrone

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

### Bloquant

```rust
use requests_native::blocking::Client;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new()?;
    let response = client.get("https://httpbin.org/get").send()?;
    println!("{}", response.text()?);
    Ok(())
}
```

Réutilisez un client pour réutiliser son pool de connexions.

## Python : installer la version améliorée 1.0.0 depuis les sources

La publication sur PyPI est prévue dans les prochains jours ; sa configuration est en cours de finalisation.
La distribution Python **n'est pas encore publiée sur PyPI** ; vous pouvez dès maintenant utiliser
les sources améliorées de `v1.0.0`. Cette version ne contient pas de wheels Python stables.

Il vous faut **CPython 3.10+**, Git, la chaîne d'outils **Rust 1.98.1** fixée par le dépôt,
ainsi qu'un compilateur C/C++ natif (MSVC Build Tools sous Windows). Installez Rust avec
[rustup](https://rustup.rs/), puis créez un nouvel environnement :

```console
rustup toolchain install 1.98.1 --profile minimal
python -m venv .venv
```

Activez-le sous Linux/macOS :

```console
source .venv/bin/activate
```

Ou dans Windows PowerShell :

```powershell
.venv\Scripts\Activate.ps1
```

Installez les sources du tag :

```console
python -m pip install "requests-native @ git+https://github.com/puneet-chandna/requests-native.git@v1.0.0"
```

Utilisez l'API habituelle :

```python
import requests

with requests.Session() as session:
    response = session.get("https://httpbin.org/get", timeout=10)
    response.raise_for_status()
    print(response.json())
```

La distribution est `requests-native` **1.0.0** ; `import requests` et
`requests.__version__ == "2.34.2"` conservent l'identité de compatibilité avec Requests.

**Utilisez un environnement distinct de celui de `requests` amont.** Les deux distributions
occupent les mêmes chemins d'importation ; une dépendance au paquet nommé `requests` peut
réinstaller Requests amont. Consultez les [détails d'installation](https://github.com/puneet-chandna/requests-native/blob/main/docs/user/install.rst).

Les vérifications Windows actuelles ont réussi sans accepter d'échecs TLS ; le [problème historique #1](https://github.com/puneet-chandna/requests-native/issues/1), qui n'a pas été reproduit, reste ouvert.

## Performances

Le cœur Rust 1.0.0 publié a été mesuré sur cinq paires dans une comparaison en boucle locale, avec 16 charges de travail correspondantes pour chaque client natif.

| Client natif | Rapport médian de débit par rapport à Requests d'origine | Rapport maximal observé par rapport à Requests d'origine | Débit maximal observé (requêtes/s) | Rapport médian de débit par rapport à la bêta |
| --- | --- | --- | --- | --- |
| Asynchrone | 2.65x | 11.92x | 13,479 | 1.005x |
| Bloquant | 2.23x | 9.96x | 10,513 | 1.055x |

Les rapports médians résument les 16 médianes par charge de travail calculées sur cinq paires.
Les pics sont les observations les plus élevées pour une seule mesure dans les rapports, pas les performances typiques ; le rapport maximal et le débit maximal peuvent provenir de mesures différentes.
Les gains varient selon la charge de travail ; les contrôles de l'oracle sont instables, ce qui laisse une incertitude statistique.
Consultez la [méthodologie](https://github.com/puneet-chandna/requests-native/blob/main/benchmarks/README.md) et les [preuves liées aux sources exactes](https://github.com/puneet-chandna/requests-native/blob/main/docs/dev/release-readiness.md).

## Compatibilité et développement

Les requêtes des classes intégrées `Session`/`HTTPAdapter` utilisent le transport natif. Les adaptateurs
personnalisés, les sous-classes, le monkeypatching et les comportements d'extension non pris en charge peuvent
revenir au transport Python avant les I/O natives. Testez vos intégrations avec votre propre charge de travail.

Consultez l'[inventaire de compatibilité de l'API](https://github.com/puneet-chandna/requests-native/blob/main/API_COMPATIBILITY.tsv),
le [guide de portage](https://github.com/puneet-chandna/requests-native/blob/main/PORTING.md) et
le [guide de contribution](https://github.com/puneet-chandna/requests-native/blob/main/.github/CONTRIBUTING.md).
Signalez les vulnérabilités en suivant la [politique de sécurité](https://github.com/puneet-chandna/requests-native/blob/main/.github/SECURITY.md).

## Attribution

Requests Native est une réécriture **non officielle et indépendante** de
[PSF Requests](https://github.com/psf/requests), maintenue par
[Puneet Chandna](https://github.com/puneet-chandna). Le projet n'est affilié ni à la
Python Software Foundation ni aux mainteneurs de Requests amont, et n'est pas approuvé par eux.

Ce projet dérivé conserve la licence Apache-2.0, les notices, l'historique et la liste
des contributeurs d'origine. Consultez [LICENSE](LICENSE), [NOTICE](NOTICE),
[AUTHORS.rst](AUTHORS.rst), les [notices des composants d'exécution](RUST_RUNTIME_NOTICES.html) et
[HISTORY.md](https://github.com/puneet-chandna/requests-native/blob/main/HISTORY.md).
