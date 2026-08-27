# CI/CD, Docker и Kubernetes

`rib` запускается как обычный непривилегированный процесс. Сборочному заданию
не требуются Docker socket, Docker-in-Docker, privileged-контейнер или доступ к
container runtime узла. Это одинаково применимо к shell runner, Docker executor
и Kubernetes executor.

Рекомендуемая модель состоит из одного сборочного образа, содержащего:

- компилятор или runtime, необходимый приложению;
- `rib` либо `cargo-rib`;
- дополнительные инструменты pipeline, например `cosign` и Vault CLI.

Такой образ загружается и кэшируется стандартным механизмом runner'а. Внутреннее
состояние отдельного build daemon между заданиями передавать не требуется.

## Сборка фронтенда и подпись через cosign

Следующий GitLab CI job предполагает, что:

- каталог `frontend` уже содержит результат сборки статического фронтенда;
- `frontend-base:1.0` содержит `execenv` и `static-web-server`;
- job image содержит `rib`, Vault CLI и `cosign`;
- в Vault настроены JWT auth method, роль `cosign` и Transit key `cosign`.

```yaml
default:
  tags:
    - core-os

stages:
  - Build

build_image_rib:
  image: registry.gitlab.com/devdsk/education/rust/rib/rib:v0.1.0
  stage: Build
  id_tokens:
    VAULT_ID_TOKEN:
      aud: https://gitlab.com
  variables:
    VAULT_AUTH_PATH: auth/jwt/login
    VAULT_AUTH_ROLE: cosign
    TRANSIT_SECRET_ENGINE_PATH: cosign
    VAULT_ADDR: https://vault.core-os.ru
    COSIGN_KEY: hashivault://cosign
    HOME: /tmp
  script:
    - |
      IMAGE_DIGEST="$(
        rib build \
          --from "$CI_REGISTRY_IMAGE/frontend-base:1.0" \
          --credential "$CI_REGISTRY=$CI_REGISTRY_USER:$CI_REGISTRY_PASSWORD" \
          --copy "./frontend:/frontend,chown=65532:65532" \
          --entrypoint "/execenv -m error -f /frontend/config.js --exec /static-web-server -p 8787 -d /frontend -g info" \
          --to "registry:$CI_REGISTRY_IMAGE/taskd-frontend:$CI_COMMIT_SHORT_SHA"
      )"
    - |
      VAULT_TOKEN="$(
        vault write --field=token "$VAULT_AUTH_PATH" \
          role="$VAULT_AUTH_ROLE" \
          jwt="$VAULT_ID_TOKEN"
      )"
      export VAULT_TOKEN
    - cosign login "$CI_REGISTRY" -u "$CI_REGISTRY_USER" -p "$CI_REGISTRY_PASSWORD"
    - >
      cosign sign --tlog-upload=false
      "${CI_REGISTRY_IMAGE}/taskd-frontend@${IMAGE_DIGEST}"
```

Человекочитаемый вывод `rib` остаётся в stderr и виден в логе job. В
`IMAGE_DIGEST` попадает только значение `sha256:...`, поэтому `cosign` подписывает
точный manifest, а не изменяемый тег.

Для базового образа не выполняется предварительный pull. `rib` получает
manifest и config из registry, после чего проверяет наличие каждого blob в
целевом репозитории. Поскольку source и target находятся в одном GitLab
Container Registry, унаследованные слои могут быть переиспользованы или
смонтированы между репозиториями без передачи через файловую систему job.

## Сборка Rust-проекта через интеграцию с Cargo

Для Rust-проекта удобно включить `cargo-rib` непосредственно в образ с
toolchain и target `x86_64-unknown-linux-musl`. GitLab кэширует сам builder
image обычным механизмом container runtime, а Cargo registry и `target`
сохраняются стандартным кэшем GitLab CI.

```yaml
default:
  tags:
    - core-os

variables:
  CARGO_HOME: "$CI_PROJECT_DIR/.cargo"

stages:
  - Build

cache:
  key:
    files:
      - Cargo.lock
    prefix: "$CI_COMMIT_REF_SLUG"
  paths:
    - .cargo/registry/index/
    - .cargo/registry/cache/
    - .cargo/git/db/
    - target/

build_image_rib:
  image: registry.gitlab.com/devdsk/education/rust/taskd/rust-builder/rust-rib:alpine3.24
  stage: Build
  script:
    - >
      cargo rib build
      --credential "$CI_REGISTRY=$CI_REGISTRY_USER:$CI_REGISTRY_PASSWORD"
      --to "registry:$CI_REGISTRY_IMAGE/taskd:$CI_COMMIT_SHORT_SHA"
```

Параметры образа хранятся рядом с исходным кодом в `Cargo.toml`. В этом примере
registry и тег зависят от переменных pipeline, поэтому цель публикации
передаётся через `--to`. Для постоянной цели её также можно указать в поле `to`
в `[package.metadata.rib]`:

```toml
[package]
name = "taskd"
version = "0.1.0"
edition = "2021"
rust-version = "1.94"

[package.metadata.rib]
artifact = "target/x86_64-unknown-linux-musl/release/taskd"
platform = "linux/amd64"
from = "scratch"

destination = "/taskd"
entrypoint = "/taskd"
workdir = "/"
user = "65532:65532"
ports = ["8080"]
creation-time = "epoch"
jobs = 4
cargo-args = [
  "--release",
  "--locked",
  "--target", "x86_64-unknown-linux-musl",
]
```

Команда `cargo rib build` выполняет `cargo build` с указанными аргументами,
проверяет существование `artifact`, добавляет его последним слоем с режимом
`0755` и публикует образ. Для `scratch` артефакт должен быть статически
скомпонован либо все runtime-зависимости должны быть добавлены через `copies`.

## Запуск в Kubernetes executor

При использовании GitLab Runner с Kubernetes executor приведённые job не
требуют специального Kubernetes manifest. Runner создаёт обычный pod из образа,
указанного в поле `image`.
