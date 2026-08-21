# cmdshim

`cmdshim` turns command declarations in `mise.toml` into real project-local executables on `PATH`.

## Configure

```toml
[tools]
"github:Unreal-Works/cmdshim" = "latest"

[env]
_.path = ["{{ exec(command='cmdshim path') }}"]

[_.cmdshim.acme]
run = ["cargo", "run", "--quiet", "--manifest-path", "{{config_root}}/Cargo.toml", "--"]

[_.cmdshim.codegen]
run = ["pnpm", "exec", "tsx", "{{config_root}}/scripts/codegen.ts"]

[_.cmdshim.server]
run = ["python", "-m", "myapp.server"]
cwd = "{{config_root}}/backend"
env = { PYTHONUNBUFFERED = "1" }
```

After mise activates the environment, these become ordinary commands:

```sh
acme foo --bar
codegen
server
```

Arguments supplied to the shim are appended to `run`.

## Commands

```text
cmdshim path [--config path/to/mise.toml]
cmdshim exec [--config path/to/mise.toml] <name> [--] [args...]
```

`cmdshim path` searches upward for `mise.toml` or `.mise.toml`, materializes wrappers in the cache, and prints only the shim directory to stdout.

Generated wrappers dispatch back to the exact `cmdshim` executable that generated them, passing the absolute config path. Command execution therefore does not depend on the caller's current directory or a later `PATH` lookup for `cmdshim`.

## Cache location

Resolution order:

1. `CMDSHIM_CACHE_DIR`
2. `%LOCALAPPDATA%\\cmdshim` on Windows
3. `$XDG_CACHE_HOME/cmdshim`
4. `$HOME/.cache/cmdshim`

Each configuration file gets a stable hashed directory. Wrappers are regenerated when the config contents, cmdshim version, or cmdshim executable path changes.

## Configuration

Each `[_.cmdshim.<name>]` table supports:

- `run` — required array of argv strings. No shell parsing is performed.
- `cwd` — optional working directory. Relative values resolve from the directory containing the config file; the default is that directory.
- `env` — optional environment variables.

The literal token `{{config_root}}` is expanded in `run`, `cwd`, and `env` values.

## Notes

`[_]` is mise's reserved namespace for data mise itself does not parse, which keeps these declarations out of mise's config schema. For compatibility, cmdshim also accepts top-level `[cmdshim.<name>]`, but `[_.cmdshim.<name>]` is recommended.

Shim names are portable executable names: ASCII letters/digits plus `.`, `_`, and `-`. The name `cmdshim` is reserved.

This intentionally is not a task runner. It only provides real executables backed by declarative command definitions.
