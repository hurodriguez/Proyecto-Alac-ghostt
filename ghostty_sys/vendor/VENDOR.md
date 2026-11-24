# Vendored libghostty-vt

`lib/libghostty-vt.a` and `include/ghostty/` are prebuilt artifacts of
[Ghostty](https://github.com/ghostty-org/ghostty)'s `libghostty-vt`. This binary parses all PTY
output, so keep its identity verifiable.

| Field | Value |
|-------|-------|
| File | `lib/libghostty-vt.a` |
| Size | 12655760 bytes |
| SHA-256 | `1f09ef66fd7b6c9bb5b4913a96d2edccac81a157497676d8540062697d5a6ef1` |
| Ghostty commit | TODO: fill in the exact commit the binary was built from |
| Zig version | 0.17 |
| Build command | `zig build -Demit-lib-vt=true -Doptimize=ReleaseFast` (from a Ghostty checkout) |

## Update procedure

1. Build from a Ghostty checkout at a known commit with the command above.
2. Copy `libghostty-vt.a` and the headers into this directory.
3. Update the table above (commit, size, SHA-256) in the same change.

## Verify

```sh
shasum -a 256 ghostty_sys/vendor/lib/libghostty-vt.a
```
