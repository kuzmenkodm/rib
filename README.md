# [cargo-]rib - Rust Image Builder

`rib` — rootless-сборщик OCI-образов для упаковки готовых артефактов. Он
формирует слои обычными файловыми операциями, не требует Docker daemon или
расширенных привилегий, переиспользует данные базового образа непосредственно
в registry и выводит итоговый manifest digest в stdout для последующей подписи
или развёртывания.

`rib` is a rootless OCI image builder designed for packaging ready-made
artifacts. It constructs layers using standard file operations, requires
neither a Docker daemon nor elevated privileges, reuses base image data
directly from the registry, and writes the final manifest digest to stdout for
subsequent signing or deployment.

- [Документация на русском языке](https://kuzmenkodm.github.io/rib/ru/)
- [Documentation in English](https://kuzmenkodm.github.io/rib/en/)
