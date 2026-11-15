# GNU bc — local import

External Linux calculator binary (`bc` 1.07.1-3ubuntu4 amd64) imported directly
from the pinned Ubuntu Noble archive without modifying the binary.

```sh
python3 tools/agent/import-bc.py
cargo xtask build
```

The importer verifies the pinned SHA-256 against the Noble archive cache (or
downloads it from the pinned Ubuntu Noble snapshot if absent), extracts only
`usr/bin/bc` into the gitignored `apps/bc/bundle/`, and writes the gitignored
`apps/bc/RUSTOS.package.toml`.
