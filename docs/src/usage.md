# Начало работы

## Установка

### crates.io

Для установки опубликованной версии требуется Rust 1.88 или новее:

```bash
cargo install rust-image-builder --version 0.1.0 --locked
```

Без `--version` Cargo установит последнюю доступную версию:

```bash
cargo install rust-image-builder --locked
```

Страница пакета: [rust-image-builder 0.1.0 на crates.io](https://crates.io/crates/rust-image-builder/0.1.0).

### GitHub Releases

Готовые бинарники не требуют установленного Rust. Архивы и их контрольные
суммы опубликованы в [релизе v0.1.0](https://github.com/kuzmenkodm/rib/releases/tag/v0.1.0).

Пример установки для Linux x86_64 с glibc:

```bash
version=v0.1.0
archive="rib-${version}-x86_64-unknown-linux-gnu.tar.gz"
base_url="https://github.com/kuzmenkodm/rib/releases/download/${version}"

curl -LO "${base_url}/${archive}"
curl -LO "${base_url}/${archive}.sha256"
sha256sum --check "${archive}.sha256"
tar -xzf "${archive}"
mkdir -p "$HOME/.local/bin"
install -m 0755 rib cargo-rib "$HOME/.local/bin/"
```

`$HOME/.local/bin` должен присутствовать в `PATH`. Для Windows следует
скачать ZIP-архив `rib-v0.1.0-x86_64-pc-windows-msvc.zip`, проверить файл с
суффиксом `.sha256`, распаковать `rib.exe` и `cargo-rib.exe` и добавить
каталог с ними в `PATH`.

### Из исходного кода

```bash
git clone https://github.com/kuzmenkodm/rib.git
cd rib
cargo install --path . --locked
```

Устанавливаются два бинарника:

- `rib` — standalone CLI для упаковки готовых файлов;
- `cargo-rib` — Cargo subcommand, доступный как `cargo rib build`.

Актуальный перечень параметров выводится командами:

```bash
rib build --help
cargo rib build --help
```

## Образ на основе registry image

Следующая команда добавляет готовый исполняемый файл в базовый Alpine image и
публикует результат непосредственно в registry:

```bash
IMAGE_DIGEST="$(rib build \
  --from alpine:3.22 \
  --platform linux/amd64 \
  --copy ./server:/app/server,mode=0755 \
  --entrypoint /app/server \
  --to registry:registry.example.com/team/server:v1)"
```

`rib` получает manifest и config базового образа, но не выполняет его полный
предварительный pull. Унаследованные слои переиспользуются в registry по
digest. В `IMAGE_DIGEST` записывается digest опубликованного manifest.

## Образ на основе scratch

Для статически скомпонованного бинарника можно использовать пустую основу:

```bash
rib build \
  --from scratch \
  --platform linux/amd64 \
  --copy ./server:/server,mode=0755,chown=65532:65532 \
  --entrypoint /server \
  --user 65532:65532 \
  --to oci-archive:dist/server.tar@server:v1
```

`scratch` является специальным указателем на пустую основу и должен
передаваться только в таком формате. Значение `scratch:latest` будет
интерпретировано как имя обычного образа, который `rib` попытается получить из
registry.

`scratch` не содержит динамический loader, системные библиотеки, CA
certificates или shell. Все runtime-зависимости должны быть включены в
artifact либо добавлены отдельными `--copy`.

## Формат `--copy`

```text
<source>:<destination>[,mode=<octal>][,chown=<uid>[:<gid>]]
```

Примеры:

```bash
--copy ./server:/usr/local/bin/server,mode=0755
--copy ./config:/etc/server/,chown=65532:65532
--copy 'target/release/*.so:/usr/local/lib/'
```

Каждый `--copy` создаёт отдельный слой. Порядок слоёв соответствует порядку
флагов. Синтаксис назначения похож на `COPY` в Dockerfile, но `rib` реализует
только описанное в этой документации подмножество правил. В частности,
символические ссылки не поддерживаются, а при нескольких результатах glob
назначение должно оканчиваться на `/`. Glob-шаблон следует заключать в кавычки,
чтобы его обрабатывал `rib`, а не shell.

## Цели публикации

Флаг `--to` можно указывать несколько раз:

```text
registry:<image-reference>
oci-archive:<path>[@<tag>]
docker-archive:<path>[@<tag>]
```

Пример одновременной публикации:

```bash
rib build ... \
  --to registry:registry.example.com/team/server:v1 \
  --to oci-archive:dist/server-oci.tar@server:v1 \
  --to docker-archive:dist/server-docker.tar@server:v1
```

Цели обрабатываются последовательно и не образуют общую транзакцию.

## Аутентификация

Явный credential передаётся в формате:

```bash
--credential 'registry.example.com=user:password'
```

Альтернативно `rib` читает секцию `auths` из одного Docker config. Файл
выбирается в следующем порядке: явно указанный через `--docker-config`, иначе
`$DOCKER_CONFIG/config.json`, иначе `~/.docker/config.json`. Поиск credentials
в других файлах после выбора config не выполняется. Явный `--credential` имеет
приоритет над выбранным Docker config.
