## Summary

- 

## Validation

- [ ] `cargo xtask dev-plan` used to select the validation lanes
- [ ] `.codex/hooks/selftest.sh`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo xtask check`
- [ ] Host tests from `.github/workflows/rust.yml` when relevant
- [ ] `mdbook build` when docs changed

For physical GPU changes, classify visual, throughput, lifecycle, and recovery
evidence separately using `docs/ai/physical-gpu-status.md`; do not convert a
deferred gate into a pass.

## Notes

- 
