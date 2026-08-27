# Интеграция с Cargo

Пакет устанавливает бинарник `cargo-rib`, который Cargo вызывает командой:

```bash
cargo rib build
```

Интеграция не реализует отдельный способ формирования образа. Она преобразует
Cargo metadata и аргументы CLI в `ImageBuildSpec`, после чего использует тот же
конвейер, что и `rib build`.

## Последовательность операций

`cargo rib build` выполняет следующие действия:

1. вызывает `cargo metadata --format-version 1 --no-deps`;
2. выбирает пакет в workspace;
3. читает `[package.metadata.rib]` из его `Cargo.toml`;
4. объединяет metadata с аргументами командной строки;
5. запускает `cargo build --manifest-path <path>` с настроенными
   `cargo-args`;
6. проверяет, что `artifact` существует и является обычным файлом;
7. добавляет artifact последним `COPY`-слоем с режимом `0755`;
8. формирует и публикует образ общим конвейером RIB.

Путь к artifact, целевая платформа и базовый образ задаются явно. Интеграция
не определяет автоматически binary target, архитектуру или способ линковки.

## Расположение и формат конфигурации

Конфигурация RIB размещается в таблице `[package.metadata.rib]` файла
`Cargo.toml` выбранного пакета.

Все имена полей записываются в kebab-case. Неизвестный ключ считается ошибкой,
поэтому опечатка обнаруживается до запуска `cargo build`.

После объединения конфигурации и CLI обязательны четыре значения:

- `artifact`;
- `platform`;
- `from`;
- хотя бы один элемент `to`.

Каждое из них можно задать как в `Cargo.toml`, так и соответствующим флагом
команды `cargo rib build`. Цели публикации не ограничены CI: `to` является
обычным полем `[package.metadata.rib]`.

## Артефакт и параметры Cargo

| Поле | Тип и значение по умолчанию | Описание и пример |
|---|---|---|
| `artifact` | Строка; обязательна после объединения с CLI | Путь к обычному файлу, который должен создать `cargo build`, например `"target/x86_64-unknown-linux-musl/release/taskd"`. Относительный путь в metadata разрешается от каталога `Cargo.toml`. |
| `cargo-args` | Массив строк; `[]` | Аргументы, передаваемые после `cargo build --manifest-path <path>`, например `["--release", "--locked", "--target", "x86_64-unknown-linux-musl", "--bin", "taskd"]`. |
| `destination` | Строка; `/app/<имя artifact>` | Абсолютный путь artifact внутри образа, например `"/usr/local/bin/taskd"`. Значение не может оканчиваться `/`. Artifact всегда добавляется последним слоем с режимом `0755`. |
| `copies` | Массив строк; `[]` | Дополнительные `COPY`-слои, например `["config/default.toml:/etc/taskd/config.toml", "assets:/opt/taskd/assets/"]`. Source-пути из metadata разрешаются от каталога `Cargo.toml`; каждый элемент создаёт отдельный слой перед artifact. |

Синтаксис элемента `copies`:

```text
<source>:<destination>[,mode=<octal>][,chown=<uid>[:<gid>]]
```

Например:

```toml
copies = [
  "config/default.toml:/etc/taskd/config.toml,mode=0644,chown=65532:65532",
  "assets:/opt/taskd/assets/",
]
```

## Основа, платформа и цели публикации

| Поле | Тип и значение по умолчанию | Описание и пример |
|---|---|---|
| `platform` | Строка; обязательна после объединения с CLI | OCI-платформа в формате `<os>/<arch>[/<variant>]`, например `"linux/amd64"` или `"linux/arm64/v8"`. RIB не проверяет, что artifact действительно собран для этой платформы. |
| `from` | Строка; обязательна после объединения с CLI | Базовый образ, например `"alpine:3.22"`, `"registry.example.com/base/runtime@sha256:..."`, либо точное значение `"scratch"`. |
| `to` | Массив строк; `[]`, но после объединения требуется хотя бы один элемент | Цели публикации. Допустимы `"registry:registry.example.com/team/taskd:v1"`, `"oci-archive:dist/taskd-oci.tar@taskd:v1"` и `"docker-archive:dist/taskd-docker.tar@taskd:v1"`. Поле может содержать несколько целей. |

Относительные пути в `oci-archive:` и `docker-archive:`, заданные в metadata,
разрешаются от каталога `Cargo.toml`. Registry reference от локального каталога
не зависит.

Цели из CLI добавляются после целей из metadata. Например, при следующей
конфигурации и команде будут созданы архив и registry image:

```toml
[package.metadata.rib]
to = ["oci-archive:dist/taskd.tar@taskd:local"]
```

```bash
cargo rib build --to registry:registry.example.com/team/taskd:v1
```

