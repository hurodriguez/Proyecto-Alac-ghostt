
<h1 align="center">Alac</h1>

<p align="center">
  Un fork de <a href="https://github.com/alacritty/alacritty">Alacritty</a> para macOS con motor de terminal
  <a href="https://github.com/ghostty-org/ghostty">libghostty-vt</a>, pestañas propias y explorador de archivos lateral.
</p>

---

## Acerca de

**Alac** es un fork modificado de Alacritty (base upstream `0.18.0-dev`, Rust edition 2024, MSRV 1.85)
enfocado en macOS. Mantiene el renderizado OpenGL rápido de Alacritty y la configuración por TOML,
pero reemplaza el parser/motor de terminal por **libghostty-vt** y agrega elementos de interfaz propios.

> **Estado:** experimental. libghostty-vt todavía es alpha y no tiene release etiquetado.

## Características

- **Motor libghostty-vt**: el stream del PTY lo procesa el motor de Ghostty, que gestiona pantalla,
  scrollback, modos, colores y cursor. El resultado se refleja en el `Grid` de Alacritty, de modo que
  selección, búsqueda y modo vi siguen funcionando.
- **Barra de pestañas propia**: reemplaza las pestañas nativas de macOS, con renombrado de pestañas.
- **Explorador de archivos lateral**: panel que muestra el directorio actual (cwd) de la terminal.
- **Menú nativo de macOS** (`macos/menu.rs`).
- **App renombrada a Alac** (`Alac.app`, `Alac.dmg`).
- Todo lo de Alacritty: configuración TOML con recarga en vivo, hints, modo vi, búsqueda, IPC por
  socket unix (`alacritty msg`), etc.

## Estructura del proyecto

Workspace de Cargo con 4 crates:

| Crate | Descripción |
|-------|-------------|
| `ghostty_sys` | Bindings (bindgen) a libghostty-vt. Headers y `libghostty-vt.a` vendorizados en `ghostty_sys/vendor/`. |
| `alac_terminal` | Núcleo de terminal: wrapper seguro del motor (`term/engine.rs`), grid espejo, PTY, event loop. |
| `alac` (paquete `alacritty`) | Aplicación: winit + OpenGL (glutin), display, renderer, input, config, glue macOS. |
| `alac_config` / `alac_config_derive` | Derive `ConfigDeserialize` y utilidades de configuración. |

Los módulos propios (`display/tab_bar.rs`, `display/file_explorer.rs`, `macos/*`,
`config/{explorer,tab_bar}.rs`) son independientes del motor: no usan `Term`, `Grid`, `Cell`,
selección, búsqueda ni `tty`. Ver [`CLAUDE.md`](./CLAUDE.md) para más detalle.

## Requisitos

- macOS
- [Rust](https://rustup.rs) 1.85 o superior
- Xcode Command Line Tools (incluye libclang, necesario para bindgen)
- Para empaquetar: `scdoc`, `tic` y `codesign`
- Solo si quieres recompilar libghostty-vt: Zig 0.17

## Compilar y ejecutar

```sh
cargo build              # debug
cargo run                # ejecutar la terminal
cargo build --release    # release
cargo test               # tests del workspace
cargo clippy --all-targets
cargo +nightly fmt       # rustfmt.toml usa opciones nightly
```

### Empaquetar la app

```sh
make app                 # target/release/osx/Alac.app
make dmg                 # Alac.dmg
make app-universal       # x86_64 + aarch64
```

### Recompilar libghostty-vt

Desde un checkout de Ghostty:

```sh
zig build -Demit-lib-vt=true -Doptimize=ReleaseFast
```

y copia `libghostty-vt.a` y los headers a `ghostty_sys/vendor/`.

## Configuración

Alac usa el mismo formato TOML que Alacritty (`~/.config/alacritty/alacritty.toml`), con secciones
adicionales para la barra de pestañas y el explorador. Las opciones están documentadas en
[`extra/man/alacritty.5.scd`](./extra/man/alacritty.5.scd).

## Contribuir

- Formatea con `cargo +nightly fmt` y pasa `cargo clippy --all-targets` sin warnings.
- Cambios visibles al usuario van en [`CHANGELOG.md`](./CHANGELOG.md).
- Consulta [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## Créditos

- [Alacritty](https://github.com/alacritty/alacritty): base del proyecto.
- [Ghostty](https://github.com/ghostty-org/ghostty): motor de terminal (libghostty-vt).

## Licencia

Distribuido bajo licencia [Apache 2.0](./LICENSE-APACHE) o [MIT](./LICENSE-MIT), a tu elección,
igual que Alacritty.