CLI не предоставляет отдельного режима замены списка `to`. Если в CI требуется
только динамическая registry-цель, поле `to` следует не задавать в metadata и
передать `--to` в job.

## Конфигурация запуска контейнера

| Поле | Тип и значение по умолчанию | Описание и пример |
|---|---|---|
| `entrypoint` | Строка или массив строк; `[destination]` | OCI Entrypoint. Примеры: `"/taskd --log-format json"` или `["/taskd", "--log-format", "json"]`. При замене Entrypoint унаследованный Cmd удаляется, если не заданы `cmd` или `keep-cmd = true`. |
| `cmd` | Строка или массив строк; явно не задаётся | OCI Cmd. Примеры: `"serve --port 8080"` или `["serve", "--port", "8080"]`. Явное значение заменяет Cmd базового образа. |
| `labels` | Массив строк; `[]` | Метки в формате `key=value`, например `["org.opencontainers.image.title=taskd", "org.opencontainers.image.version=1.0.0"]`. Они объединяются с метками базового образа; одинаковый ключ перезаписывается. |
| `ports` | Массив строк; `[]` | Открытые порты, например `["8080", "8443/tcp", "8125/udp"]`. Если протокол не указан, добавляется `/tcp`. Порты дополняют значения базового образа. |
| `workdir` | Строка; наследуется из базового образа | Рабочий каталог процесса, например `"/var/lib/taskd"`. Для `scratch` значение отсутствует, пока не задано явно. |
| `user` | Строка; наследуется из базового образа | Пользователь процесса, например `"65532:65532"`, `"1000"` или `"app"`. RIB записывает значение в OCI config и не проверяет наличие пользователя внутри rootfs. |
| `keep-cmd` | Булево значение; `false` | Сохраняет Cmd базового образа при замене Entrypoint. Пример: `keep-cmd = true`. Поле не влияет на явно заданный `cmd`. |
| `creation-time` | Строка; `"epoch"` | Время создания config и новых history entries: `"epoch"`, `"now"` или RFC3339, например `"2026-08-27T12:00:00Z"`. Для воспроизводимых образов следует использовать `"epoch"`. |

Строковые значения `entrypoint` и `cmd` разбираются по правилам shell quoting:

```toml
entrypoint = "/execenv -m error --exec /taskd"
cmd = "serve --title 'Task service'"
```

Массив задаёт argv без дополнительного разбора и предпочтителен, если границы
аргументов должны быть зафиксированы явно:

```toml
entrypoint = ["/execenv", "-m", "error", "--exec", "/taskd"]
cmd = ["serve", "--title", "Task service"]
```

## Параллелизм и кэш

| Поле | Тип и значение по умолчанию | Описание и пример |
|---|---|---|
| `jobs` | Положительное целое число; число доступных потоков, но не более `4` | Максимальное число одновременно собираемых `COPY`-слоёв и передаваемых blob'ов. Пример: `jobs = 4`. |
| `cache` | Булево значение; `false` | Включает постоянный кэш скачанных слоёв базового образа. Пример: `cache = true`. Построенные `COPY`-слои в этот кэш не входят. |
| `cache-path` | Строка; `".rib-cache"` при включённом кэше | Каталог кэша, например `"/cache/rib"` или `".rib-cache"`. Само поле не включает кэш: требуется `cache = true` либо флаг `--cache`. Относительный путь используется относительно рабочего каталога процесса. |

## Параметры registry-клиента

| Поле | Тип и значение по умолчанию | Описание и пример |
|---|---|---|
| `connect-timeout` | Положительное целое число; `30` | Тайм-аут установления соединения в секундах, например `connect-timeout = 15`. |
| `read-timeout` | Положительное целое число; `60` | Тайм-аут чтения registry в секундах, например `read-timeout = 120`. |
| `max-attempts` | Положительное целое число; `3` | Максимальное число попыток каждой registry-операции, например `max-attempts = 5`. |
| `from-plain-http` | Булево значение; `false` | Использует HTTP вместо HTTPS для source-registry. Пример: `from-plain-http = true`. Допустимо только для доверенного локального или тестового registry. |
| `from-skip-tls` | Булево значение; `false` | Отключает проверку TLS-сертификата source-registry. Пример: `from-skip-tls = true`. Не следует использовать в production. |
| `to-plain-http` | Булево значение; `false` | Использует HTTP вместо HTTPS для всех registry-целей. Пример: `to-plain-http = true`. Допустимо только для доверенного локального или тестового registry. |
| `to-skip-tls` | Булево значение; `false` | Отключает проверку TLS-сертификатов для всех registry-целей. Пример: `to-skip-tls = true`. Не следует использовать в production. |

Credentials, Docker config и формат progress намеренно не поддерживаются в
metadata. Они задаются флагами `--credential`, `--docker-config` и `--progress`
либо через окружение. Секреты не должны храниться в `Cargo.toml`.

## Правила объединения metadata и CLI

- `artifact`, `platform`, `from`, `destination`, `entrypoint`, `cmd`, `workdir`,
  `user`, `creation-time`, `jobs`, `cache-path`, сетевые тайм-ауты и
  `max-attempts`, переданные через CLI, заменяют соответствующие значения
  metadata.
- Булевы значения объединяются логическим ИЛИ. Флаг CLI может включить опцию,
  но не может отключить значение `true` из metadata.
- `to`, `copies`, `labels`, `ports` и аргументы Cargo дополняются: сначала
  используются значения metadata, затем CLI.
- Artifact всегда добавляется последним слоем и не может быть перекрыт более
  ранним дополнительным `COPY`.
- Путь к artifact и source-пути `copies` из CLI разрешаются относительно
  текущего рабочего каталога, а значения из metadata — относительно каталога
  `Cargo.toml`.

Аргументы Cargo из CLI указываются после разделителя `--`:

```bash
cargo rib build -- --release --locked --bin taskd
```

Они добавляются после `cargo-args` из metadata.

## Полный пример конфигурации

Следующий пример показывает все поддерживаемые поля. Значения следует
адаптировать к структуре проекта и используемым registry:

```toml
[package]
name = "taskd"
version = "0.1.0"
edition = "2021"
rust-version = "1.94"

[package.metadata.rib]
artifact = "target/x86_64-unknown-linux-musl/release/taskd"
cargo-args = [
  "--release",
  "--locked",
  "--target", "x86_64-unknown-linux-musl",
  "--bin", "taskd",
]
destination = "/taskd"
copies = [
  "config/default.toml:/etc/taskd/config.toml,mode=0644,chown=65532:65532",
  "assets:/opt/taskd/assets/",
]

platform = "linux/amd64"
from = "scratch"
to = [
  "registry:registry.example.com/team/taskd:v1",
  "oci-archive:dist/taskd.tar@taskd:v1",
]

entrypoint = ["/taskd"]
cmd = ["serve", "--port", "8080"]
labels = [
  "org.opencontainers.image.title=taskd",
  "org.opencontainers.image.version=0.1.0",
]
ports = ["8080/tcp"]
workdir = "/"
user = "65532:65532"
keep-cmd = false
creation-time = "epoch"

jobs = 4
cache = true
cache-path = ".rib-cache"

connect-timeout = 30
read-timeout = 60
max-attempts = 3
from-plain-http = false
from-skip-tls = false
to-plain-http = false
to-skip-tls = false
```

При такой конфигурации команда не требует `--to`:

```bash
cargo rib build
```

Для registry, требующего аутентификации, credentials передаются отдельно:

```bash
cargo rib build \
  --credential "registry.example.com=${REGISTRY_USER}:${REGISTRY_PASSWORD}"
```

Если тег должен формироваться из переменных CI, `to` можно не указывать в
metadata и передать динамическую цель через CLI:

```bash
cargo rib build \
  --credential "$CI_REGISTRY=$CI_REGISTRY_USER:$CI_REGISTRY_PASSWORD" \
  --to "registry:$CI_REGISTRY_IMAGE/taskd:$CI_COMMIT_SHORT_SHA"
```

Это один из вариантов конфигурации, а не ограничение Cargo-интеграции.

## Настройка без metadata

Конфигурацию можно полностью передать флагами:

```bash
cargo rib build \
  --artifact target/x86_64-unknown-linux-musl/release/taskd \
  --platform linux/amd64 \
  --from scratch \
  --to registry:registry.example.com/team/taskd:v1 \
  --destination /taskd \
  --copy config/default.toml:/etc/taskd/config.toml \
  --entrypoint /taskd \
  -- \
  --release --locked --target x86_64-unknown-linux-musl --bin taskd
```

## Выбор пакета workspace

Если текущий каталог находится внутри каталога Cargo package, выбирается этот
package. Из корня virtual workspace пакет выбирается только при одном
`workspace.default-member` либо единственном package во всём workspace.

При неоднозначности команда завершается ошибкой. Для выбора требуемого package
необходимо запустить `cargo rib build` из его каталога.

## Ограничения интеграции

`cargo rib build` не выполняет следующие операции:

- определение архитектуры artifact;
- проверка статической или динамической компоновки;
- добавление динамического loader и shared libraries;
- установка Rust target;
- cross-compilation;
- автоматический поиск сертификатов, конфигурации и runtime-файлов.

При использовании `from = "scratch"` artifact должен работать без файлов
базового образа. Все дополнительные зависимости необходимо перечислить в
`copies` либо включить в сам artifact.
